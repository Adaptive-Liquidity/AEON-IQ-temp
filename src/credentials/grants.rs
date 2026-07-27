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
    pub agent_uuid: Uuid,
    pub created_at: DateTime<Utc>,
}

/// May the authenticated credential act for `agent_uuid`?
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
    agent_uuid: Uuid,
) -> Result<AgentDecision, StoreError> {
    let tenant_id = context.tenant_id();

    let row: Option<(bool, bool, bool, bool)> = sqlx::query_as(
        "SELECT c.tenant_wide, \
                EXISTS (SELECT 1 FROM credential_agent_grants g \
                         WHERE g.credential_id = c.id) AS has_any_grant, \
                EXISTS (SELECT 1 FROM credential_agent_grants g \
                         WHERE g.credential_id = c.id \
                           AND g.tenant_id = $2 AND g.agent_uuid = $3) AS grants_agent, \
                EXISTS (SELECT 1 FROM agents a \
                         WHERE a.id = $3 AND a.tenant_id = $2) AS agent_in_tenant \
           FROM credentials c \
          WHERE c.id = $1 AND c.tenant_id = $2",
    )
    .bind(context.credential_id())
    .bind(tenant_id)
    .bind(agent_uuid)
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
    agent_uuid: Uuid,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO credential_agent_grants (credential_id, tenant_id, agent_uuid) \
         VALUES ($1, $2, $3)",
    )
    .bind(credential_id)
    .bind(tenant_id)
    .bind(agent_uuid)
    .execute(pool)
    .await?;

    tracing::info!(%credential_id, %tenant_id, %agent_uuid, "agent grant added");
    Ok(())
}

/// Withdraw a grant. Returns whether a row was removed.
pub async fn revoke_grant(
    pool: &PgPool,
    credential_id: Uuid,
    agent_uuid: Uuid,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "DELETE FROM credential_agent_grants WHERE credential_id = $1 AND agent_uuid = $2",
    )
    .bind(credential_id)
    .bind(agent_uuid)
    .execute(pool)
    .await?;

    let removed = result.rows_affected() > 0;
    if removed {
        tracing::info!(%credential_id, %agent_uuid, "agent grant withdrawn");
    }
    Ok(removed)
}

/// Every grant held by a credential, oldest first.
pub async fn list_grants(
    pool: &PgPool,
    credential_id: Uuid,
) -> Result<Vec<AgentGrant>, StoreError> {
    let rows: Vec<(Uuid, Uuid, Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT credential_id, tenant_id, agent_uuid, created_at \
           FROM credential_agent_grants WHERE credential_id = $1 \
          ORDER BY created_at, agent_uuid",
    )
    .bind(credential_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(credential_id, tenant_id, agent_uuid, created_at)| AgentGrant {
                credential_id,
                tenant_id,
                agent_uuid,
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
    const MIGRATION_0031: &str =
        include_str!("../../migrations/0031_credential_agent_grants_hardening.sql");
    const MIGRATION_0031_DOWN: &str =
        include_str!("../../rollback/0031_credential_agent_grants_hardening_down.sql");
    const MIGRATION_0028_DOWN: &str =
        include_str!("../../rollback/0028_agent_tenancy_identity_down.sql");

    /// Put a credential into the contradictory state the triggers exist to
    /// prevent, the only way it can happen in practice: with the trigger off.
    ///
    /// Returns `(credential, agent)`.
    async fn contradictory(pool: &PgPool, tenant: Uuid) -> (Uuid, Uuid) {
        let cred = credential(pool, tenant, false).await;
        let target = agent(pool, tenant, "ext-drifted").await;
        grant(pool, cred, tenant, target).await.unwrap();

        sqlx::query(
            "ALTER TABLE credentials DISABLE TRIGGER credentials_not_tenant_wide_with_grants",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE credentials SET tenant_wide = TRUE WHERE id = $1")
            .bind(cred)
            .execute(pool)
            .await
            .expect("with the trigger off, the contradictory state is writable");
        sqlx::query(
            "ALTER TABLE credentials ENABLE TRIGGER credentials_not_tenant_wide_with_grants",
        )
        .execute(pool)
        .await
        .unwrap();

        (cred, target)
    }

    // ── 0031: a drifted credential must remain revocable and repairable ──────

    #[sqlx::test(migrations = "./migrations")]
    async fn a_contradictory_credential_can_still_be_revoked_disabled_and_maintained(pool: PgPool) {
        // 0030's trigger fired on every UPDATE and refused whenever tenant_wide
        // was set and grants existed. That is right for the write that *creates*
        // the contradiction and wrong for every other write: the control meant
        // to prevent a bad state was preventing its repair. A credential nobody
        // could revoke is a worse outcome than the state it was guarding
        // against, because `authorize_agent` already denies it.
        let tenant = Uuid::new_v4();
        let (cred, target) = contradictory(&pool, tenant).await;

        // 1. Authorization still denies it, throughout.
        assert_eq!(
            authorize_agent(&pool, &context(cred, tenant), target)
                .await
                .unwrap(),
            AgentDecision::Denied(DenialReason::ContradictoryMode)
        );

        // 4. Maintenance writes succeed (checked first: it is the one that
        //    happens on every authenticated request).
        sqlx::query("UPDATE credentials SET last_used_at = NOW() WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("last_used_at must not be blocked by a state it did not create");

        // 3. Disabling succeeds.
        sqlx::query("UPDATE credentials SET status = 'disabled' WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("disabling must succeed");

        // 2. Revocation succeeds — the one that actually matters under attack.
        sqlx::query("UPDATE credentials SET status = 'revoked', revoked_at = NOW() WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("revocation must succeed");

        // …and it is still denied afterwards, for the same reason.
        assert_eq!(
            authorize_agent(&pool, &context(cred, tenant), target)
                .await
                .unwrap(),
            AgentDecision::Denied(DenialReason::ContradictoryMode)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_contradictory_credential_can_be_repaired_by_clearing_tenant_wide(pool: PgPool) {
        let tenant = Uuid::new_v4();
        let (cred, target) = contradictory(&pool, tenant).await;

        // 5. true -> false repairs it. Always allowed: this is the exit.
        sqlx::query("UPDATE credentials SET tenant_wide = FALSE WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("clearing tenant_wide must repair rather than be refused");

        // The credential is now a plain agent-restricted one, and its grant works.
        assert_eq!(
            authorize_agent(&pool, &context(cred, tenant), target)
                .await
                .unwrap(),
            AgentDecision::Granted(GrantBasis::ExplicitGrant)
        );

        // 6. false -> true is still refused while the grant exists.
        let error = sqlx::query("UPDATE credentials SET tenant_wide = TRUE WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect_err("re-entering the contradictory state must be refused");
        assert!(
            format!("{error:?}").contains("tenant_wide cannot be set"),
            "{error:?}"
        );

        // 7. …and succeeds once the grants are gone.
        revoke_grant(&pool, cred, target).await.unwrap();
        sqlx::query("UPDATE credentials SET tenant_wide = TRUE WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("with no grants left, tenant_wide is a legal choice");
        assert_eq!(
            authorize_agent(&pool, &context(cred, tenant), target)
                .await
                .unwrap(),
            AgentDecision::Granted(GrantBasis::TenantWide)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_insert_that_creates_the_contradiction_is_still_refused(pool: PgPool) {
        // The INSERT arm of the transition rule. Narrowing the trigger to
        // transitions must not open a hole for a row that arrives contradictory.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;
        grant(&pool, cred, tenant, target).await.unwrap();

        // The arm has to be reached with a credential id that is *free*, or the
        // primary key rejects the INSERT first and the test passes even with the
        // arm deleted. Grants normally cannot precede their credential — the
        // composite foreign key sees to that — so the key is dropped for the
        // duration to stage a grant against an id that does not exist yet.
        //
        // This is a contrived state on purpose: it is the only way to put the
        // trigger's INSERT branch under test, and it is exactly the state a
        // restore-with-constraints-disabled can produce.
        sqlx::query("ALTER TABLE credential_agent_grants DROP CONSTRAINT credential_agent_grants_credential_fkey")
            .execute(&pool)
            .await
            .unwrap();

        let unborn = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO credential_agent_grants (credential_id, tenant_id, agent_uuid) \
             VALUES ($1, $2, $3)",
        )
        .bind(unborn)
        .bind(tenant)
        .bind(target)
        .execute(&pool)
        .await
        .expect("with the foreign key gone, a grant can precede its credential");

        let error = sqlx::query(
            "INSERT INTO credentials (id, tenant_id, principal_id, secret_mac, mode, tenant_wide) \
             VALUES ($1, $2, 'p', $3, 'v2_required', TRUE)",
        )
        .bind(unborn)
        .bind(tenant)
        .bind(vec![0u8; 32])
        .execute(&pool)
        .await
        .expect_err("a credential arriving tenant_wide onto existing grants must be refused");

        // Specifically the trigger, not the primary key: `unborn` is a fresh id.
        assert!(
            format!("{error:?}").contains("tenant_wide cannot be set"),
            "the INSERT arm must be what refuses it, got: {error:?}"
        );

        // …and the same INSERT without tenant_wide is fine, so the refusal is
        // about the contradiction rather than about the row.
        sqlx::query(
            "INSERT INTO credentials (id, tenant_id, principal_id, secret_mac, mode, tenant_wide) \
             VALUES ($1, $2, 'p', $3, 'v2_required', FALSE)",
        )
        .bind(unborn)
        .bind(tenant)
        .bind(vec![0u8; 32])
        .execute(&pool)
        .await
        .expect("an agent-restricted credential over the same grants is fine");

        assert_ne!(cred, unborn);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_uncontradicted_tenant_wide_credential_is_unaffected(pool: PgPool) {
        // The transition rule must not weaken the ordinary path: a tenant-wide
        // credential with no grants still cannot be given one.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, true).await;
        let target = agent(&pool, tenant, "ext-a").await;

        assert!(
            grant(&pool, cred, tenant, target).await.is_err(),
            "granting a tenant-wide credential is still refused"
        );

        // And ordinary maintenance on it still works.
        sqlx::query("UPDATE credentials SET last_used_at = NOW() WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("maintenance on a consistent tenant-wide credential");
    }

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
                "INSERT INTO credential_agent_grants (credential_id, tenant_id, agent_uuid) \
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
        assert_eq!(remaining[0].agent_uuid, kept);
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
        // 0030's idempotency is a property of the schema *at* 0030. Re-applying
        // it on top of 0031 would be re-applying an older migration over a
        // newer one, which is not a thing this repository ever does.
        sqlx::raw_sql(MIGRATION_0031_DOWN)
            .execute(&pool)
            .await
            .expect("0031 rollback");

        for attempt in 1..=2 {
            sqlx::raw_sql(MIGRATION_0030)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("re-applying 0030 (attempt {attempt}) failed: {e}"));
        }

        sqlx::raw_sql(MIGRATION_0031)
            .execute(&pool)
            .await
            .expect("0031 re-applied");

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
        // 0031 first: it is the later migration, and 0030's rollback now
        // refuses while it is applied.
        sqlx::raw_sql(MIGRATION_0031_DOWN)
            .execute(&pool)
            .await
            .expect("0031 rollback");
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

        // 0031 first: it is the later migration, and 0030's rollback now
        // refuses while it is applied.
        sqlx::raw_sql(MIGRATION_0031_DOWN)
            .execute(&pool)
            .await
            .expect("0031 rollback");
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
    async fn the_rollback_is_safe_to_re_run_and_to_run_when_never_applied(pool: PgPool) {
        // Three reviewers independently predicted this would fail, on the theory
        // that `DROP TRIGGER IF EXISTS … ON credential_agent_grants` errors once
        // the table is gone, aborting the transaction and stranding the trigger
        // on `credentials`. On PostgreSQL 16 it does not: the missing relation
        // is skipped with a notice. Pinned here so the question is settled by
        // the database rather than re-argued, and so a change to either the
        // script or the server version is caught.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;
        grant(&pool, cred, tenant, target).await.unwrap();

        // 0031 first: it is the later migration, and 0030's rollback now
        // refuses while it is applied.
        sqlx::raw_sql(MIGRATION_0031_DOWN)
            .execute(&pool)
            .await
            .expect("0031 rollback");

        for attempt in 1..=3 {
            sqlx::raw_sql(MIGRATION_0030_DOWN)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("rollback attempt {attempt} failed: {e}"));
        }

        // Every object is gone after the first pass and stays gone — in
        // particular the trigger on `credentials`, which is the one an aborted
        // transaction would have stranded.
        let (triggers,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM pg_trigger \
              WHERE NOT tgisinternal \
                AND tgname IN ('credentials_not_tenant_wide_with_grants', \
                               'credential_agent_grants_not_tenant_wide')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            triggers, 0,
            "both triggers removed, and re-runs kept them so"
        );

        let (migration_row,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM _sqlx_migrations WHERE version = 30")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(migration_row, 0, "the migration record is cleared");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_trigger_with_the_right_name_but_a_different_definition_is_rejected(pool: PgPool) {
        // The trigger equivalent of the weakened-CHECK drift: right name,
        // enabled, and enforcing nothing. Presence and `tgenabled` both pass.
        sqlx::query(
            "CREATE OR REPLACE FUNCTION inert_noop() RETURNS TRIGGER AS $$ \
             BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE OR REPLACE TRIGGER credential_agent_grants_not_tenant_wide \
             BEFORE INSERT ON credential_agent_grants \
             FOR EACH ROW EXECUTE FUNCTION inert_noop()",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Proof it enforces nothing: a tenant-wide credential can now be
        // granted, which is the state §2.2 forbids.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, true).await;
        let target = agent(&pool, tenant, "ext-a").await;
        grant(&pool, cred, tenant, target)
            .await
            .expect("the inert trigger permits the contradictory state");

        let error = crate::credentials::assert_schema_contract(&pool)
            .await
            .expect_err("an inert trigger must fail the contract");
        let rendered = error
            .chain()
            .map(|cause| cause.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            rendered.contains("has the expected name but a different definition"),
            "{rendered}"
        );
        assert!(rendered.contains("inert_noop"), "{rendered}");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_disabled_trigger_is_rejected(pool: PgPool) {
        sqlx::query(
            "ALTER TABLE credentials DISABLE TRIGGER credentials_not_tenant_wide_with_grants",
        )
        .execute(&pool)
        .await
        .unwrap();

        let error = crate::credentials::assert_schema_contract(&pool)
            .await
            .expect_err("a disabled trigger must fail the contract");
        let rendered = error
            .chain()
            .map(|cause| cause.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(rendered.contains("is not enabled"), "{rendered}");
        assert!(
            rendered.contains("tenant_wide and grants at once"),
            "{rendered}"
        );
    }

    // ── 0031: the trigger FUNCTIONS are pinned, not just the declarations ────

    /// Run the contract check and return the flattened failure, or `None`.
    async fn schema_failure(pool: &PgPool) -> Option<String> {
        match crate::credentials::assert_schema_contract(pool).await {
            Ok(()) => None,
            Err(error) => Some(
                error
                    .chain()
                    .map(|cause| cause.to_string())
                    .collect::<Vec<_>>()
                    .join(" | "),
            ),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_migrated_functions_satisfy_the_contract(pool: PgPool) {
        // The control. Without it every rejection below could be passing
        // because the checker rejects everything it is shown.
        assert_eq!(schema_failure(&pool).await, None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_trigger_function_with_a_no_op_body_is_rejected(pool: PgPool) {
        // `pg_get_triggerdef` proves the declaration — which table, which event,
        // which function name. It says nothing about what the function does, so
        // a same-named replacement that returns NEW satisfies every
        // declaration-level check while enforcing nothing.
        sqlx::query(
            "CREATE OR REPLACE FUNCTION public.credentials_reject_tenant_wide_with_grants() \
             RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER \
             SET search_path = pg_catalog, public AS $$ BEGIN RETURN NEW; END; $$",
        )
        .execute(&pool)
        .await
        .unwrap();

        // It enforces nothing: the forbidden transition now succeeds.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;
        grant(&pool, cred, tenant, target).await.unwrap();
        sqlx::query("UPDATE credentials SET tenant_wide = TRUE WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("the no-op body permits the contradictory state");

        let failure = schema_failure(&pool).await.expect("must be rejected");
        assert!(failure.contains("has an unexpected body"), "{failure}");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_altered_predicate_is_rejected(pool: PgPool) {
        // Subtler than a no-op: the structure is intact and only the condition
        // is inverted, so it refuses the writes it should allow and allows the
        // one it should refuse.
        sqlx::query(
            "CREATE OR REPLACE FUNCTION public.credentials_reject_tenant_wide_with_grants() \
             RETURNS TRIGGER LANGUAGE plpgsql SECURITY INVOKER \
             SET search_path = pg_catalog, public AS $$ \
             BEGIN IF NEW.tenant_wide THEN RETURN NEW; END IF; RETURN NEW; END; $$",
        )
        .execute(&pool)
        .await
        .unwrap();

        let failure = schema_failure(&pool).await.expect("must be rejected");
        assert!(failure.contains("has an unexpected body"), "{failure}");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_security_definer_replacement_is_rejected(pool: PgPool) {
        sqlx::query(
            "ALTER FUNCTION public.credential_agent_grants_reject_tenant_wide() SECURITY DEFINER",
        )
        .execute(&pool)
        .await
        .unwrap();

        let failure = schema_failure(&pool).await.expect("must be rejected");
        assert!(failure.contains("is SECURITY DEFINER"), "{failure}");
        assert!(failure.contains("definer's privileges"), "{failure}");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unexpected_function_configuration_is_rejected(pool: PgPool) {
        // A different search_path on the function changes how the names in its
        // body resolve, which is the whole reason it is pinned.
        sqlx::query(
            "ALTER FUNCTION public.credentials_reject_tenant_wide_with_grants() \
             SET search_path = public",
        )
        .execute(&pool)
        .await
        .unwrap();
        let failure = schema_failure(&pool).await.expect("must be rejected");
        assert!(failure.contains("has configuration"), "{failure}");

        // Removing it entirely is reported differently, and just as firmly.
        sqlx::query("ALTER FUNCTION public.credentials_reject_tenant_wide_with_grants() RESET ALL")
            .execute(&pool)
            .await
            .unwrap();
        let failure = schema_failure(&pool).await.expect("must be rejected");
        assert!(failure.contains("has no `SET search_path`"), "{failure}");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_function_in_the_wrong_schema_is_rejected(pool: PgPool) {
        // Reported as *misplaced* rather than merely missing — the difference
        // matters when a shadow schema is what put it there.
        sqlx::query("CREATE SCHEMA elsewhere")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE FUNCTION elsewhere.credential_agent_grants_reject_tenant_wide() \
             RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("DROP FUNCTION public.credential_agent_grants_reject_tenant_wide() CASCADE")
            .execute(&pool)
            .await
            .unwrap();

        let failure = schema_failure(&pool).await.expect("must be rejected");
        assert!(failure.contains("does not exist in `public`"), "{failure}");
        assert!(failure.contains("elsewhere"), "{failure}");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_non_volatile_replacement_is_rejected(pool: PgPool) {
        // `ALTER FUNCTION … STABLE` moves nothing else this contract inspects:
        // same body, same configuration, same SECURITY INVOKER, same signature,
        // same trigger definition. What it changes is *when* the function sees
        // data — which is the whole basis of the locking argument below.
        sqlx::query("ALTER FUNCTION public.credentials_reject_tenant_wide_with_grants() STABLE")
            .execute(&pool)
            .await
            .unwrap();

        let failure = schema_failure(&pool).await.expect("must be rejected");
        assert!(
            failure.contains("is STABLE, expected VOLATILE"),
            "{failure}"
        );
    }

    /// Drive the interleaving the `FOR SHARE`/`FOR UPDATE` pairing exists for:
    /// a grant commits while a `false -> true` UPDATE is already waiting on the
    /// credential's row lock. Returns what that UPDATE did.
    async fn transition_racing_a_grant(
        pool: &PgPool,
        cred: Uuid,
        tenant: Uuid,
        target: Uuid,
    ) -> Result<(), sqlx::Error> {
        let mut inserter = pool.acquire().await.unwrap();
        sqlx::raw_sql("BEGIN")
            .execute(&mut *inserter)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO public.credential_agent_grants (credential_id, tenant_id, agent_uuid) \
             VALUES ($1, $2, $3)",
        )
        .bind(cred)
        .bind(tenant)
        .bind(target)
        .execute(&mut *inserter)
        .await
        .unwrap();

        // The UPDATE takes FOR NO KEY UPDATE on the same row, which conflicts
        // with the FOR SHARE the grant trigger took and still holds, so it
        // blocks until the insert commits.
        let updater = {
            let pool = pool.clone();
            tokio::spawn(async move {
                sqlx::query("UPDATE public.credentials SET tenant_wide = TRUE WHERE id = $1")
                    .bind(cred)
                    .execute(&pool)
                    .await
                    .map(|_| ())
            })
        };

        // Wait for it to actually be blocked rather than sleeping a guess. If
        // it never blocks, the interleaving under test did not happen and the
        // result would mean nothing either way — so say so instead of scoring
        // it.
        let mut blocked = false;
        for _ in 0..200 {
            let (waiting,): (i64,) = sqlx::query_as(
                "SELECT count(*) FROM pg_stat_activity \
                  WHERE datname = current_database() AND wait_event_type = 'Lock' \
                    AND pid <> pg_backend_pid()",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting > 0 {
                blocked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            blocked,
            "the UPDATE never waited on the row lock; the race under test did not occur"
        );

        sqlx::raw_sql("COMMIT")
            .execute(&mut *inserter)
            .await
            .unwrap();
        drop(inserter);
        updater.await.unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_non_volatile_trigger_function_misses_a_concurrent_grant(pool: PgPool) {
        // Why volatility is worth pinning at all. PL/pgSQL runs a non-VOLATILE
        // function read-only, so its queries reuse the calling statement's
        // snapshot instead of taking a fresh one. The UPDATE that waited behind
        // the grant's row lock then looks for grants as of *before* that grant
        // committed, finds none, and lets the contradiction through — with a
        // body, a configuration and a trigger definition that all still match.
        let tenant = Uuid::new_v4();

        // Shipped and VOLATILE: it takes a fresh snapshot on waking, sees the
        // grant that committed while it waited, and refuses.
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-volatile").await;
        let refused = transition_racing_a_grant(&pool, cred, tenant, target).await;
        assert!(
            format!("{refused:?}").contains("tenant_wide cannot be set"),
            "VOLATILE must refuse the racing transition: {refused:?}"
        );

        // STABLE: same statement, same body, wrong answer.
        sqlx::query("ALTER FUNCTION public.credentials_reject_tenant_wide_with_grants() STABLE")
            .execute(&pool)
            .await
            .unwrap();

        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-stable").await;
        transition_racing_a_grant(&pool, cred, tenant, target)
            .await
            .expect("STABLE reuses the waiting statement's snapshot and misses the grant");

        // …leaving exactly the state the trigger exists to prevent.
        let (contradictory,): (bool,) = sqlx::query_as(
            "SELECT c.tenant_wide AND EXISTS (SELECT 1 FROM public.credential_agent_grants g \
              WHERE g.credential_id = c.id) FROM public.credentials c WHERE c.id = $1",
        )
        .bind(cred)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            contradictory,
            "the contradiction was really created, not merely permitted in principle"
        );
    }

    // ── 0031: rollbacks resolve to public regardless of search_path ──────────

    #[sqlx::test(migrations = "./migrations")]
    async fn the_rollbacks_ignore_a_hostile_search_path(pool: PgPool) {
        // Every rollback runs as `psql -f` in whatever session an operator
        // happens to have. If a schema ahead of `public` can capture the names
        // in those scripts, a rollback becomes an arbitrary-object-deletion
        // primitive — and the function drops are the sharpest edge, since they
        // named no schema at all before this change.
        let tenant = Uuid::new_v4();
        let cred = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;
        grant(&pool, cred, tenant, target).await.unwrap();

        sqlx::query("CREATE SCHEMA shadow")
            .execute(&pool)
            .await
            .unwrap();
        for ddl in [
            "CREATE TABLE shadow._sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT)",
            "INSERT INTO shadow._sqlx_migrations VALUES (28,'shadow'),(30,'shadow'),(31,'shadow')",
            "CREATE TABLE shadow.credential_agent_grants (marker TEXT)",
            "INSERT INTO shadow.credential_agent_grants VALUES ('untouched')",
            "CREATE TABLE shadow.credentials (marker TEXT)",
            "INSERT INTO shadow.credentials VALUES ('untouched')",
            "CREATE TABLE shadow.agents (marker TEXT)",
            "INSERT INTO shadow.agents VALUES ('untouched')",
            "CREATE TABLE shadow.agent_tenancy_migrations (marker TEXT)",
            "CREATE FUNCTION shadow.credentials_reject_tenant_wide_with_grants() \
             RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$",
            "CREATE FUNCTION shadow.credential_agent_grants_reject_tenant_wide() \
             RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$",
            "CREATE FUNCTION shadow.agents_bridge_identity_columns() \
             RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$",
        ] {
            sqlx::query(ddl).execute(&pool).await.unwrap();
        }

        // One connection, shadow first, all three scripts in order.
        let mut conn = pool.acquire().await.unwrap();
        sqlx::raw_sql("SET search_path = shadow, public")
            .execute(&mut *conn)
            .await
            .unwrap();
        for (label, script) in [
            ("0031", MIGRATION_0031_DOWN),
            ("0030", MIGRATION_0030_DOWN),
            ("0028", MIGRATION_0028_DOWN),
        ] {
            sqlx::raw_sql(script)
                .execute(&mut *conn)
                .await
                .unwrap_or_else(|e| panic!("{label} rollback under a hostile search_path: {e}"));
        }
        drop(conn);

        // The intended public objects are gone…
        for relation in [
            "public.credential_agent_grants",
            "public.agent_tenancy_migrations",
        ] {
            let (present,): (bool,) = sqlx::query_as("SELECT to_regclass($1) IS NOT NULL")
                .bind(relation)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert!(!present, "{relation} should have been dropped");
        }
        let (public_ledger,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM public._sqlx_migrations WHERE version IN (28, 30, 31)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(public_ledger, 0, "the public ledger rows were cleared");

        // …and nothing in `shadow` was touched. Spelled out rather than built
        // with `format!`: sqlx refuses a dynamic query string, and rightly so.
        for (relation, query) in [
            (
                "shadow.credential_agent_grants",
                "SELECT count(*) FROM shadow.credential_agent_grants WHERE marker = 'untouched'",
            ),
            (
                "shadow.credentials",
                "SELECT count(*) FROM shadow.credentials WHERE marker = 'untouched'",
            ),
            (
                "shadow.agents",
                "SELECT count(*) FROM shadow.agents WHERE marker = 'untouched'",
            ),
        ] {
            let (count,): (i64,) = sqlx::query_as(query)
                .fetch_one(&pool)
                .await
                .unwrap_or_else(|e| panic!("{relation} should still exist: {e}"));
            assert_eq!(count, 1, "{relation} was modified");
        }
        let (shadow_ledger,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM shadow._sqlx_migrations WHERE version IN (28, 30, 31)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(shadow_ledger, 3, "the shadow ledger must be untouched");

        let (shadow_functions,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
              WHERE n.nspname = 'shadow'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            shadow_functions, 3,
            "all three shadow functions must survive; the drops name public explicitly"
        );

        assert!(cred != target, "fixtures were distinct");
    }

    // ── 0031: ordering, rename and round trip ───────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn the_0030_rollback_refuses_until_0031_is_unwound(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let error = sqlx::raw_sql(MIGRATION_0030_DOWN)
            .execute(&mut *conn)
            .await
            .expect_err("0030's rollback must refuse while 0031 is applied");
        sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await.unwrap();
        drop(conn);

        let rendered = error.to_string();
        assert!(rendered.contains("0031"), "{rendered}");
        assert!(
            rendered.contains("rollback/0031_credential_agent_grants_hardening_down.sql"),
            "the error must name the script to run first: {rendered}"
        );

        // Nothing was dropped by the refusal.
        let (present,): (bool,) =
            sqlx::query_as("SELECT to_regclass('public.credential_agent_grants') IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(present);

        // In the right order it succeeds.
        sqlx::raw_sql(MIGRATION_0031_DOWN)
            .execute(&pool)
            .await
            .expect("0031 rollback");
        sqlx::raw_sql(MIGRATION_0030_DOWN)
            .execute(&pool)
            .await
            .expect("0030 rollback, once 0031 is gone");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_rename_round_trips_through_0031_and_back(pool: PgPool) {
        let column = |name: &'static str| {
            let pool = pool.clone();
            async move {
                let (present,): (bool,) = sqlx::query_as(
                    "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
                      WHERE table_schema = 'public' AND table_name = 'credential_agent_grants' \
                        AND column_name = $1)",
                )
                .bind(name)
                .fetch_one(&pool)
                .await
                .unwrap();
                present
            }
        };

        assert!(column("agent_uuid").await, "0031 renamed it");
        assert!(!column("agent_id").await);

        // Down: back to the 0030 name. The name is the reversible half.
        sqlx::raw_sql(MIGRATION_0031_DOWN)
            .execute(&pool)
            .await
            .expect("0031 rollback");
        assert!(
            column("agent_id").await,
            "the rollback restores 0030's name"
        );
        assert!(!column("agent_uuid").await);

        let (config,): (Option<String>,) = sqlx::query_as(
            "SELECT array_to_string(p.proconfig, ',') FROM pg_proc p \
               JOIN pg_namespace n ON n.oid = p.pronamespace \
              WHERE n.nspname = 'public' \
                AND p.proname = 'credentials_reject_tenant_wide_with_grants'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            config.as_deref(),
            Some("search_path=pg_catalog, public"),
            "the security half is forward-only: the rollback must not unpin \
             search_path, because a supported rollback path is not a supported \
             way to re-arm a fixed vulnerability"
        );

        // Up again: re-applying 0031 is idempotent and lands back on agent_uuid.
        for attempt in 1..=2 {
            sqlx::raw_sql(MIGRATION_0031)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("re-applying 0031 (attempt {attempt}): {e}"));
        }
        assert!(column("agent_uuid").await);
        assert_eq!(
            schema_failure(&pool).await,
            None,
            "and the contract holds again"
        );
    }

    // ── 0031: the security half is forward-only ─────────────────────────────
    //
    // The down script reverses the rename and the ledger row, and nothing else.
    // An earlier revision also restored 0030's function bodies and then reset
    // their configuration, which made a supported rollback path a way to re-arm
    // two fixed defects: the unconditional trigger that blocks revocation, and
    // the unqualified, unpinned bodies a caller's search_path can redirect.
    //
    // Note what these tests can and cannot use after the rollback. The column
    // is `agent_id` again, so `grant`, `revoke_grant` and `authorize_agent` —
    // which are compiled against `agent_uuid` — no longer apply, and everything
    // below reaches for raw SQL instead. That is the intended shape: a build
    // that requires 0031 does not run against a schema unwound to 0030, and
    // `store::verify_schema` is what stops it.

    /// `(security_invoker, proconfig, body)` for one trigger function in
    /// `public`. Panics if it is not there, which is itself the first thing
    /// worth knowing after a rollback.
    async fn function_facts(pool: &PgPool, name: &str) -> (bool, Option<String>, String) {
        sqlx::query_as(
            "SELECT NOT p.prosecdef, array_to_string(p.proconfig, ','), p.prosrc \
               FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
              WHERE n.nspname = 'public' AND p.proname = $1 AND p.pronargs = 0",
        )
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("public.{name} must survive the rollback: {e}"))
    }

    /// How many of the two grant trigger functions exist in `public`.
    async fn trigger_function_count(pool: &PgPool) -> i64 {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
              WHERE n.nspname = 'public' \
                AND p.proname IN ('credentials_reject_tenant_wide_with_grants', \
                                  'credential_agent_grants_reject_tenant_wide')",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        count
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_0031_rollback_reverts_the_name_and_the_ledger_only(pool: PgPool) {
        sqlx::raw_sql(MIGRATION_0031_DOWN)
            .execute(&pool)
            .await
            .expect("0031 rollback");

        // 1. The naming correction is reversed — that half genuinely is
        //    reversible, and rollback/0030 keys its ordering guard on it.
        let (renamed_back,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
              WHERE table_schema = 'public' AND table_name = 'credential_agent_grants' \
                AND column_name = 'agent_id')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(renamed_back, "agent_uuid must come back as agent_id");

        // 2. And the ledger row, so sqlx will re-apply 0031 rather than skip it.
        let (ledger,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM public._sqlx_migrations WHERE version = 31")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(ledger, 0, "the version-31 row must be removed");

        for (name, qualified) in [
            (
                "credentials_reject_tenant_wide_with_grants",
                "FROM public.credential_agent_grants g",
            ),
            (
                "credential_agent_grants_reject_tenant_wide",
                "FROM public.credentials c",
            ),
        ] {
            let (invoker, config, body) = function_facts(&pool, name).await;

            // 9. SECURITY INVOKER survives.
            assert!(invoker, "public.{name} must not become SECURITY DEFINER");

            // 10. …and so does the pinned search_path.
            assert_eq!(
                config.as_deref(),
                Some("search_path=pg_catalog, public"),
                "public.{name} must keep 0031's pinned search_path"
            );

            // …and the qualification it exists to back up. Both together:
            // neither is load-bearing alone.
            assert!(
                body.contains(qualified),
                "public.{name} must keep naming public explicitly: {body}"
            );
        }

        // 3. The transition arm in particular — the line 0030 did not have, and
        //    the one whose absence blocks revocation.
        let (_, _, body) =
            function_facts(&pool, "credentials_reject_tenant_wide_with_grants").await;
        assert!(
            body.contains("TG_OP = 'UPDATE'") && body.contains("OLD.tenant_wide"),
            "the transition-aware arm must survive the rollback: {body}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_transition_aware_trigger_still_governs_after_the_0031_rollback(pool: PgPool) {
        let tenant = Uuid::new_v4();
        let (cred, target) = contradictory(&pool, tenant).await;

        sqlx::raw_sql(MIGRATION_0031_DOWN)
            .execute(&pool)
            .await
            .expect("0031 rollback");

        // 4/5. The writes 0030's body refused. Restoring that body here — which
        //      is exactly what the earlier revision of the down script did —
        //      fails on the first of the three.
        sqlx::query("UPDATE public.credentials SET last_used_at = NOW() WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("maintenance must survive the rollback");
        sqlx::query("UPDATE public.credentials SET status = 'disabled' WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("disabling must survive the rollback");
        sqlx::query(
            "UPDATE public.credentials SET status = 'revoked', revoked_at = NOW() WHERE id = $1",
        )
        .bind(cred)
        .execute(&pool)
        .await
        .expect("revocation must survive the rollback");

        // 6. The repair path is still open.
        sqlx::query("UPDATE public.credentials SET tenant_wide = FALSE WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("clearing tenant_wide must still repair rather than be refused");

        // 3/7. …and the rule the trigger exists for is still armed: this is not
        //      a trigger that permits everything.
        let error = sqlx::query("UPDATE public.credentials SET tenant_wide = TRUE WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect_err("re-entering the contradictory state must still be refused");
        assert!(
            format!("{error:?}").contains("tenant_wide cannot be set"),
            "{error:?}"
        );

        // …and lifts once the grants are gone. Raw SQL: the column is `agent_id`
        // again, so `revoke_grant` does not apply here.
        sqlx::query(
            "DELETE FROM public.credential_agent_grants \
              WHERE credential_id = $1 AND agent_id = $2",
        )
        .bind(cred)
        .bind(target)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE public.credentials SET tenant_wide = TRUE WHERE id = $1")
            .bind(cred)
            .execute(&pool)
            .await
            .expect("with no grants left, tenant_wide is a legal choice");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_hostile_search_path_cannot_redirect_the_triggers_after_the_rollback(pool: PgPool) {
        // 8. The pinned `search_path` is the half most easily lost, because
        //    nothing observable breaks when it goes: the triggers still fire,
        //    still have the right names, and simply consult whichever tables
        //    the caller points them at.
        let tenant = Uuid::new_v4();
        let guarded = credential(&pool, tenant, false).await;
        let target = agent(&pool, tenant, "ext-a").await;
        grant(&pool, guarded, tenant, target).await.unwrap();
        let wide = credential(&pool, tenant, true).await;
        let other = agent(&pool, tenant, "ext-b").await;

        sqlx::raw_sql(MIGRATION_0031_DOWN)
            .execute(&pool)
            .await
            .expect("0031 rollback");

        // A shadow schema whose contents flip both answers if either function
        // resolves its relations through the session search_path: an empty
        // grants table (so the credentials trigger would see no grants to
        // conflict with) and an empty credentials table (so the grants trigger
        // would read NULL for tenant_wide and wave the grant through).
        for ddl in [
            "CREATE SCHEMA shadow",
            "CREATE TABLE shadow.credential_agent_grants (credential_id UUID, agent_id UUID)",
            "CREATE TABLE shadow.credentials (id UUID, tenant_wide BOOLEAN)",
        ] {
            sqlx::query(ddl).execute(&pool).await.unwrap();
        }

        let mut conn = pool.acquire().await.unwrap();
        sqlx::raw_sql("SET search_path = shadow, public")
            .execute(&mut *conn)
            .await
            .unwrap();

        // The credentials-side trigger still reads public.
        let error = sqlx::query("UPDATE public.credentials SET tenant_wide = TRUE WHERE id = $1")
            .bind(guarded)
            .execute(&mut *conn)
            .await
            .expect_err("the function's pinned search_path must win over the session's");
        assert!(
            format!("{error:?}").contains("tenant_wide cannot be set"),
            "{error:?}"
        );

        // The grants-side trigger too.
        let error = sqlx::query(
            "INSERT INTO public.credential_agent_grants (credential_id, tenant_id, agent_id) \
             VALUES ($1, $2, $3)",
        )
        .bind(wide)
        .bind(tenant)
        .bind(other)
        .execute(&mut *conn)
        .await
        .expect_err("the grants-side function must read public.credentials, not shadow's");
        assert!(format!("{error:?}").contains("is tenant_wide"), "{error:?}");

        // Falsification. With 0030's bodies — unqualified and unpinned, exactly
        // what the earlier rollback restored — the same two statements succeed,
        // because `credential_agent_grants` and `credentials` now mean the
        // shadow tables. The assertions above are not passing by accident.
        sqlx::query(
            "CREATE OR REPLACE FUNCTION public.credential_agent_grants_reject_tenant_wide() \
             RETURNS TRIGGER LANGUAGE plpgsql AS $$ DECLARE w BOOLEAN; BEGIN \
             SELECT c.tenant_wide INTO w FROM credentials c WHERE c.id = NEW.credential_id; \
             IF w THEN RAISE EXCEPTION 'is tenant_wide'; END IF; RETURN NEW; END; $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO public.credential_agent_grants (credential_id, tenant_id, agent_id) \
             VALUES ($1, $2, $3)",
        )
        .bind(wide)
        .bind(tenant)
        .bind(other)
        .execute(&mut *conn)
        .await
        .expect("0030's grants-side body resolves through the session search_path — the defect");

        sqlx::query(
            "CREATE OR REPLACE FUNCTION public.credentials_reject_tenant_wide_with_grants() \
             RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN \
             IF NEW.tenant_wide \
                AND EXISTS (SELECT 1 FROM credential_agent_grants g \
                             WHERE g.credential_id = NEW.id) \
             THEN RAISE EXCEPTION 'tenant_wide cannot be set'; END IF; RETURN NEW; END; $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE public.credentials SET tenant_wide = TRUE WHERE id = $1")
            .bind(guarded)
            .execute(&mut *conn)
            .await
            .expect("0030's credentials-side body resolves through the session search_path too");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn re_running_the_0031_rollback_creates_nothing(pool: PgPool) {
        // 11. The concrete failure of the earlier revision: its
        //     `CREATE OR REPLACE FUNCTION` calls ran unconditionally, so a pass
        //     after 0030 had already been unwound put both 0030 functions back
        //     on a database that should have had neither. A rollback that
        //     resurrects objects is not a rollback.
        assert_eq!(trigger_function_count(&pool).await, 2);

        for attempt in 1..=3 {
            sqlx::raw_sql(MIGRATION_0031_DOWN)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("0031 rollback attempt {attempt}: {e}"));
        }
        assert_eq!(
            trigger_function_count(&pool).await,
            2,
            "re-runs neither drop nor duplicate the hardened functions"
        );

        // Now the state that regressed: 0030 unwound as well, so nothing should
        // exist — and running 0031's rollback again must keep it that way.
        sqlx::raw_sql(MIGRATION_0030_DOWN)
            .execute(&pool)
            .await
            .expect("0030 rollback");
        assert_eq!(trigger_function_count(&pool).await, 0);

        for attempt in 1..=2 {
            sqlx::raw_sql(MIGRATION_0031_DOWN)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("post-0030 0031 rollback attempt {attempt}: {e}"));
        }
        assert_eq!(
            trigger_function_count(&pool).await,
            0,
            "the rollback must not resurrect 0030's functions"
        );

        let (present,): (bool,) =
            sqlx::query_as("SELECT to_regclass('public.credential_agent_grants') IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!present, "and it must not resurrect the table either");
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
                "FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id) ON DELETE CASCADE",
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
