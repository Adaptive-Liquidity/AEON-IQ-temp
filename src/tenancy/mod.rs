//! Tenant-scoped agent identity.
//!
//! Step 1 of the AEON authentication & tenancy plan (§10 step 1), accepted at
//! `Adaptive-Liquidity/nexus-planning @ b1fe06505d400e435c3ef8d10dc197f15641bebd`.
//!
//! This module owns the *operator-facing* half of migration `0028`: selecting a
//! legacy migration mode, backfilling `agents.tenant_id` accordingly, and
//! refusing to enable multi-tenant mode while any precondition is unmet.
//!
//! Three properties are load-bearing and are each covered by a test:
//!
//! * **No implicit tenant.**  An unset `LEGACY_MIGRATION_MODE` assigns nothing.
//!   There is no default tenant, and none is invented for unmapped rows.
//! * **Unmapped means unreachable.**  An agent with no tenant assignment keeps
//!   `tenant_id IS NULL`, which no `WHERE tenant_id = $1` predicate can ever
//!   match, so it is served to nobody — not to a legacy tenant, not to an
//!   admin, not to a default.
//! * **Enablement fails closed.**  `assert_multi_tenant_preconditions` refuses
//!   startup when the mode is absent, when the recorded migration disagrees
//!   with the configured one, while any row is unmapped, while the management
//!   API is unauthenticated, or while the legacy `MANAGEMENT_API_KEY` is still
//!   accepted (plan §6).
//!
//! Nothing here changes V1 behaviour: with no tenancy environment variables
//! set, `TenancySettings::from_env` yields no plan, no backfill runs, and the
//! enablement gate returns immediately.

// Step 4A: the read-only tenancy inventory and mismatch report. Diagnostic
// only — it adds no route, alters no row, and activates nothing.
pub mod audit;
#[cfg(test)]
mod audit_db_tests;
// Step 4B-2: the BACKFILL stage's engine. Library-first deliberately — a CLI
// subcommand drives it today and a management endpoint will drive it later, and
// the invariants (batching, cursor persistence, reconciliation, the refusal to
// complete over a dirty audit) belong to the behaviour rather than to either
// front door.
pub mod backfill;
#[cfg(test)]
mod backfill_db_tests;
pub mod inventory;
// Step 4B-0: the typed migration contract. Planning data only — it creates no
// column, no index, no constraint and no migration file.
pub mod plan;
pub mod report;
// Step 4B-1: the PREPARE stage's live behaviour. Separate from
// `audit_db_tests` because it tests migrations rather than the audit, and
// because the no-database CI path skips `tenancy::*_db_tests` by name.
#[cfg(test)]
mod tranche1_db_tests;

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

/// Serialises concurrent backfills so two starting replicas cannot interleave
/// their `UPDATE`s and record contradictory migration rows.
const BACKFILL_ADVISORY_LOCK_ID: i64 = 0x4145_4F4E_5431; // "AEONT1"

// ── Operator configuration ───────────────────────────────────────────────────

/// The two frozen legacy migration modes (plan §6, decision A-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyMigrationMode {
    /// Every historical row maps to one operator-declared `LEGACY_TENANT_ID`.
    SingleTenant,
    /// The operator supplies explicit mappings; unmapped rows stay unmapped.
    Mapped,
}

impl LegacyMigrationMode {
    /// Wire form, shared by `LEGACY_MIGRATION_MODE` and the
    /// `agent_tenancy_migrations.mode` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SingleTenant => "single_tenant",
            Self::Mapped => "mapped",
        }
    }
}

/// A validated migration plan.  Constructing one requires the operator to have
/// supplied everything the selected mode needs, so a half-specified mode cannot
/// reach the backfill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyTenancyPlan {
    SingleTenant {
        tenant_id: Uuid,
    },
    Mapped {
        /// `external_agent_id` → owning tenant.  Ordered so error messages and
        /// the generated SQL parameters are deterministic.
        mappings: BTreeMap<String, Uuid>,
    },
}

impl LegacyTenancyPlan {
    pub fn mode(&self) -> LegacyMigrationMode {
        match self {
            Self::SingleTenant { .. } => LegacyMigrationMode::SingleTenant,
            Self::Mapped { .. } => LegacyMigrationMode::Mapped,
        }
    }

    /// The declared tenant, which exists only in single-tenant mode.  Mode 2
    /// has no single legacy tenant to name — that asymmetry is enforced by
    /// `agent_tenancy_migrations_mode_tenant_ck` in migration 0028 as well.
    pub fn legacy_tenant_id(&self) -> Option<Uuid> {
        match self {
            Self::SingleTenant { tenant_id } => Some(*tenant_id),
            Self::Mapped { .. } => None,
        }
    }
}

/// Raw operator inputs, before validation.
///
/// Split out from [`TenancySettings::from_env`] so every parsing rule is
/// testable without mutating the process environment — the same shape as
/// `ProcessRole::from_env_value`, and necessary here because Rust runs tests in
/// threads that share one environment.
#[derive(Debug, Clone, Default)]
pub struct TenancyEnv {
    pub mode: Option<String>,
    pub legacy_tenant_id: Option<String>,
    /// Contents (not path) of the tenant map, as JSON.
    pub tenant_map_json: Option<String>,
    pub multi_tenant_enabled: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TenancySettings {
    /// `None` means no mode was selected, so no backfill runs at all.
    pub legacy_plan: Option<LegacyTenancyPlan>,
    pub multi_tenant_enabled: bool,
}

impl TenancySettings {
    pub fn from_env() -> Result<Self> {
        let inline = non_empty(std::env::var("LEGACY_TENANT_MAP").ok());
        let path = non_empty(std::env::var("LEGACY_TENANT_MAP_FILE").ok());

        let tenant_map_json = match (inline, path) {
            (Some(_), Some(_)) => bail!(
                "LEGACY_TENANT_MAP and LEGACY_TENANT_MAP_FILE are both set; \
                 supply exactly one so the effective mapping is unambiguous"
            ),
            (Some(json), None) => Some(json),
            (None, Some(path)) => Some(
                std::fs::read_to_string(&path)
                    .with_context(|| format!("LEGACY_TENANT_MAP_FILE could not be read: {path}"))?,
            ),
            (None, None) => None,
        };

        Self::from_values(TenancyEnv {
            mode: non_empty(std::env::var("LEGACY_MIGRATION_MODE").ok()),
            legacy_tenant_id: non_empty(std::env::var("LEGACY_TENANT_ID").ok()),
            tenant_map_json,
            multi_tenant_enabled: non_empty(std::env::var("MULTI_TENANT_ENABLED").ok()),
        })
    }

    pub fn from_values(env: TenancyEnv) -> Result<Self> {
        let multi_tenant_enabled = env
            .multi_tenant_enabled
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));

        let mode = non_empty(env.mode);
        let legacy_tenant_id = non_empty(env.legacy_tenant_id);
        let tenant_map_json = non_empty(env.tenant_map_json);

        let legacy_plan = match mode.as_deref() {
            // No mode selected: nothing is migrated and nothing is assigned.
            // This is not an error on its own — it becomes one only when
            // multi-tenant mode is enabled (§6, and the gate below).
            None => None,

            Some(m) if m.eq_ignore_ascii_case("single_tenant") => {
                if tenant_map_json.is_some() {
                    bail!(
                        "LEGACY_MIGRATION_MODE=single_tenant does not use a tenant map; \
                         unset LEGACY_TENANT_MAP / LEGACY_TENANT_MAP_FILE or select \
                         LEGACY_MIGRATION_MODE=mapped"
                    );
                }
                let raw = legacy_tenant_id.context(
                    "LEGACY_MIGRATION_MODE=single_tenant requires LEGACY_TENANT_ID; \
                     there is no default tenant and none will be generated",
                )?;
                let tenant_id = Uuid::parse_str(&raw)
                    .context("LEGACY_TENANT_ID must be a UUID identifying the legacy tenant")?;
                Some(LegacyTenancyPlan::SingleTenant { tenant_id })
            }

            Some(m) if m.eq_ignore_ascii_case("mapped") => {
                if legacy_tenant_id.is_some() {
                    bail!(
                        "LEGACY_MIGRATION_MODE=mapped has no single legacy tenant; \
                         unset LEGACY_TENANT_ID or select \
                         LEGACY_MIGRATION_MODE=single_tenant"
                    );
                }
                let raw = tenant_map_json.context(
                    "LEGACY_MIGRATION_MODE=mapped requires LEGACY_TENANT_MAP or \
                     LEGACY_TENANT_MAP_FILE",
                )?;
                Some(LegacyTenancyPlan::Mapped {
                    mappings: parse_tenant_map(&raw)?,
                })
            }

            Some(other) => bail!(
                "LEGACY_MIGRATION_MODE must be one of single_tenant, mapped; got {other:?}. \
                 There is no default mode."
            ),
        };

        Ok(Self {
            legacy_plan,
            multi_tenant_enabled,
        })
    }
}

/// `{"<external_agent_id>": "<tenant-uuid>", ...}`.
fn parse_tenant_map(raw: &str) -> Result<BTreeMap<String, Uuid>> {
    let decoded: BTreeMap<String, String> = serde_json::from_str(raw)
        .context("tenant map must be a JSON object of {\"external_agent_id\": \"tenant-uuid\"}")?;

    if decoded.is_empty() {
        bail!(
            "tenant map is empty; supply at least one mapping or leave \
             LEGACY_MIGRATION_MODE unset"
        );
    }

    let mut mappings = BTreeMap::new();
    for (external_agent_id, tenant) in decoded {
        if external_agent_id.trim().is_empty() {
            bail!("tenant map contains a blank external_agent_id key");
        }
        let tenant_id = Uuid::parse_str(tenant.trim()).with_context(|| {
            format!("tenant map value for {external_agent_id:?} is not a UUID: {tenant:?}")
        })?;
        mappings.insert(external_agent_id, tenant_id);
    }
    Ok(mappings)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

// ── Backfill ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillReport {
    pub mode: LegacyMigrationMode,
    pub legacy_tenant_id: Option<Uuid>,
    pub agents_total: i64,
    /// Rows this run moved from unmapped to a tenant.
    pub agents_assigned: i64,
    /// Rows still unmapped after this run.  Non-zero blocks multi-tenant mode.
    pub agents_unmapped: i64,
    /// A prior migration record already existed, i.e. this was a re-run.
    pub already_applied: bool,
}

/// Assign tenants to historical agents according to an explicitly selected mode.
///
/// Idempotent: only rows with `tenant_id IS NULL` are ever written, so a second
/// run with the same plan is a no-op. A run whose mode or declared tenant
/// disagrees with the recorded migration is rejected rather than silently
/// re-assigning rows that a previous mode already placed.
pub async fn run_legacy_backfill(
    pool: &PgPool,
    plan: &LegacyTenancyPlan,
) -> Result<BackfillReport> {
    let mut tx = pool.begin().await?;

    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(BACKFILL_ADVISORY_LOCK_ID)
        .execute(&mut *tx)
        .await?;

    let prior: Option<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT mode, legacy_tenant_id FROM agent_tenancy_migrations \
         ORDER BY applied_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((recorded_mode, recorded_tenant)) = prior.as_ref() {
        if recorded_mode.as_str() != plan.mode().as_str()
            || *recorded_tenant != plan.legacy_tenant_id()
        {
            bail!(
                "agent tenancy was already migrated in mode {recorded_mode:?} \
                 (legacy tenant {recorded_tenant:?}); re-running in mode {:?} \
                 (legacy tenant {:?}) would reassign rows a previous mode placed. \
                 Roll back with rollback/0028_agent_tenancy_identity_down.sql if the \
                 earlier migration was wrong.",
                plan.mode().as_str(),
                plan.legacy_tenant_id(),
            );
        }
    }

    let agents_assigned = match plan {
        LegacyTenancyPlan::SingleTenant { tenant_id } => {
            sqlx::query("UPDATE agents SET tenant_id = $1 WHERE tenant_id IS NULL")
                .bind(tenant_id)
                .execute(&mut *tx)
                .await?
                .rows_affected() as i64
        }

        LegacyTenancyPlan::Mapped { mappings } => {
            let external_ids: Vec<String> = mappings.keys().cloned().collect();
            let tenant_ids: Vec<Uuid> = mappings.values().copied().collect();

            // A row already sitting in a different tenant is a contradiction
            // between this map and reality: overwriting it would silently move
            // an agent between tenants.  Report and refuse instead.
            let conflicts: Vec<(String, Uuid, Uuid)> = sqlx::query_as(
                "SELECT a.external_agent_id, a.tenant_id, m.tenant_id \
                   FROM agents a \
                   JOIN unnest($1::text[], $2::uuid[]) AS m(external_agent_id, tenant_id) \
                     ON a.external_agent_id = m.external_agent_id \
                  WHERE a.tenant_id IS NOT NULL AND a.tenant_id <> m.tenant_id \
                  ORDER BY a.external_agent_id",
            )
            .bind(&external_ids)
            .bind(&tenant_ids)
            .fetch_all(&mut *tx)
            .await?;

            if !conflicts.is_empty() {
                let detail = conflicts
                    .iter()
                    .map(|(external, current, requested)| {
                        format!("{external:?}: currently {current}, map says {requested}")
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                bail!(
                    "tenant map contradicts {} already-assigned agent(s): {detail}",
                    conflicts.len()
                );
            }

            sqlx::query(
                "UPDATE agents AS a SET tenant_id = m.tenant_id \
                   FROM unnest($1::text[], $2::uuid[]) AS m(external_agent_id, tenant_id) \
                  WHERE a.external_agent_id = m.external_agent_id \
                    AND a.tenant_id IS NULL",
            )
            .bind(&external_ids)
            .bind(&tenant_ids)
            .execute(&mut *tx)
            .await?
            .rows_affected() as i64
        }
    };

    let (agents_total, agents_unmapped): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*)::bigint, \
                COUNT(*) FILTER (WHERE tenant_id IS NULL)::bigint \
           FROM agents",
    )
    .fetch_one(&mut *tx)
    .await?;

    // Record the first run, and any later run that actually assigned something.
    // A pure no-op re-run adds no row, so the ledger stays a history of real
    // assignments rather than of process restarts.
    if prior.is_none() || agents_assigned > 0 {
        sqlx::query(
            "INSERT INTO agent_tenancy_migrations \
                 (mode, legacy_tenant_id, agents_total, agents_assigned, agents_unmapped) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(plan.mode().as_str())
        .bind(plan.legacy_tenant_id())
        .bind(agents_total)
        .bind(agents_assigned)
        .bind(agents_unmapped)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    Ok(BackfillReport {
        mode: plan.mode(),
        legacy_tenant_id: plan.legacy_tenant_id(),
        agents_total,
        agents_assigned,
        agents_unmapped,
        already_applied: prior.is_some(),
    })
}

// ── Enablement gate ──────────────────────────────────────────────────────────

/// Agents that no tenant can reach.
pub async fn count_unmapped_agents(pool: &PgPool) -> Result<i64> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*)::bigint FROM agents WHERE tenant_id IS NULL")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// What the management API accepts at startup.
///
/// Both fields matter to the gate, and neither implies the other. "No legacy
/// key" is retirement only when something authenticates in its place; on its
/// own it can equally mean nothing authenticates at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagementAuth {
    /// `MANAGEMENT_API_KEY` is set, so the legacy key still authorises every
    /// route with no tenant of its own.
    pub legacy_key_accepted: bool,
    /// No key is set and `ALLOW_UNAUTH_MANAGEMENT=true`, so
    /// `auth::check_management_key` performs no check at all (`auth.rs:22` —
    /// the whole comparison is inside `if let Some(required)`).
    pub unauthenticated: bool,
}

/// Refuse to enable multi-tenant mode unless every §6 precondition holds.
///
/// Returns `Ok(())` immediately when multi-tenant mode is not enabled, which is
/// the V1 default — so this gate is inert until an operator deliberately turns
/// multi-tenancy on.
pub async fn assert_multi_tenant_preconditions(
    pool: &PgPool,
    settings: &TenancySettings,
    management_auth: ManagementAuth,
) -> Result<()> {
    if !settings.multi_tenant_enabled {
        return Ok(());
    }

    let plan = settings.legacy_plan.as_ref().context(
        "MULTI_TENANT_ENABLED=true but LEGACY_MIGRATION_MODE is not set. \
         Select single_tenant or mapped; there is no default mode and historical \
         rows are never assigned a tenant implicitly.",
    )?;

    let recorded: Option<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT mode, legacy_tenant_id FROM agent_tenancy_migrations \
         ORDER BY applied_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    let (recorded_mode, recorded_tenant) = recorded.context(
        "MULTI_TENANT_ENABLED=true but no legacy tenancy migration has been recorded. \
         Run the backfill before enabling multi-tenant mode.",
    )?;

    if recorded_mode.as_str() != plan.mode().as_str() || recorded_tenant != plan.legacy_tenant_id()
    {
        bail!(
            "recorded legacy migration (mode {recorded_mode:?}, tenant {recorded_tenant:?}) \
             does not match the configured mode {:?} (tenant {:?}); refusing to enable \
             multi-tenant mode against a migration it did not run.",
            plan.mode().as_str(),
            plan.legacy_tenant_id(),
        );
    }

    let unmapped = count_unmapped_agents(pool).await?;
    if unmapped > 0 {
        bail!(
            "{unmapped} agent(s) have no tenant assignment. Unmapped agents are served \
             to nobody, and multi-tenant mode cannot be enabled while any remain. \
             Extend the tenant map and re-run the backfill."
        );
    }

    // Checked before the legacy-key condition because it is the worse state of
    // the two: an absent MANAGEMENT_API_KEY satisfies "the legacy key is no
    // longer accepted" while leaving every /api/v1/* route open to anyone.
    // Treating that as retirement would let this gate bless a multi-tenant
    // deployment with no authentication whatsoever.
    if management_auth.unauthenticated {
        bail!(
            "MANAGEMENT_API_KEY is unset and ALLOW_UNAUTH_MANAGEMENT=true, so every \
             /api/v1/* route is unauthenticated. Multi-tenant mode must not be enabled \
             without authentication: removing the legacy key counts as retirement only \
             once a replacement exists, and the credential registry is plan §10 steps \
             2-3, which have not landed."
        );
    }

    if management_auth.legacy_key_accepted {
        bail!(
            "MANAGEMENT_API_KEY is still accepted. Legacy-key retirement (plan §10 step 6) \
             must complete before multi-tenant mode is enabled, because the legacy key \
             authorises every route with no tenant of its own."
        );
    }

    Ok(())
}

// ── Tenant-scoped agent identity ─────────────────────────────────────────────
//
// The three helpers below are the foundation later steps build on.  They are
// not reachable from V1 request handling by design: this PR does not change
// endpoint authorization (plan §10 step 5 does).  They are exercised by the
// tests in this module, which is what proves the identity model works before
// anything depends on it.

/// The legacy compatibility key for a tenant-scoped agent: its own UUID.
///
/// `agents.agent_id` stays globally `UNIQUE` until §10 step 7, because
/// `sessions.agent_id` and `archival_batches.agent_id` still reference it and
/// PostgreSQL requires a unique target.  Two tenants may now share an
/// `external_agent_id`, which that global constraint cannot represent, so rows
/// created through the tenant-aware path need some other value there.
///
/// It is the agent's UUID rather than anything derived from
/// `external_agent_id`.  `agents.agent_id` is arbitrary text, so a scheme that
/// embeds the caller-supplied identifier can always be made to collide: a
/// legacy V1 agent may already be named exactly the string such a scheme would
/// generate, and an otherwise-valid `(tenant_id, external_agent_id)` pair would
/// then be rejected by `agents_agent_id_key`.  Deriving the key from the row's
/// own UUID puts it outside caller control entirely.
///
/// Rows written through the unchanged V1 path keep their identifier verbatim —
/// this applies only to rows V1 could not have created.  The whole mechanism
/// disappears at §10 step 7 along with the column it feeds; the rollback script
/// restores `agent_id` from `external_agent_id` before dropping it.
#[allow(dead_code)] // wired into request handling in plan §10 step 5
pub fn legacy_compat_agent_id(agent_uuid: Uuid) -> String {
    agent_uuid.to_string()
}

/// Create a tenant-scoped agent and return its internal UUID.
#[allow(dead_code)] // wired into request handling in plan §10 step 5
pub async fn insert_agent(pool: &PgPool, tenant_id: Uuid, external_agent_id: &str) -> Result<Uuid> {
    // Generated here rather than left to the column default so the
    // compatibility key can be derived from it in the same statement.
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agents (id, agent_id, tenant_id, external_agent_id) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(legacy_compat_agent_id(id))
    .bind(tenant_id)
    .bind(external_agent_id)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Resolve a caller-supplied `external_agent_id` **within one tenant**.
///
/// This is the canonical resolution the plan's §5.1 global rule 2 requires:
/// a miss is indistinguishable from "belongs to another tenant", and an
/// unmapped agent (`tenant_id IS NULL`) matches no tenant at all.
#[allow(dead_code)] // wired into request handling in plan §10 step 5
pub async fn resolve_agent_uuid(
    pool: &PgPool,
    tenant_id: Uuid,
    external_agent_id: &str,
) -> Result<Option<Uuid>> {
    let row: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM agents WHERE tenant_id = $1 AND external_agent_id = $2")
            .bind(tenant_id)
            .bind(external_agent_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| r.0))
}

// ─────────────────────────────────────────────────────────────────────────────
// Configuration tests (no database)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const TENANT_A: &str = "11111111-1111-1111-1111-111111111111";
    const TENANT_B: &str = "22222222-2222-2222-2222-222222222222";

    fn env(mode: Option<&str>, tenant: Option<&str>, map: Option<&str>) -> TenancyEnv {
        TenancyEnv {
            mode: mode.map(str::to_string),
            legacy_tenant_id: tenant.map(str::to_string),
            tenant_map_json: map.map(str::to_string),
            multi_tenant_enabled: None,
        }
    }

    #[test]
    fn absent_mode_selects_no_plan_and_is_not_an_error() {
        let settings = TenancySettings::from_values(TenancyEnv::default()).unwrap();

        assert!(settings.legacy_plan.is_none());
        assert!(!settings.multi_tenant_enabled);
    }

    #[test]
    fn blank_mode_is_treated_as_absent() {
        let settings = TenancySettings::from_values(env(Some("   "), None, None)).unwrap();

        assert!(settings.legacy_plan.is_none());
    }

    #[test]
    fn unknown_mode_is_rejected() {
        let error = TenancySettings::from_values(env(Some("default"), None, None)).unwrap_err();

        assert!(
            error.to_string().contains("LEGACY_MIGRATION_MODE"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn single_tenant_mode_without_legacy_tenant_id_fails_closed() {
        let error =
            TenancySettings::from_values(env(Some("single_tenant"), None, None)).unwrap_err();

        assert!(
            error.to_string().contains("LEGACY_TENANT_ID"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn single_tenant_mode_rejects_a_malformed_legacy_tenant_id() {
        let error =
            TenancySettings::from_values(env(Some("single_tenant"), Some("tenant-one"), None))
                .unwrap_err();

        assert!(
            format!("{error:#}").contains("LEGACY_TENANT_ID"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn single_tenant_mode_rejects_a_tenant_map() {
        let map = format!(r#"{{"assistant": "{TENANT_A}"}}"#);
        let error =
            TenancySettings::from_values(env(Some("single_tenant"), Some(TENANT_A), Some(&map)))
                .unwrap_err();

        assert!(
            error.to_string().contains("does not use a tenant map"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn single_tenant_mode_parses_the_declared_tenant() {
        let settings =
            TenancySettings::from_values(env(Some("SINGLE_TENANT"), Some(TENANT_A), None)).unwrap();

        assert_eq!(
            settings.legacy_plan,
            Some(LegacyTenancyPlan::SingleTenant {
                tenant_id: Uuid::parse_str(TENANT_A).unwrap()
            })
        );
    }

    #[test]
    fn mapped_mode_without_a_map_fails_closed() {
        let error = TenancySettings::from_values(env(Some("mapped"), None, None)).unwrap_err();

        assert!(
            error.to_string().contains("LEGACY_TENANT_MAP"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn mapped_mode_rejects_a_single_legacy_tenant() {
        let map = format!(r#"{{"assistant": "{TENANT_A}"}}"#);
        let error = TenancySettings::from_values(env(Some("mapped"), Some(TENANT_A), Some(&map)))
            .unwrap_err();

        assert!(
            error.to_string().contains("no single legacy tenant"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn mapped_mode_rejects_an_empty_map() {
        let error =
            TenancySettings::from_values(env(Some("mapped"), None, Some("{}"))).unwrap_err();

        assert!(
            format!("{error:#}").contains("empty"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn mapped_mode_rejects_a_non_uuid_tenant() {
        let error =
            TenancySettings::from_values(env(Some("mapped"), None, Some(r#"{"a": "tenant-a"}"#)))
                .unwrap_err();

        assert!(
            format!("{error:#}").contains("not a UUID"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn mapped_mode_parses_every_mapping() {
        let map = format!(r#"{{"alpha": "{TENANT_A}", "beta": "{TENANT_B}"}}"#);
        let settings = TenancySettings::from_values(env(Some("mapped"), None, Some(&map))).unwrap();

        let Some(LegacyTenancyPlan::Mapped { mappings }) = settings.legacy_plan else {
            panic!("expected a mapped plan");
        };
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings["alpha"], Uuid::parse_str(TENANT_A).unwrap());
        assert_eq!(mappings["beta"], Uuid::parse_str(TENANT_B).unwrap());
    }

    #[test]
    fn multi_tenant_flag_defaults_to_disabled_and_only_true_enables_it() {
        let disabled = TenancySettings::from_values(TenancyEnv::default()).unwrap();
        assert!(!disabled.multi_tenant_enabled);

        let bogus = TenancySettings::from_values(TenancyEnv {
            multi_tenant_enabled: Some("yes".into()),
            ..TenancyEnv::default()
        })
        .unwrap();
        assert!(!bogus.multi_tenant_enabled);

        let enabled = TenancySettings::from_values(TenancyEnv {
            multi_tenant_enabled: Some("TRUE".into()),
            ..TenancyEnv::default()
        })
        .unwrap();
        assert!(enabled.multi_tenant_enabled);
    }

    #[test]
    fn legacy_compat_key_is_the_agent_uuid_and_owes_nothing_to_caller_input() {
        let agent_uuid = Uuid::parse_str("cccccccc-0000-4000-8000-000000000003").unwrap();

        assert_eq!(legacy_compat_agent_id(agent_uuid), agent_uuid.to_string());
        assert_ne!(
            legacy_compat_agent_id(agent_uuid),
            legacy_compat_agent_id(Uuid::parse_str(TENANT_A).unwrap())
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Schema and migration tests (require Postgres)
// ─────────────────────────────────────────────────────────────────────────────
//
// `#[sqlx::test]` creates an isolated database per test and applies every
// migration, so each test starts from the exact schema a deployment gets.
// Requirements are the same as the store tests (see src/memory/store.rs): a
// pgvector-enabled Postgres reachable via DATABASE_URL.  The no-database CI job
// skips this module by name (.github/workflows/ci.yml).

#[cfg(test)]
mod db_tests {
    use super::*;

    const MIGRATION_SQL: &str = include_str!("../../migrations/0028_agent_tenancy_identity.sql");
    const ROLLBACK_SQL: &str = include_str!("../../rollback/0028_agent_tenancy_identity_down.sql");
    const GRANTS_ROLLBACK_SQL: &str =
        include_str!("../../rollback/0030_credential_agent_grants_down.sql");
    const GRANTS_HARDENING_ROLLBACK_SQL: &str =
        include_str!("../../rollback/0031_credential_agent_grants_hardening_down.sql");
    const TRANCHE1_CONSTRAINTS_ROLLBACK_SQL: &str =
        include_str!("../../rollback/0041_tenancy_tranche1_constraints_down.sql");
    const TRANCHE1_INDEXES_ROLLBACK_SQL: &str =
        include_str!("../../rollback/0033_tenancy_tranche1_indexes_down.sql");
    const TRANCHE1_PREPARE_ROLLBACK_SQL: &str =
        include_str!("../../rollback/0032_tenancy_tranche1_prepare_down.sql");

    /// Unwind to the 0027 baseline, in order.
    ///
    /// Unwind order is 0041 -> 0033-0040 -> 0032 -> 0031 -> 0030 -> 0028.
    /// Tenancy tranche 1 hangs five composite foreign keys off
    /// `agents_tenant_id_id_key`, and 0030's agent-side foreign key depends on
    /// it too; the 0028 rollback drops it, so both have to come out first.
    /// 0030's rollback in turn refuses while 0031 is applied. Each script
    /// refuses up front and names the one to run first. Every test that rolls step 1
    /// back goes through here rather than reaching for `ROLLBACK_SQL` directly,
    /// so a future migration adding another dependency has one place to update.
    async fn rollback_to_0027(pool: &PgPool) {
        sqlx::raw_sql(TRANCHE1_CONSTRAINTS_ROLLBACK_SQL)
            .execute(pool)
            .await
            .expect("0041 rollback");
        sqlx::raw_sql(TRANCHE1_INDEXES_ROLLBACK_SQL)
            .execute(pool)
            .await
            .expect("0033-0040 rollback");
        sqlx::raw_sql(TRANCHE1_PREPARE_ROLLBACK_SQL)
            .execute(pool)
            .await
            .expect("0032 rollback");
        sqlx::raw_sql(GRANTS_HARDENING_ROLLBACK_SQL)
            .execute(pool)
            .await
            .expect("0031 rollback");
        sqlx::raw_sql(GRANTS_ROLLBACK_SQL)
            .execute(pool)
            .await
            .expect("0030 rollback");
        sqlx::raw_sql(ROLLBACK_SQL)
            .execute(pool)
            .await
            .expect("0028 rollback");
    }

    fn tenant_a() -> Uuid {
        Uuid::parse_str("aaaaaaaa-0000-4000-8000-000000000001").unwrap()
    }

    fn tenant_b() -> Uuid {
        Uuid::parse_str("bbbbbbbb-0000-4000-8000-000000000002").unwrap()
    }

    /// Byte-for-byte the V1 write path (src/memory/store.rs:63).
    async fn legacy_upsert_agent(pool: &PgPool, agent_id: &str) {
        sqlx::query("INSERT INTO agents (agent_id) VALUES ($1) ON CONFLICT (agent_id) DO NOTHING")
            .bind(agent_id)
            .execute(pool)
            .await
            .unwrap();
    }

    /// Every query below is a literal with bound parameters — sqlx 0.9 requires
    /// `&'static str`, and interpolating identifiers into test SQL is a habit
    /// worth not forming.
    async fn scalar_i64(pool: &PgPool, sql: &'static str) -> i64 {
        let row: (i64,) = sqlx::query_as(sql).fetch_one(pool).await.unwrap();
        row.0
    }

    async fn count_agents_in_tenant(pool: &PgPool, tenant_id: Uuid) -> i64 {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*)::bigint FROM agents WHERE tenant_id = $1")
                .bind(tenant_id)
                .fetch_one(pool)
                .await
                .unwrap();
        row.0
    }

    async fn constraint_exists(pool: &PgPool, name: &str) -> bool {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*)::bigint FROM pg_constraint \
             WHERE conname = $1 AND conrelid = 'agents'::regclass",
        )
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
        row.0 > 0
    }

    async fn column_exists(pool: &PgPool, table: &str, column: &str) -> bool {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*)::bigint FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND table_name = $1 AND column_name = $2",
        )
        .bind(table)
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap();
        row.0 > 0
    }

    /// `MANAGEMENT_API_KEY` is set — the V1 production shape.
    fn legacy_key_set() -> ManagementAuth {
        ManagementAuth {
            legacy_key_accepted: true,
            unauthenticated: false,
        }
    }

    /// The legacy key is gone *and* something authenticates in its place. That
    /// replacement is the credential registry of plan §10 steps 2-3; this is
    /// the only management-auth state the gate is meant to accept.
    fn legacy_key_retired() -> ManagementAuth {
        ManagementAuth {
            legacy_key_accepted: false,
            unauthenticated: false,
        }
    }

    /// No key and `ALLOW_UNAUTH_MANAGEMENT=true`: every `/api/v1/*` route open.
    fn management_unauthenticated() -> ManagementAuth {
        ManagementAuth {
            legacy_key_accepted: false,
            unauthenticated: true,
        }
    }

    fn mapped(pairs: &[(&str, Uuid)]) -> LegacyTenancyPlan {
        LegacyTenancyPlan::Mapped {
            mappings: pairs
                .iter()
                .map(|(name, tenant)| ((*name).to_string(), *tenant))
                .collect(),
        }
    }

    // ── Per-tenant identity ───────────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn same_external_agent_id_in_two_tenants_produces_distinct_agents(pool: PgPool) {
        let in_a = insert_agent(&pool, tenant_a(), "assistant").await.unwrap();
        let in_b = insert_agent(&pool, tenant_b(), "assistant").await.unwrap();

        assert_ne!(
            in_a, in_b,
            "the same external identifier in two tenants must be two agents"
        );

        // Each tenant resolves its own agent and nothing else.
        assert_eq!(
            resolve_agent_uuid(&pool, tenant_a(), "assistant")
                .await
                .unwrap(),
            Some(in_a)
        );
        assert_eq!(
            resolve_agent_uuid(&pool, tenant_b(), "assistant")
                .await
                .unwrap(),
            Some(in_b)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_legacy_identifier_cannot_squat_a_compatibility_key(pool: PgPool) {
        // `agents.agent_id` is arbitrary text, so a legacy V1 agent may already
        // be named whatever a compatibility-key scheme would generate. A key
        // derived from the caller-supplied external id would collide here and
        // reject an otherwise-valid (tenant_id, external_agent_id) pair; a key
        // derived from the row's own UUID cannot be steered into a collision.
        legacy_upsert_agent(&pool, &format!("assistant#{}", tenant_a())).await;
        legacy_upsert_agent(&pool, "assistant").await;

        let scoped = insert_agent(&pool, tenant_a(), "assistant").await.unwrap();

        assert_eq!(
            resolve_agent_uuid(&pool, tenant_a(), "assistant")
                .await
                .unwrap(),
            Some(scoped)
        );
        // Three distinct agents: two legacy, one tenant-scoped.
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::bigint FROM agents").await,
            3
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn duplicate_external_agent_id_within_one_tenant_is_rejected(pool: PgPool) {
        insert_agent(&pool, tenant_a(), "assistant").await.unwrap();

        // A distinct legacy key is supplied deliberately, so the only constraint
        // that can reject this row is the new per-tenant one.  Without it the
        // retained global `agents_agent_id_key` would fire first and the test
        // would prove nothing about tenant-scoped uniqueness.
        let error = sqlx::query(
            "INSERT INTO agents (agent_id, tenant_id, external_agent_id) VALUES ($1, $2, $3)",
        )
        .bind("a-distinct-legacy-key")
        .bind(tenant_a())
        .bind("assistant")
        .execute(&pool)
        .await
        .unwrap_err();

        assert_eq!(
            error
                .as_database_error()
                .expect("expected a database error")
                .constraint(),
            Some("agents_tenant_id_external_agent_id_key")
        );
    }

    // ── Legacy migration modes ────────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn single_tenant_backfill_assigns_every_historical_row(pool: PgPool) {
        for agent in ["alpha", "beta", "gamma"] {
            legacy_upsert_agent(&pool, agent).await;
        }

        let report = run_legacy_backfill(
            &pool,
            &LegacyTenancyPlan::SingleTenant {
                tenant_id: tenant_a(),
            },
        )
        .await
        .unwrap();

        assert_eq!(report.agents_total, 3);
        assert_eq!(report.agents_assigned, 3);
        assert_eq!(report.agents_unmapped, 0);
        assert!(!report.already_applied);

        for agent in ["alpha", "beta", "gamma"] {
            assert!(resolve_agent_uuid(&pool, tenant_a(), agent)
                .await
                .unwrap()
                .is_some());
            assert!(
                resolve_agent_uuid(&pool, tenant_b(), agent)
                    .await
                    .unwrap()
                    .is_none(),
                "{agent} must not resolve in a tenant it was never assigned to"
            );
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unmapped_agents_remain_inaccessible_in_mapped_mode(pool: PgPool) {
        legacy_upsert_agent(&pool, "mapped-one").await;
        legacy_upsert_agent(&pool, "unmapped-one").await;

        let report = run_legacy_backfill(&pool, &mapped(&[("mapped-one", tenant_a())]))
            .await
            .unwrap();

        assert_eq!(report.agents_assigned, 1);
        assert_eq!(report.agents_unmapped, 1);

        assert!(resolve_agent_uuid(&pool, tenant_a(), "mapped-one")
            .await
            .unwrap()
            .is_some());

        // Served to nobody: not to the tenant that was mapped, not to any other.
        assert!(resolve_agent_uuid(&pool, tenant_a(), "unmapped-one")
            .await
            .unwrap()
            .is_none());
        assert!(resolve_agent_uuid(&pool, tenant_b(), "unmapped-one")
            .await
            .unwrap()
            .is_none());

        // Unreachable, but not destroyed — the row is still there to be mapped
        // later, which is what makes the migration reversible.
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*)::bigint FROM agents \
                 WHERE external_agent_id = 'unmapped-one' AND tenant_id IS NULL",
            )
            .await,
            1
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn backfill_is_idempotent(pool: PgPool) {
        for agent in ["alpha", "beta"] {
            legacy_upsert_agent(&pool, agent).await;
        }
        let plan = LegacyTenancyPlan::SingleTenant {
            tenant_id: tenant_a(),
        };

        let first = run_legacy_backfill(&pool, &plan).await.unwrap();
        assert_eq!(first.agents_assigned, 2);
        assert!(!first.already_applied);

        let second = run_legacy_backfill(&pool, &plan).await.unwrap();
        assert_eq!(second.agents_assigned, 0, "a re-run must assign nothing");
        assert!(second.already_applied);
        assert_eq!(second.agents_total, first.agents_total);
        assert_eq!(second.agents_unmapped, first.agents_unmapped);

        // Assignments are untouched and the ledger gained no no-op row.
        assert_eq!(count_agents_in_tenant(&pool, tenant_a()).await, 2);
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*)::bigint FROM agent_tenancy_migrations"
            )
            .await,
            1
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn backfill_refuses_to_switch_mode_after_the_fact(pool: PgPool) {
        legacy_upsert_agent(&pool, "alpha").await;
        run_legacy_backfill(
            &pool,
            &LegacyTenancyPlan::SingleTenant {
                tenant_id: tenant_a(),
            },
        )
        .await
        .unwrap();

        let error = run_legacy_backfill(&pool, &mapped(&[("alpha", tenant_b())]))
            .await
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("already migrated"),
            "unexpected error: {error:#}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn mapped_backfill_refuses_to_move_an_already_assigned_agent(pool: PgPool) {
        legacy_upsert_agent(&pool, "alpha").await;
        run_legacy_backfill(&pool, &mapped(&[("alpha", tenant_a())]))
            .await
            .unwrap();

        let error = run_legacy_backfill(&pool, &mapped(&[("alpha", tenant_b())]))
            .await
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("contradicts"),
            "unexpected error: {error:#}"
        );
        // The original assignment survived the rejected run.
        assert_eq!(
            resolve_agent_uuid(&pool, tenant_b(), "alpha")
                .await
                .unwrap(),
            None
        );
        assert!(resolve_agent_uuid(&pool, tenant_a(), "alpha")
            .await
            .unwrap()
            .is_some());
    }

    // ── Multi-tenant enablement gate ──────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn enablement_is_refused_when_the_migration_mode_is_absent(pool: PgPool) {
        let settings = TenancySettings {
            legacy_plan: None,
            multi_tenant_enabled: true,
        };

        let error = assert_multi_tenant_preconditions(&pool, &settings, legacy_key_retired())
            .await
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("LEGACY_MIGRATION_MODE"),
            "unexpected error: {error:#}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enablement_is_refused_before_the_backfill_has_run(pool: PgPool) {
        legacy_upsert_agent(&pool, "alpha").await;
        let settings = TenancySettings {
            legacy_plan: Some(LegacyTenancyPlan::SingleTenant {
                tenant_id: tenant_a(),
            }),
            multi_tenant_enabled: true,
        };

        let error = assert_multi_tenant_preconditions(&pool, &settings, legacy_key_retired())
            .await
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("no legacy tenancy migration has been recorded"),
            "unexpected error: {error:#}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enablement_is_refused_while_any_row_is_unmapped(pool: PgPool) {
        legacy_upsert_agent(&pool, "mapped-one").await;
        legacy_upsert_agent(&pool, "unmapped-one").await;

        let plan = mapped(&[("mapped-one", tenant_a())]);
        run_legacy_backfill(&pool, &plan).await.unwrap();

        let settings = TenancySettings {
            legacy_plan: Some(plan),
            multi_tenant_enabled: true,
        };
        let error = assert_multi_tenant_preconditions(&pool, &settings, legacy_key_retired())
            .await
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("no tenant assignment"),
            "unexpected error: {error:#}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enablement_is_refused_while_the_legacy_management_key_is_accepted(pool: PgPool) {
        legacy_upsert_agent(&pool, "alpha").await;
        let plan = LegacyTenancyPlan::SingleTenant {
            tenant_id: tenant_a(),
        };
        run_legacy_backfill(&pool, &plan).await.unwrap();

        let settings = TenancySettings {
            legacy_plan: Some(plan),
            multi_tenant_enabled: true,
        };
        let error = assert_multi_tenant_preconditions(&pool, &settings, legacy_key_set())
            .await
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("MANAGEMENT_API_KEY"),
            "unexpected error: {error:#}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enablement_is_refused_when_the_recorded_mode_differs(pool: PgPool) {
        legacy_upsert_agent(&pool, "alpha").await;
        run_legacy_backfill(&pool, &mapped(&[("alpha", tenant_a())]))
            .await
            .unwrap();

        let settings = TenancySettings {
            legacy_plan: Some(LegacyTenancyPlan::SingleTenant {
                tenant_id: tenant_a(),
            }),
            multi_tenant_enabled: true,
        };
        let error = assert_multi_tenant_preconditions(&pool, &settings, legacy_key_retired())
            .await
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("does not match the configured mode"),
            "unexpected error: {error:#}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enablement_succeeds_once_every_precondition_holds(pool: PgPool) {
        legacy_upsert_agent(&pool, "alpha").await;
        let plan = LegacyTenancyPlan::SingleTenant {
            tenant_id: tenant_a(),
        };
        run_legacy_backfill(&pool, &plan).await.unwrap();

        let settings = TenancySettings {
            legacy_plan: Some(plan),
            multi_tenant_enabled: true,
        };
        assert_multi_tenant_preconditions(&pool, &settings, legacy_key_retired())
            .await
            .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enablement_is_refused_while_management_routes_are_unauthenticated(pool: PgPool) {
        legacy_upsert_agent(&pool, "alpha").await;
        let plan = LegacyTenancyPlan::SingleTenant {
            tenant_id: tenant_a(),
        };
        run_legacy_backfill(&pool, &plan).await.unwrap();

        // Every other precondition holds, and MANAGEMENT_API_KEY is absent — so
        // a gate that read "no legacy key" as "legacy key retired" would let a
        // deployment with no authentication at all enable multi-tenant mode.
        let settings = TenancySettings {
            legacy_plan: Some(plan),
            multi_tenant_enabled: true,
        };
        let error =
            assert_multi_tenant_preconditions(&pool, &settings, management_unauthenticated())
                .await
                .unwrap_err();

        assert!(
            format!("{error:#}").contains("ALLOW_UNAUTH_MANAGEMENT"),
            "unexpected error: {error:#}"
        );
    }

    // ── V1 behaviour is unchanged ─────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn v1_write_path_is_unchanged_and_assigns_no_tenant(pool: PgPool) {
        legacy_upsert_agent(&pool, "assistant").await;

        let (agent_id, external_agent_id, tenant_id): (String, String, Option<Uuid>) =
            sqlx::query_as(
                "SELECT agent_id, external_agent_id, tenant_id FROM agents WHERE agent_id = $1",
            )
            .bind("assistant")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(agent_id, "assistant", "legacy identifier is preserved");
        assert_eq!(
            external_agent_id, "assistant",
            "the bridge trigger mirrors the legacy identifier"
        );
        assert!(
            tenant_id.is_none(),
            "no tenant may be assigned implicitly by a V1 write"
        );

        // ON CONFLICT (agent_id) still resolves, so the V1 upsert stays a no-op
        // on repeat rather than erroring.
        legacy_upsert_agent(&pool, "assistant").await;
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::bigint FROM agents").await,
            1
        );

        // The global namespace is still enforced by the retained constraint.
        let error = sqlx::query("INSERT INTO agents (agent_id) VALUES ($1)")
            .bind("assistant")
            .execute(&pool)
            .await
            .unwrap_err();
        assert_eq!(
            error
                .as_database_error()
                .expect("expected a database error")
                .constraint(),
            Some("agents_agent_id_key")
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn legacy_foreign_keys_still_reference_agent_id(pool: PgPool) {
        legacy_upsert_agent(&pool, "assistant").await;

        sqlx::query("INSERT INTO sessions (session_id, agent_id) VALUES ($1, $2)")
            .bind("session-1")
            .bind("assistant")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO archival_batches (agent_id, source_count, l3_count) VALUES ($1, 1, 1)",
        )
        .bind("assistant")
        .execute(&pool)
        .await
        .unwrap();

        // An unknown agent is still rejected by both foreign keys.
        assert!(
            sqlx::query("INSERT INTO sessions (session_id, agent_id) VALUES ($1, $2)")
                .bind("session-2")
                .bind("nonexistent")
                .execute(&pool)
                .await
                .is_err()
        );

        // ON DELETE CASCADE still reaches both children.
        sqlx::query("DELETE FROM agents WHERE agent_id = $1")
            .bind("assistant")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::bigint FROM sessions").await,
            0
        );
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::bigint FROM archival_batches").await,
            0
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn gate_is_inert_until_multi_tenant_mode_is_enabled(pool: PgPool) {
        legacy_upsert_agent(&pool, "assistant").await;

        // The default deployment: no mode, no backfill, unmapped rows, legacy
        // key still accepted — and startup is still allowed, because nothing
        // has opted into multi-tenancy.
        assert_multi_tenant_preconditions(
            &pool,
            &TenancySettings::default(),
            management_unauthenticated(),
        )
        .await
        .unwrap();
        assert_eq!(count_unmapped_agents(&pool).await.unwrap(), 1);
    }

    // ── Upgrading a populated deployment ──────────────────────────────────────

    /// `#[sqlx::test]` applies every migration to an *empty* database, so the
    /// `external_agent_id` backfill in 0028 never sees a row there. This test
    /// walks the path a real deployment takes: rewind to the 0027 baseline,
    /// create agents exactly as a pre-0028 release did, then apply 0028.
    #[sqlx::test(migrations = "./migrations")]
    async fn upgrading_a_populated_deployment_preserves_identifiers(pool: PgPool) {
        rollback_to_0027(&pool).await;
        assert!(!column_exists(&pool, "agents", "external_agent_id").await);

        for agent in ["alpha", "beta"] {
            legacy_upsert_agent(&pool, agent).await;
        }
        let ids_before: Vec<(String, Uuid)> =
            sqlx::query_as("SELECT agent_id, id FROM agents ORDER BY agent_id")
                .fetch_all(&pool)
                .await
                .unwrap();

        sqlx::raw_sql(MIGRATION_SQL).execute(&pool).await.unwrap();

        let rows: Vec<(String, String, Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT agent_id, external_agent_id, id, tenant_id FROM agents ORDER BY agent_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(rows.len(), 2);
        for (i, (agent_id, external_agent_id, id, tenant_id)) in rows.iter().enumerate() {
            assert_eq!(
                agent_id, external_agent_id,
                "the caller-facing identifier must be preserved verbatim"
            );
            assert_eq!(
                (agent_id.clone(), *id),
                ids_before[i],
                "existing agents keep their identifier and their UUID across the upgrade"
            );
            assert!(
                tenant_id.is_none(),
                "the migration must not assign a tenant to anything"
            );
        }

        assert!(constraint_exists(&pool, "agents_tenant_id_external_agent_id_key").await);
        assert!(constraint_exists(&pool, "agents_tenant_id_id_key").await);

        // Re-applying is a no-op: every statement in 0028 is guarded.
        sqlx::raw_sql(MIGRATION_SQL).execute(&pool).await.unwrap();
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::bigint FROM agents").await,
            2
        );
    }

    // ── Reversibility ─────────────────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn rollback_restores_the_baseline_schema_without_data_loss(pool: PgPool) {
        legacy_upsert_agent(&pool, "legacy-agent").await;
        insert_agent(&pool, tenant_a(), "scoped-agent")
            .await
            .unwrap();
        run_legacy_backfill(
            &pool,
            &LegacyTenancyPlan::SingleTenant {
                tenant_id: tenant_a(),
            },
        )
        .await
        .unwrap();

        rollback_to_0027(&pool).await;

        // Everything 0028 added is gone.
        assert!(!column_exists(&pool, "agents", "tenant_id").await);
        assert!(!column_exists(&pool, "agents", "external_agent_id").await);
        assert!(!constraint_exists(&pool, "agents_tenant_id_external_agent_id_key").await);
        assert!(!constraint_exists(&pool, "agents_tenant_id_id_key").await);
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*)::bigint FROM pg_trigger \
                 WHERE tgname = 'agents_bridge_identity_columns_trg'",
            )
            .await,
            0
        );
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*)::bigint FROM pg_class \
                 WHERE relname = 'agent_tenancy_migrations'",
            )
            .await,
            0
        );
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*)::bigint FROM _sqlx_migrations WHERE version = 28",
            )
            .await,
            0,
            "the migration ledger must allow 0028 to be re-applied"
        );

        // The baseline survived: both agents, their identifiers, the global
        // unique constraint, and the V1 write path.
        assert!(constraint_exists(&pool, "agents_agent_id_key").await);
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::bigint FROM agents").await,
            2
        );
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*)::bigint FROM agents WHERE agent_id = 'legacy-agent'",
            )
            .await,
            1
        );
        // The tenant-scoped agent is back under the identifier its caller used,
        // not under the UUID compatibility key it carried while 0028 was
        // applied. Without the restore step it would be reachable only by a
        // value it never advertised.
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*)::bigint FROM agents WHERE agent_id = 'scoped-agent'",
            )
            .await,
            1
        );
        legacy_upsert_agent(&pool, "post-rollback-agent").await;
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::bigint FROM agents").await,
            3
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn rollback_repoints_dependent_rows_when_restoring_identifiers(pool: PgPool) {
        // A tenant-scoped agent carries a UUID compatibility key in agent_id,
        // and sessions/archival_batches reference *that*. Both foreign keys are
        // ON UPDATE NO ACTION, so restoring the identifier without repointing
        // the children first is rejected by PostgreSQL and aborts the rollback.
        insert_agent(&pool, tenant_a(), "scoped-agent")
            .await
            .unwrap();
        let compat_key: (String,) =
            sqlx::query_as("SELECT agent_id FROM agents WHERE external_agent_id = $1")
                .bind("scoped-agent")
                .fetch_one(&pool)
                .await
                .unwrap();

        sqlx::query("INSERT INTO sessions (session_id, agent_id) VALUES ($1, $2)")
            .bind("session-1")
            .bind(&compat_key.0)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO archival_batches (agent_id, source_count, l3_count) VALUES ($1, 1, 1)",
        )
        .bind(&compat_key.0)
        .execute(&pool)
        .await
        .unwrap();

        rollback_to_0027(&pool).await;

        // Parent and both children now agree on the caller-facing identifier.
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*)::bigint FROM agents WHERE agent_id = 'scoped-agent'",
            )
            .await,
            1
        );
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*)::bigint FROM sessions WHERE agent_id = 'scoped-agent'",
            )
            .await,
            1
        );
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*)::bigint FROM archival_batches WHERE agent_id = 'scoped-agent'",
            )
            .await,
            1
        );

        // Both foreign keys are back, so the cascade still reaches the children.
        sqlx::query("DELETE FROM agents WHERE agent_id = $1")
            .bind("scoped-agent")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::bigint FROM sessions").await,
            0
        );
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::bigint FROM archival_batches").await,
            0
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn rollback_refuses_when_the_baseline_cannot_hold_the_identifiers(pool: PgPool) {
        // Two tenants legitimately share one external_agent_id under 0028 — and
        // that is exactly what the baseline's global UNIQUE on agent_id cannot
        // represent. Silently keeping one and mangling the other would be worse
        // than refusing, so the script raises and changes nothing.
        insert_agent(&pool, tenant_a(), "assistant").await.unwrap();
        insert_agent(&pool, tenant_b(), "assistant").await.unwrap();

        // On a dedicated connection: the script opens an explicit transaction
        // (right for `psql -f`, where autocommit would otherwise leave a failed
        // rollback half-applied), so the RAISE leaves this connection in an
        // aborted transaction that must be cleared before it is reused.
        // Tranche 1 and then 0030 first, or a migration-order guard fires and
        // this test would pass for the wrong reason — it would prove the
        // ordering guard works, which is a different test, rather than proving
        // the identifier check refuses.
        sqlx::raw_sql(TRANCHE1_CONSTRAINTS_ROLLBACK_SQL)
            .execute(&pool)
            .await
            .expect("0041 rollback");
        sqlx::raw_sql(TRANCHE1_INDEXES_ROLLBACK_SQL)
            .execute(&pool)
            .await
            .expect("0033-0040 rollback");
        sqlx::raw_sql(TRANCHE1_PREPARE_ROLLBACK_SQL)
            .execute(&pool)
            .await
            .expect("0032 rollback");
        sqlx::raw_sql(GRANTS_HARDENING_ROLLBACK_SQL)
            .execute(&pool)
            .await
            .expect("0031 rollback");
        sqlx::raw_sql(GRANTS_ROLLBACK_SQL)
            .execute(&pool)
            .await
            .expect("0030 rollback");

        let mut conn = pool.acquire().await.unwrap();
        let error = sqlx::raw_sql(ROLLBACK_SQL)
            .execute(&mut *conn)
            .await
            .unwrap_err();
        sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await.unwrap();
        drop(conn);

        assert!(
            error
                .to_string()
                .contains("would lose caller-facing identifiers"),
            "unexpected error: {error}"
        );
        // The refusal is inside the script's transaction, so nothing was
        // dropped and the deployment is still on 0028.
        assert!(column_exists(&pool, "agents", "external_agent_id").await);
        assert!(constraint_exists(&pool, "agents_tenant_id_external_agent_id_key").await);
        assert_eq!(
            scalar_i64(&pool, "SELECT COUNT(*)::bigint FROM agents").await,
            2
        );
    }
}
