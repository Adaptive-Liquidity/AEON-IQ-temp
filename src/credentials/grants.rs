//! Agent grants (plan §2.2, §10 step 3).
//!
//! Authentication established *who is calling*. This establishes *which agents
//! they may act for* — a separate control, because tenant isolation does not
//! imply agent authorization. Without it, any valid credential could name any
//! agent in its own tenant and satisfy every route's tenant+agent predicate,
//! which is the cross-agent hole plan test #5 forbids.
//!
//! # The two modes, and the two states that are not modes
//!
//! | Credential | Grant rows | `tenant_wide` | Result |
//! |---|---|---|---|
//! | Agent-restricted | ≥1 | `false` | may act only for the granted agents |
//! | Tenant-wide | 0 | `true` | may act for any agent **in its own tenant** |
//! | *Unconfigured* | 0 | `false` | **denied** — this is the fail-closed default |
//! | *Contradictory* | ≥1 | `true` | **denied**, loudly |
//!
//! The unconfigured row is why `credentials.tenant_wide` defaults to `false`:
//! forgetting to add grants denies rather than permits.
//!
//! The contradictory row asserts both "all agents" and "these agents".
//! Resolving it toward tenant-wide would let a stray flag quietly widen a
//! restricted credential; resolving it toward the grants would let a flag
//! intended as tenant-wide be silently narrowed. Migration 0030 makes it
//! unrepresentable with a pair of triggers; [`authorize_agent`] refuses it
//! anyway, because a database whose triggers were disabled or restored from a
//! drifted dump is exactly when a second line matters.
//!
//! # Not wired to any route
//!
//! Nothing here is consulted on a request path. Route enforcement — resolving
//! `external_agent_id` within the authenticated tenant, then requiring a grant,
//! then running the tenant-scoped query, all returning one identical not-found
//! — is plan step 5.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::context::AeonAuthContext;
use super::store::StoreError;

/// Why a credential may act for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantBasis {
    /// A row in `credential_agent_grants` names this agent.
    ExplicitGrant,
    /// `credentials.tenant_wide` is set and the agent is in the same tenant.
    TenantWide,
}

/// Why it may not. **Internal**: step 5 collapses every one of these into the
/// same not-found, so the distinction never reaches a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialReason {
    /// The credential is agent-restricted and no grant names this agent. The
    /// ordinary denial, and the one an unconfigured credential always gets.
    NoGrant,
    /// The agent does not belong to the authenticated tenant — including the
    /// case where it belongs to no tenant at all (`agents.tenant_id IS NULL`,
    /// step 1's "unmapped" state).
    AgentOutsideTenant,
    /// `tenant_wide` and grant rows at once. Should be unrepresentable; denied
    /// rather than resolved if it somehow exists.
    ContradictoryMode,
    /// No credential with this id in this tenant. Post-authentication this
    /// means the row was deleted or moved between the two steps.
    CredentialNotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDecision {
    Granted(GrantBasis),
    Denied(DenialReason),
}

impl AgentDecision {
    pub fn is_granted(&self) -> bool {
        matches!(self, Self::Granted(_))
    }

    /// Short label for audit records and logs.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Granted(GrantBasis::ExplicitGrant) => "granted:explicit",
            Self::Granted(GrantBasis::TenantWide) => "granted:tenant_wide",
            Self::Denied(DenialReason::NoGrant) => "denied:no_grant",
            Self::Denied(DenialReason::AgentOutsideTenant) => "denied:agent_outside_tenant",
            Self::Denied(DenialReason::ContradictoryMode) => "denied:contradictory_mode",
            Self::Denied(DenialReason::CredentialNotFound) => "denied:credential_not_found",
        }
    }
}

/// One stored grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentGrant {
    pub credential_id: Uuid,
    pub tenant_id: Uuid,
    /// `agents.id` (UUID), never `agents.agent_id` (the legacy TEXT identifier).
    pub agent_id: Uuid,
    pub created_at: DateTime<Utc>,
}

/// May the authenticated credential act for `agent_id`?
///
/// The tenant is taken from [`AeonAuthContext`] and is **not** a parameter:
/// there is deliberately no way for a caller to propose one.
///
/// One round trip. Every branch fails closed — an error is an error, never an
/// implicit grant, and `Ok` always carries an explicit decision rather than a
/// bare boolean, so a future caller cannot confuse "false" with "we could not
/// tell".
///
/// Credential lifecycle (revoked, expired, disabled) is **not** re-checked
/// here. That is authentication's job, with the 30-second bound documented on
/// the cache. Re-checking it would make agent-bearing routes revoke instantly
/// while every other route still waited out the bound — a second, differently
/// bounded revocation path is more surprising than one consistent one.
pub async fn authorize_agent(
    pool: &PgPool,
    context: &AeonAuthContext,
    agent_id: Uuid,
) -> Result<AgentDecision, StoreError> {
    let tenant_id = context.tenant_id();

    let row: Option<(bool, bool, bool, bool)> = sqlx::query_as(
        "SELECT c.tenant_wide, \
                EXISTS (SELECT 1 FROM credential_agent_grants g \
                         WHERE g.credential_id = c.id) AS has_any_grant, \
                EXISTS (SELECT 1 FROM credential_agent_grants g \
                         WHERE g.credential_id = c.id \
                           AND g.tenant_id = $2 AND g.agent_id = $3) AS grants_agent, \
                EXISTS (SELECT 1 FROM agents a \
                         WHERE a.id = $3 AND a.tenant_id = $2) AS agent_in_tenant \
           FROM credentials c \
          WHERE c.id = $1 AND c.tenant_id = $2",
    )
    .bind(context.credential_id())
    .bind(tenant_id)
    .bind(agent_id)
    .fetch_optional(pool)
    .await?;

    let Some((tenant_wide, has_any_grant, grants_agent, agent_in_tenant)) = row else {
        return Ok(AgentDecision::Denied(DenialReason::CredentialNotFound));
    };

    // Checked first: it is a property of the credential itself, so it should be
    // reported even when the agent would have failed anyway. An operator
    // debugging "why is this denied" needs to see the configuration error, not
    // a downstream symptom of it.
    if tenant_wide && has_any_grant {
        tracing::error!(
            credential_id = %context.credential_id(),
            tenant_id = %tenant_id,
            "credential is tenant_wide and also holds per-agent grants; denying. \
             Migration 0030's triggers should make this unrepresentable — check \
             whether they were disabled or lost in a restore."
        );
        return Ok(AgentDecision::Denied(DenialReason::ContradictoryMode));
    }

    // Applies to BOTH modes. Without it a tenant-wide credential would reach
    // agents in other tenants, which is the opposite of what "tenant-wide"
    // means. The composite foreign key already makes a cross-tenant *grant*
    // unwritable, but tenant-wide credentials have no grant rows to constrain.
    if !agent_in_tenant {
        return Ok(AgentDecision::Denied(DenialReason::AgentOutsideTenant));
    }

    if tenant_wide {
        return Ok(AgentDecision::Granted(GrantBasis::TenantWide));
    }
    if grants_agent {
        return Ok(AgentDecision::Granted(GrantBasis::ExplicitGrant));
    }

    // Zero grants and no flag lands here: denied, which is the point.
    Ok(AgentDecision::Denied(DenialReason::NoGrant))
}

/// Grant an agent to a credential.
///
/// Deliberately **not** idempotent: the primary key rejects a duplicate rather
/// than `ON CONFLICT DO NOTHING` swallowing it, because granting the same agent
/// twice means the caller was not tracking what it had already granted.
///
/// `tenant_id` is passed explicitly and must match both the credential's and
/// the agent's tenant — the database checks that, not this function.
pub async fn grant(
    pool: &PgPool,
    credential_id: Uuid,
    tenant_id: Uuid,
    agent_id: Uuid,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO credential_agent_grants (credential_id, tenant_id, agent_id) \
         VALUES ($1, $2, $3)",
    )
    .bind(credential_id)
    .bind(tenant_id)
    .bind(agent_id)
    .execute(pool)
    .await?;

    tracing::info!(%credential_id, %tenant_id, %agent_id, "agent grant added");
    Ok(())
}

/// Withdraw a grant. Returns whether a row was removed.
pub async fn revoke_grant(
    pool: &PgPool,
    credential_id: Uuid,
    agent_id: Uuid,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "DELETE FROM credential_agent_grants WHERE credential_id = $1 AND agent_id = $2",
    )
    .bind(credential_id)
    .bind(agent_id)
    .execute(pool)
    .await?;

    let removed = result.rows_affected() > 0;
    if removed {
        tracing::info!(%credential_id, %agent_id, "agent grant withdrawn");
    }
    Ok(removed)
}

/// Every grant held by a credential, oldest first.
pub async fn list_grants(
    pool: &PgPool,
    credential_id: Uuid,
) -> Result<Vec<AgentGrant>, StoreError> {
    let rows: Vec<(Uuid, Uuid, Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT credential_id, tenant_id, agent_id, created_at \
           FROM credential_agent_grants WHERE credential_id = $1 \
          ORDER BY created_at, agent_id",
    )
    .bind(credential_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(credential_id, tenant_id, agent_id, created_at)| AgentGrant {
                credential_id,
                tenant_id,
                agent_id,
                created_at,
            },
        )
        .collect())
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::credentials::context::CredentialMode;
    use crate::credentials::scope::ScopeSet;
    use crate::credentials::store;

    const MIGRATION_0030: &str = include_str!("../../migrations/0030_credential_agent_grants.sql");
    const MIGRATION_0030_DOWN: &str =
        include_str!("../../rollback/0030_credential_agent_grants_down.sql");

    /// A credential written straight through the store, so these tests do not
    /// need a pepper or an authenticator to exercise grants.
    async fn credential(pool: &PgPool, tenant_id: Uuid, tenant_wide: bool) -> Uuid {
        let id = Uuid::new_v4();
        store::insert(
            pool,
            store::NewCredential {
                id,
                tenant_id,
                principal_id: "svc-ingest",
                secret_mac: &[0u8; 32],
                mode: CredentialMode::V2Required,
                scopes: &ScopeSet::parse_list(&["memory:read"]).unwrap(),
                tenant_wide,
                expires_at: None,
            },
        )
        .await
        .expect("credential insert");
        id
    }

    async fn agent(pool: &PgPool, tenant_id: Uuid, external: &str) -> Uuid {
        crate::tenancy::insert_agent(pool, tenant_id, external)
            .await
            .expect("agent insert")
    }

    /// The context an authenticated caller would be holding. Scopes are
    /// irrelevant here: agent authorization is a separate control.
    fn context(credential_id: Uuid, tenant_id: Uuid) -> AeonAuthContext {
        AeonAuthContext::new(
            credential_id,
            tenant_id,
            "svc-ingest".to_string(),
            ScopeSet::default(),
            CredentialMode::V2Required,
        )
    }

    // ── Proof: absence of a grant denies ─────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn absence_of_a_grant_denies(pool: PgPool) {
        // The fail-closed default, and the reason `tenant_wide` defaults false:
        // a credential nobody configured reaches nothing.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;

        assert_eq!(
            authorize_agent(&pool, &context(cred, tenant), target)
                .await
                .unwrap(),
            AgentDecision::Denied(DenialReason::NoGrant),
            "same tenant, valid agent, no grant — must still be denied"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_grant_authorises_only_the_agent_it_names(pool: PgPool) {
        // Plan test #5: a credential must not reach a non-granted agent in its
        // OWN tenant.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let granted = agent(&pool, tenant, "ext-granted").await;
        let other = agent(&pool, tenant, "ext-other").await;

        grant(&pool, cred, tenant, granted).await.unwrap();
        let ctx = context(cred, tenant);

        assert_eq!(
            authorize_agent(&pool, &ctx, granted).await.unwrap(),
            AgentDecision::Granted(GrantBasis::ExplicitGrant)
        );
        assert_eq!(
            authorize_agent(&pool, &ctx, other).await.unwrap(),
            AgentDecision::Denied(DenialReason::NoGrant),
            "an ungranted agent in the same tenant must be denied"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn withdrawing_a_grant_denies_again(pool: PgPool) {
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;
        let ctx = context(cred, tenant);

        grant(&pool, cred, tenant, target).await.unwrap();
        assert!(authorize_agent(&pool, &ctx, target)
            .await
            .unwrap()
            .is_granted());

        assert!(revoke_grant(&pool, cred, target).await.unwrap());
        assert_eq!(
            authorize_agent(&pool, &ctx, target).await.unwrap(),
            AgentDecision::Denied(DenialReason::NoGrant)
        );
        assert!(
            !revoke_grant(&pool, cred, target).await.unwrap(),
            "withdrawing twice removes nothing the second time"
        );
    }

    // ── Proof: the credential belongs to the same tenant ─────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn a_grant_cannot_claim_a_tenant_other_than_the_credentials(pool: PgPool) {
        // The credential-side composite foreign key. Writing tenant B into a
        // grant for a tenant-A credential has no parent row to match.
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let cred_a = credential(&pool, tenant_a, false).await;
        let agent_b = agent(&pool, tenant_b, "ext-b").await;

        let error = grant(&pool, cred_a, tenant_b, agent_b)
            .await
            .expect_err("a grant must not name a tenant the credential is not in");
        assert!(
            format!("{error:?}").contains("credential_agent_grants_credential_fkey"),
            "expected the credential-side FK to reject it, got: {error:?}"
        );
    }

    // ── Proof: the agent belongs to the same tenant ──────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn a_grant_cannot_name_an_agent_in_another_tenant(pool: PgPool) {
        // The agent-side composite foreign key.
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let cred_a = credential(&pool, tenant_a, false).await;
        let agent_b = agent(&pool, tenant_b, "ext-b").await;

        let error = grant(&pool, cred_a, tenant_a, agent_b)
            .await
            .expect_err("a grant must not name an agent outside its tenant");
        assert!(
            format!("{error:?}").contains("credential_agent_grants_agent_fkey"),
            "expected the agent-side FK to reject it, got: {error:?}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unmapped_agent_can_never_be_granted(pool: PgPool) {
        // Step 1 encodes "unmapped" as `agents.tenant_id IS NULL`, and a NULL
        // parent cannot satisfy a composite foreign key. Unmapped agents are
        // served to nobody, which is the property step 1 established and this
        // step must not weaken.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;

        let unmapped = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO agents (id, agent_id, external_agent_id, tenant_id) \
             VALUES ($1, $2, $2, NULL)",
        )
        .bind(unmapped)
        .bind(format!("legacy-{unmapped}"))
        .execute(&pool)
        .await
        .expect("an unmapped agent is representable");

        assert!(
            grant(&pool, cred, tenant, unmapped).await.is_err(),
            "an agent belonging to no tenant must not be grantable"
        );
    }

    // ── Proof: cross-tenant grants cannot be inserted ────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn no_tenant_value_can_satisfy_a_cross_tenant_grant(pool: PgPool) {
        // Both foreign keys read the SAME `tenant_id` column, so there is no
        // value that satisfies a tenant-A credential and a tenant-B agent at
        // once. Every candidate is enumerated rather than argued.
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let cred_a = credential(&pool, tenant_a, false).await;
        let agent_b = agent(&pool, tenant_b, "ext-b").await;

        for (label, claimed) in [
            ("the credential's tenant", tenant_a),
            ("the agent's tenant", tenant_b),
            ("a third tenant", Uuid::new_v4()),
        ] {
            assert!(
                grant(&pool, cred_a, claimed, agent_b).await.is_err(),
                "a cross-tenant grant claiming {label} must be rejected"
            );
        }

        // …and direct SQL fares no better: this is a database constraint, not
        // an application check that could be bypassed.
        assert!(
            sqlx::query(
                "INSERT INTO credential_agent_grants (credential_id, tenant_id, agent_id) \
                 VALUES ($1, $2, $3)"
            )
            .bind(cred_a)
            .bind(tenant_a)
            .bind(agent_b)
            .execute(&pool)
            .await
            .is_err(),
            "direct SQL must be rejected too"
        );
    }

    // ── Proof: duplicate grants are impossible ───────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn duplicate_grants_are_impossible(pool: PgPool) {
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;

        grant(&pool, cred, tenant, target).await.unwrap();
        let error = grant(&pool, cred, tenant, target)
            .await
            .expect_err("the same grant twice must be rejected");
        assert!(
            format!("{error:?}").contains("credential_agent_grants_pkey"),
            "expected the primary key to reject it, got: {error:?}"
        );

        assert_eq!(list_grants(&pool, cred).await.unwrap().len(), 1);
    }

    // ── Proof: tenant_wide is a separate, explicit mode ──────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn a_tenant_wide_credential_reaches_every_agent_in_its_own_tenant(pool: PgPool) {
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, true).await;
        let ctx = context(cred, tenant);

        for external in ["ext-a", "ext-b", "ext-c"] {
            let target = agent(&pool, tenant, external).await;
            assert_eq!(
                authorize_agent(&pool, &ctx, target).await.unwrap(),
                AgentDecision::Granted(GrantBasis::TenantWide),
                "tenant-wide must reach {external} with no grant row"
            );
        }
        assert!(
            list_grants(&pool, cred).await.unwrap().is_empty(),
            "and it holds no grants at all"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_tenant_wide_credential_does_not_reach_another_tenants_agent(pool: PgPool) {
        // "Tenant-wide" is wide within one tenant, not across tenants. There
        // are no grant rows here for a foreign key to constrain, so this is the
        // application predicate's job — which is why it is tested.
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let cred_a = credential(&pool, tenant_a, true).await;
        let agent_b = agent(&pool, tenant_b, "ext-b").await;

        assert_eq!(
            authorize_agent(&pool, &context(cred_a, tenant_a), agent_b)
                .await
                .unwrap(),
            AgentDecision::Denied(DenialReason::AgentOutsideTenant)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_tenant_wide_credential_cannot_be_given_a_grant(pool: PgPool) {
        // The two modes are mutually exclusive at the database level, in both
        // directions. This is the first direction.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, true).await;
        let target = agent(&pool, tenant, "ext-a").await;

        let error = grant(&pool, cred, tenant, target)
            .await
            .expect_err("granting a tenant-wide credential must be refused");
        assert!(
            format!("{error:?}").contains("mutually exclusive"),
            "expected the trigger's message, got: {error:?}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_granted_credential_cannot_be_made_tenant_wide(pool: PgPool) {
        // …and the second direction, which a trigger on `credentials` covers.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;
        grant(&pool, cred, tenant, target).await.unwrap();

        let error = sqlx::query("UPDATE credentials SET tenant_wide = TRUE WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect_err("setting tenant_wide while grants exist must be refused");
        assert!(
            format!("{error:?}").contains("tenant_wide cannot be set"),
            "expected the trigger's message, got: {error:?}"
        );

        // Withdrawing the grant first makes it legal — the modes are exclusive,
        // not one-way.
        revoke_grant(&pool, cred, target).await.unwrap();
        sqlx::query("UPDATE credentials SET tenant_wide = TRUE WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("with no grants left, tenant_wide is a legal choice");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_contradictory_state_is_denied_rather_than_resolved(pool: PgPool) {
        // Reached only by disabling the trigger — which is the point. A restore
        // from a drifted dump, or an operator who disabled triggers for a bulk
        // load, can produce this state, and the application must not resolve it
        // toward either reading.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;
        grant(&pool, cred, tenant, target).await.unwrap();

        sqlx::query(
            "ALTER TABLE credentials DISABLE TRIGGER credentials_not_tenant_wide_with_grants",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE credentials SET tenant_wide = TRUE WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("with the trigger off, the contradictory state is writable");

        let ctx = context(cred, tenant);
        assert_eq!(
            authorize_agent(&pool, &ctx, target).await.unwrap(),
            AgentDecision::Denied(DenialReason::ContradictoryMode),
            "not resolved toward the grant…"
        );

        let other = agent(&pool, tenant, "ext-other").await;
        assert_eq!(
            authorize_agent(&pool, &ctx, other).await.unwrap(),
            AgentDecision::Denied(DenialReason::ContradictoryMode),
            "…and not resolved toward tenant-wide either"
        );
    }

    // ── Lifecycle ────────────────────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn deleting_a_credential_removes_its_grants(pool: PgPool) {
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;
        grant(&pool, cred, tenant, target).await.unwrap();

        sqlx::query("DELETE FROM credentials WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .unwrap();

        assert!(list_grants(&pool, cred).await.unwrap().is_empty());
        assert_eq!(
            authorize_agent(&pool, &context(cred, tenant), target)
                .await
                .unwrap(),
            AgentDecision::Denied(DenialReason::CredentialNotFound)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn deleting_an_agent_removes_the_grants_that_named_it(pool: PgPool) {
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let doomed = agent(&pool, tenant, "ext-doomed").await;
        let kept = agent(&pool, tenant, "ext-kept").await;
        grant(&pool, cred, tenant, doomed).await.unwrap();
        grant(&pool, cred, tenant, kept).await.unwrap();

        sqlx::query("DELETE FROM agents WHERE id = $1")
            .bind(doomed)
            .execute(&pool)
            .await
            .unwrap();

        let remaining = list_grants(&pool, cred).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].agent_id, kept);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn moving_an_agent_to_another_tenant_is_refused_while_grants_exist(pool: PgPool) {
        // `ON UPDATE NO ACTION`, deliberately. A silent repoint would leave a
        // live grant pointing at an agent that now belongs to someone else.
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let cred = credential(&pool, tenant_a, false).await;
        let target = agent(&pool, tenant_a, "ext-a").await;
        grant(&pool, cred, tenant_a, target).await.unwrap();

        assert!(
            sqlx::query("UPDATE agents SET tenant_id = $2 WHERE id = $1")
                .bind(target)
                .bind(tenant_b)
                .execute(&pool)
                .await
                .is_err(),
            "moving a granted agent between tenants must be refused"
        );
    }

    // ── Schema ───────────────────────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn the_migration_is_idempotent(pool: PgPool) {
        for attempt in 1..=2 {
            sqlx::raw_sql(MIGRATION_0030)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("re-applying 0030 (attempt {attempt}) failed: {e}"));
        }

        // …and the table still behaves afterwards.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;
        grant(&pool, cred, tenant, target).await.unwrap();
        assert!(authorize_agent(&pool, &context(cred, tenant), target)
            .await
            .unwrap()
            .is_granted());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_step_one_rollback_refuses_until_this_migration_is_unwound(pool: PgPool) {
        // Step 3's agent-side foreign key depends on `agents_tenant_id_id_key`,
        // which step 1's rollback drops. Without a guard that script fails on a
        // dependency error *partway through* — after it has already moved
        // caller-facing identifiers back into `agents.agent_id`. Refusing up
        // front, naming the remedy, is the difference between "run this first"
        // and "work out what state your database is in now".
        //
        // `DROP ... CASCADE` is deliberately not the answer: it would silently
        // delete every agent grant as a side effect of a rollback the operator
        // asked for on a different migration.
        const STEP_ONE_ROLLBACK: &str =
            include_str!("../../rollback/0028_agent_tenancy_identity_down.sql");

        // The script opens its own transaction, so the RAISE leaves this
        // connection aborted and it must be cleared before reuse.
        let mut conn = pool.acquire().await.unwrap();
        let error = sqlx::raw_sql(STEP_ONE_ROLLBACK)
            .execute(&mut *conn)
            .await
            .expect_err("step 1's rollback must refuse while 0030 is applied");
        sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await.unwrap();
        drop(conn);

        let rendered = error.to_string();
        assert!(rendered.contains("0030"), "{rendered}");
        assert!(
            rendered.contains("rollback/0030_credential_agent_grants_down.sql"),
            "the error must name the script to run first: {rendered}"
        );

        // Nothing was dropped: the refusal is inside the script's transaction.
        let (still_there,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint \
              WHERE conrelid = 'agents'::regclass AND conname = 'agents_tenant_id_id_key')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(still_there, "the refusal must change nothing");

        // In the right order it succeeds.
        sqlx::raw_sql(MIGRATION_0030_DOWN)
            .execute(&pool)
            .await
            .expect("0030 rollback");
        sqlx::raw_sql(STEP_ONE_ROLLBACK)
            .execute(&pool)
            .await
            .expect("0028 rollback, once 0030 is gone");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_rollback_removes_the_table_and_both_triggers(pool: PgPool) {
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;
        grant(&pool, cred, tenant, target).await.unwrap();

        sqlx::raw_sql(MIGRATION_0030_DOWN)
            .execute(&pool)
            .await
            .expect("0030 rollback");

        let (table,): (bool,) =
            sqlx::query_as("SELECT to_regclass('public.credential_agent_grants') IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!table, "the table is gone");

        // Including the trigger on `credentials`, which outlives the grants
        // table and would otherwise be left calling a function about a table
        // that no longer exists.
        let (trigger,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM pg_trigger \
              WHERE tgrelid = 'credentials'::regclass \
                AND tgname = 'credentials_not_tenant_wide_with_grants')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!trigger, "the trigger on credentials is gone too");

        // And `tenant_wide` is once again the inert 0029 flag it was between
        // steps 2 and 3 — settable with no grants table to contradict it.
        sqlx::query("UPDATE credentials SET tenant_wide = TRUE WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("tenant_wide is a plain column again");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_grants_table_carries_every_frozen_constraint(pool: PgPool) {
        for name in [
            "credential_agent_grants_pkey",
            "credential_agent_grants_credential_fkey",
            "credential_agent_grants_agent_fkey",
        ] {
            let (present,): (bool,) = sqlx::query_as(
                "SELECT EXISTS (SELECT 1 FROM pg_constraint \
                  WHERE conrelid = 'credential_agent_grants'::regclass AND conname = $1)",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(present, "constraint {name} is missing");
        }

        // Both foreign keys must be composite — a single-column reference would
        // leave `tenant_id` free to name any tenant, which is the earlier draft
        // §2.2 rejects.
        for (name, expected) in [
            (
                "credential_agent_grants_credential_fkey",
                "FOREIGN KEY (credential_id, tenant_id) REFERENCES credentials(id, tenant_id) ON DELETE CASCADE",
            ),
            (
                "credential_agent_grants_agent_fkey",
                "FOREIGN KEY (tenant_id, agent_id) REFERENCES agents(tenant_id, id) ON DELETE CASCADE",
            ),
        ] {
            let (definition,): (String,) = sqlx::query_as(
                "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
                  WHERE conrelid = 'credential_agent_grants'::regclass AND conname = $1",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(definition, expected, "for {name}");
        }
    }
}
