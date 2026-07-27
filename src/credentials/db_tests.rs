//! Database-backed tests for the credential registry.
//!
//! Split into its own module so CI's no-database job can skip the whole set
//! with `--skip credentials::db_tests`, matching `tenancy::db_tests`.
//!
//! Every test gets its own database from `#[sqlx::test]`, so the ones that drop
//! tables or remove constraints cannot affect anything else.

use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::cache::TestClock;
use super::*;

const MIGRATION_0029: &str = include_str!("../../migrations/0029_credentials.sql");

fn pepper() -> Pepper {
    Pepper::from_hex(&"ab".repeat(32)).expect("64 hex characters is a valid pepper")
}

/// Anchored to real time rather than a fixed epoch: `credentials_expires_ck`
/// compares `expires_at` against a server-generated `created_at`, so a test
/// clock parked in the past could not write an expiry at all. Only *relative*
/// movement matters, and [`TestClock`] still supplies that deterministically.
fn start() -> DateTime<Utc> {
    Utc::now()
}

fn authenticator(pool: &PgPool, clock: &Arc<TestClock>) -> Authenticator {
    Authenticator::new(pool.clone(), pepper(), clock.clone(), DEFAULT_CAPACITY)
}

fn spec(tenant_id: Uuid, scopes: &[&str]) -> IssueSpec<'static> {
    IssueSpec {
        tenant_id,
        principal_id: "svc-ingest",
        mode: CredentialMode::V2Required,
        scopes: ScopeSet::parse_list(scopes).expect("test scopes must parse"),
        tenant_wide: false,
        expires_at: None,
    }
}

/// Revoke without going through [`Authenticator::revoke`], so the local cache
/// is *not* invalidated.
///
/// This is the honest scenario for the 30-second bound: a revocation performed
/// by another replica or by an operator's SQL. Revoking through the
/// authenticator would drop the cache entry and prove nothing about the TTL.
async fn revoke_elsewhere(pool: &PgPool, id: Uuid) {
    sqlx::query("UPDATE credentials SET status = 'revoked', revoked_at = NOW() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("out-of-band revocation");
}

// ── The happy path ───────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_valid_credential_authenticates(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let tenant = Uuid::new_v4();

    let issued = auth
        .issue(spec(tenant, &["memory:read", "admin"]))
        .await
        .expect("issuance");

    let outcome = auth.authenticate(issued.presented()).await;
    let ctx = outcome
        .result
        .expect("a freshly issued credential must authenticate");

    assert_eq!(ctx.credential_id(), issued.credential_id);
    assert_eq!(ctx.tenant_id(), tenant);
    assert_eq!(ctx.principal_id(), "svc-ingest");
    assert_eq!(ctx.mode(), CredentialMode::V2Required);
    assert!(ctx.allows(Scope::MemoryRead));
    assert_eq!(outcome.audit.outcome, AuthOutcome::Authenticated);
    assert_eq!(outcome.audit.tenant_id, Some(tenant));
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_does_not_imply_memory_export_through_a_database_round_trip(pool: PgPool) {
    // The A-5 property proven end to end, not only over an in-memory ScopeSet:
    // storage, retrieval and parsing all preserve it.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);

    let issued = auth
        .issue(spec(Uuid::new_v4(), &["admin"]))
        .await
        .expect("issuance");
    let ctx = auth
        .authenticate(issued.presented())
        .await
        .result
        .expect("authentication");

    assert!(ctx.allows(Scope::Admin));
    assert!(!ctx.allows(Scope::MemoryExport));
    assert!(!ctx.allows(Scope::MemoryRead));
    assert!(!ctx.allows(Scope::MemoryHistory));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_reserved_scope_survives_storage_but_never_authorises(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);

    let issued = auth
        .issue(spec(Uuid::new_v4(), &["proxy:chat"]))
        .await
        .expect("proxy:chat is canonical and therefore storable");
    let ctx = auth
        .authenticate(issued.presented())
        .await
        .result
        .expect("authentication");

    assert!(!ctx.allows(Scope::ProxyChat), "Path B is not enabled (A-3)");
    assert!(ctx.scopes().holds_reserved(Scope::ProxyChat));
}

// ── Rejections ───────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_malformed_credential_fails(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);

    let cases = [
        String::new(),
        "no-separator".to_string(),
        format!("not-a-uuid.{}", "ab".repeat(32)),
        format!("{}.short", Uuid::new_v4()),
        format!("{}.{}", Uuid::new_v4(), "zz".repeat(32)),
    ];

    for raw in cases {
        let outcome = auth.authenticate(&raw).await;
        let error = outcome.result.expect_err("must be rejected");
        assert_eq!(error.reason(), FailureReason::Malformed, "for {raw:?}");
        assert_eq!(error.to_string(), "invalid credential");
        assert!(
            outcome.audit.credential_id.is_none(),
            "nothing parsed, so nothing to attribute"
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn an_unknown_credential_fails_and_runs_the_decoy_mac(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);

    assert_eq!(auth.dummy_mac_count(), 0);

    let unknown = format!("{}.{}", Uuid::new_v4(), "cd".repeat(32));
    let error = auth
        .authenticate(&unknown)
        .await
        .result
        .expect_err("no such credential");

    assert_eq!(error.reason(), FailureReason::UnknownCredential);
    assert_eq!(
        auth.dummy_mac_count(),
        1,
        "an indexed miss must still pay for one HMAC, or its cost betrays which \
         credential ids exist"
    );

    // Every subsequent miss pays too — not just the first.
    for expected in 2..=4 {
        let unknown = format!("{}.{}", Uuid::new_v4(), "cd".repeat(32));
        let _ = auth.authenticate(&unknown).await;
        assert_eq!(auth.dummy_mac_count(), expected);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_real_credential_does_not_run_the_decoy(pool: PgPool) {
    // The counter would be useless as evidence if it incremented on every call.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);

    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");
    let _ = auth.authenticate(issued.presented()).await;

    assert_eq!(auth.dummy_mac_count(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_altered_secret_fails(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);

    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    let (id, secret) = issued.presented().split_once('.').unwrap();
    let mut chars: Vec<char> = secret.chars().collect();
    chars[0] = if chars[0] == 'a' { 'b' } else { 'a' };
    let altered: String = chars.into_iter().collect();

    let error = auth
        .authenticate(&format!("{id}.{altered}"))
        .await
        .result
        .expect_err("a one-character change must fail");
    assert_eq!(error.reason(), FailureReason::BadSecret);

    // …and is indistinguishable from an unknown credential to the caller.
    let unknown = auth
        .authenticate(&format!("{}.{}", Uuid::new_v4(), "cd".repeat(32)))
        .await
        .result
        .expect_err("unknown");
    assert_eq!(error.to_string(), unknown.to_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_credential_from_another_pepper_fails(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    // Same database, different pepper: a stolen database is not enough.
    let other = Authenticator::new(
        pool.clone(),
        Pepper::from_hex(&"cd".repeat(32)).unwrap(),
        clock.clone(),
        DEFAULT_CAPACITY,
    );
    let error = other
        .authenticate(issued.presented())
        .await
        .result
        .expect_err("the MAC was computed under a different key");
    assert_eq!(error.reason(), FailureReason::BadSecret);
}

// ── Expiry and status ────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn an_expired_credential_fails(pool: PgPool) {
    let now = start();
    let clock = Arc::new(TestClock::new(now));
    let auth = authenticator(&pool, &clock);

    let mut s = spec(Uuid::new_v4(), &["memory:read"]);
    s.expires_at = Some(now + ChronoDuration::seconds(10));
    let issued = auth.issue(s).await.expect("issuance");

    assert!(
        auth.authenticate(issued.presented()).await.result.is_ok(),
        "usable before its expiry"
    );

    // Past the expiry — and past the cache TTL, so the record is re-read.
    clock.advance(std::time::Duration::from_secs(31));
    let error = auth
        .authenticate(issued.presented())
        .await
        .result
        .expect_err("expired");
    assert_eq!(error.reason(), FailureReason::Expired);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_disabled_credential_fails(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    sqlx::query("UPDATE credentials SET status = 'disabled' WHERE id = $1")
        .bind(issued.credential_id)
        .execute(&pool)
        .await
        .unwrap();

    let error = auth
        .authenticate(issued.presented())
        .await
        .result
        .expect_err("disabled");
    assert_eq!(error.reason(), FailureReason::Inactive);
}

// ── Revocation and the 30-second bound ───────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_revoked_credential_fails_within_the_thirty_second_bound(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    // Warm the cache, then revoke out of band.
    assert!(auth.authenticate(issued.presented()).await.result.is_ok());
    revoke_elsewhere(&pool, issued.credential_id).await;

    // Inside the window the cached record is still honoured. That is the
    // documented cost of caching, stated rather than hidden.
    clock.advance(std::time::Duration::from_secs(29));
    assert!(
        auth.authenticate(issued.presented()).await.result.is_ok(),
        "within the bound, the cached record is still served"
    );

    // At the bound it must stop. Measured, not assumed.
    clock.advance(std::time::Duration::from_secs(1));
    let error = auth
        .authenticate(issued.presented())
        .await
        .result
        .expect_err("revocation must take effect within 30 s");
    assert_eq!(error.reason(), FailureReason::Revoked);
    assert_eq!(auth.cache_ttl(), MAX_TTL);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_continuously_used_revoked_credential_still_fails_within_thirty_seconds(pool: PgPool) {
    // Plan test #28. Under a sliding TTL this credential would authenticate
    // forever, because every second's request would reset the deadline.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    assert!(auth.authenticate(issued.presented()).await.result.is_ok());
    revoke_elsewhere(&pool, issued.credential_id).await;

    // Hammer it, the way an attacker holding a stolen credential would.
    for _ in 1..30 {
        clock.advance(std::time::Duration::from_secs(1));
        let _ = auth.authenticate(issued.presented()).await;
    }

    clock.advance(std::time::Duration::from_secs(1));
    let error = auth
        .authenticate(issued.presented())
        .await
        .result
        .expect_err("29 intervening uses must not have extended the entry");
    assert_eq!(error.reason(), FailureReason::Revoked);
}

#[sqlx::test(migrations = "./migrations")]
async fn revoking_through_the_authenticator_takes_effect_immediately(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    assert!(auth.authenticate(issued.presented()).await.result.is_ok());
    assert!(auth.revoke(issued.credential_id).await.expect("revoke"));

    // No clock advance: local invalidation short-circuits the TTL.
    let error = auth
        .authenticate(issued.presented())
        .await
        .result
        .expect_err("revoked in this process");
    assert_eq!(error.reason(), FailureReason::Revoked);

    // Idempotent.
    assert!(
        !auth
            .revoke(issued.credential_id)
            .await
            .expect("revoke again"),
        "re-revoking changes nothing and is not an error"
    );
}

// ── The cache ────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_cached_credential_is_served_without_the_database(pool: PgPool) {
    // Proves the cache is actually consulted: with the table gone, only a
    // cached record can answer.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");
    assert!(auth.authenticate(issued.presented()).await.result.is_ok());

    sqlx::query("DROP TABLE credentials")
        .execute(&pool)
        .await
        .unwrap();

    assert!(
        auth.authenticate(issued.presented()).await.result.is_ok(),
        "served from cache"
    );

    clock.advance(std::time::Duration::from_secs(31));
    let error = auth
        .authenticate(issued.presented())
        .await
        .result
        .expect_err("the entry expired and the table is gone");
    assert_eq!(error.reason(), FailureReason::Backend);
}

// ── Fail closed ──────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_database_error_fails_closed(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    // A fresh authenticator has an empty cache, so this reaches the database.
    let cold = authenticator(&pool, &clock);
    sqlx::query("DROP TABLE credentials")
        .execute(&pool)
        .await
        .unwrap();

    let error = cold
        .authenticate(issued.presented())
        .await
        .result
        .expect_err("an unavailable store must reject, never admit");
    assert_eq!(error.reason(), FailureReason::Backend);
    assert!(
        error.is_backend(),
        "callers need to answer 503 rather than 401 here"
    );
    assert_eq!(error.to_string(), "invalid credential");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_row_this_build_cannot_interpret_fails_closed(pool: PgPool) {
    // A newer writer stores a mode this build has never heard of. Reading it as
    // anything at all would be guessing at a capability boundary.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    sqlx::query("ALTER TABLE credentials DROP CONSTRAINT credentials_mode_ck")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE credentials SET mode = 'mode_from_the_future' WHERE id = $1")
        .bind(issued.credential_id)
        .execute(&pool)
        .await
        .unwrap();

    let cold = authenticator(&pool, &clock);
    let error = cold
        .authenticate(issued.presented())
        .await
        .result
        .expect_err("an uninterpretable row must not authenticate");
    assert_eq!(error.reason(), FailureReason::Corrupt);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_unknown_stored_scope_fails_closed_rather_than_being_dropped(pool: PgPool) {
    // Silently discarding an unrecognised scope would *narrow* the credential
    // without telling anyone, which is a different credential than the one that
    // was issued.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    sqlx::query("ALTER TABLE credentials DROP CONSTRAINT credentials_scopes_ck")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE credentials SET scopes = ARRAY['memory:read','memory:teleport'] WHERE id = $1",
    )
    .bind(issued.credential_id)
    .execute(&pool)
    .await
    .unwrap();

    let cold = authenticator(&pool, &clock);
    let error = cold
        .authenticate(issued.presented())
        .await
        .result
        .expect_err("an unknown scope must fail the whole record");
    assert_eq!(error.reason(), FailureReason::Corrupt);
}

// ── Tenancy ──────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn the_tenant_comes_only_from_the_credential_record(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);

    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let cred_a = auth.issue(spec(tenant_a, &["memory:read"])).await.unwrap();
    let cred_b = auth.issue(spec(tenant_b, &["memory:read"])).await.unwrap();

    // `authenticate` takes one argument: the credential string. There is no
    // parameter through which a caller could propose a tenant, and no field on
    // AeonAuthContext that could be set from a body — see the compile-time
    // probes in `context.rs`.
    let ctx_a = auth
        .authenticate(cred_a.presented())
        .await
        .result
        .expect("tenant A");
    let ctx_b = auth
        .authenticate(cred_b.presented())
        .await
        .result
        .expect("tenant B");

    assert_eq!(ctx_a.tenant_id(), tenant_a);
    assert_eq!(ctx_b.tenant_id(), tenant_b);
    assert_ne!(ctx_a.tenant_id(), ctx_b.tenant_id());

    // Rewriting the stored tenant changes the authenticated tenant — the row is
    // the only source.
    sqlx::query("UPDATE credentials SET tenant_id = $2 WHERE id = $1")
        .bind(cred_a.credential_id)
        .bind(tenant_b)
        .execute(&pool)
        .await
        .unwrap();

    let cold = authenticator(&pool, &clock);
    let moved = cold
        .authenticate(cred_a.presented())
        .await
        .result
        .expect("still a valid credential");
    assert_eq!(moved.tenant_id(), tenant_b);
}

// ── Secret hygiene ───────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn the_plaintext_secret_never_appears_in_a_persisted_row(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    let secret_hex = issued.presented().split_once('.').unwrap().1.to_string();

    // Render the entire row — every column, including bytea — as text.
    let (rendered,): (String,) =
        sqlx::query_as("SELECT c::text FROM credentials c WHERE c.id = $1")
            .bind(issued.credential_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    assert!(
        !rendered.contains(&secret_hex),
        "the plaintext secret must not be recoverable from the row"
    );
    assert!(!rendered.contains(issued.presented()));
    // What *is* stored is the MAC, and it is not the secret.
    assert!(rendered.contains(&hex::encode(issued.secret_mac)));
}

#[sqlx::test(migrations = "./migrations")]
async fn the_plaintext_secret_never_appears_in_an_error_or_an_audit_record(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");
    let secret_hex = issued.presented().split_once('.').unwrap().1.to_string();

    // A success and a failure, both inspected.
    let ok = auth.authenticate(issued.presented()).await;
    let bad = auth
        .authenticate(&format!("{}.{}", issued.credential_id, "ff".repeat(32)))
        .await;

    for outcome in [&ok, &bad] {
        let audit = format!("{:?}", outcome.audit);
        assert!(!audit.contains(&secret_hex), "audit leaked the secret");
        assert!(!audit.contains(issued.presented()));
        assert!(!audit.to_lowercase().contains("pepper"));
    }

    let error = bad.result.expect_err("wrong secret");
    assert!(!error.to_string().contains(&secret_hex));
    assert!(!format!("{error:?}").contains(&secret_hex));

    // The context that *is* handed to a handler carries no secret either.
    let ctx = ok.result.expect("valid");
    assert!(!format!("{ctx:?}").contains(&secret_hex));
}

// ── Migration ────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn the_migration_is_idempotent(pool: PgPool) {
    // Every statement is IF NOT EXISTS and every constraint is inline, so
    // re-applying the file must be a no-op rather than a duplicate-object error.
    for attempt in 1..=2 {
        sqlx::raw_sql(MIGRATION_0029)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("re-applying 0029 (attempt {attempt}) failed: {e}"));
    }

    // …and the table still works afterwards.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance after re-application");
    assert!(auth.authenticate(issued.presented()).await.result.is_ok());
}

#[sqlx::test(migrations = "./migrations")]
async fn the_credentials_table_carries_every_frozen_constraint(pool: PgPool) {
    let expected = [
        "credentials_mode_ck",
        "credentials_status_ck",
        "credentials_secret_mac_len_ck",
        "credentials_principal_id_ck",
        "credentials_revoked_ck",
        "credentials_expires_ck",
        "credentials_scopes_ck",
        // Step 3's credential_agent_grants FK target (plan §2.2).
        "credentials_id_tenant_id_key",
    ];
    for name in expected {
        let (present,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint \
              WHERE conrelid = 'credentials'::regclass AND conname = $1)",
        )
        .bind(name)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(present, "constraint {name} is missing");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn the_database_refuses_states_the_design_calls_incoherent(pool: PgPool) {
    let tenant = Uuid::new_v4();

    let insert_with = |status: &'static str, revoked: bool| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO credentials (id, tenant_id, principal_id, secret_mac, mode, status, revoked_at) \
                 VALUES ($1, $2, 'p', $3, 'v2_required', $4, CASE WHEN $5 THEN NOW() ELSE NULL END)",
            )
            .bind(Uuid::new_v4())
            .bind(tenant)
            .bind(vec![0u8; 32])
            .bind(status)
            .bind(revoked)
            .execute(&pool)
            .await
        }
    };

    assert!(
        insert_with("revoked", false).await.is_err(),
        "revoked without a timestamp must be unrepresentable"
    );
    assert!(
        insert_with("active", true).await.is_err(),
        "a revocation timestamp on an active row must be unrepresentable"
    );
    assert!(insert_with("active", false).await.is_ok());

    // A short MAC is a weakened MAC.
    let short = sqlx::query(
        "INSERT INTO credentials (id, tenant_id, principal_id, secret_mac, mode) \
         VALUES ($1, $2, 'p', $3, 'v2_required')",
    )
    .bind(Uuid::new_v4())
    .bind(tenant)
    .bind(vec![0u8; 16])
    .execute(&pool)
    .await;
    assert!(short.is_err(), "a 16-byte MAC must be rejected");

    // An invented scope cannot be written at all.
    let bad_scope = sqlx::query(
        "INSERT INTO credentials (id, tenant_id, principal_id, secret_mac, mode, scopes) \
         VALUES ($1, $2, 'p', $3, 'v2_required', ARRAY['memory:teleport'])",
    )
    .bind(Uuid::new_v4())
    .bind(tenant)
    .bind(vec![0u8; 32])
    .execute(&pool)
    .await;
    assert!(bad_scope.is_err(), "a non-canonical scope must be rejected");
}

#[sqlx::test(migrations = "./migrations")]
async fn the_agent_identity_migration_remains_valid(pool: PgPool) {
    // Step 1 (merged fe756a6) is what step 3's grant table will reference. This
    // migration must not have disturbed it.
    for name in [
        "agents_tenant_id_external_agent_id_key",
        "agents_tenant_id_id_key",
    ] {
        let (present,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint \
              WHERE conrelid = 'agents'::regclass AND conname = $1)",
        )
        .bind(name)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(present, "step 1 constraint {name} is missing");
    }

    // And its columns still hold the shape step 1 established.
    for column in ["id", "tenant_id", "external_agent_id", "agent_id"] {
        let (present,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
              WHERE table_name = 'agents' AND column_name = $1)",
        )
        .bind(column)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(present, "agents.{column} is missing");
    }
}

// ── Startup gate ─────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn multi_tenant_enablement_is_refused_against_an_empty_registry(pool: PgPool) {
    // Plan test #31.
    let error = assert_registry_ready_for_multi_tenant(&pool, true)
        .await
        .expect_err("an empty registry must refuse multi-tenant enablement");
    let rendered = error.to_string();
    assert!(rendered.contains("no active credentials"), "{rendered}");
    assert!(
        rendered.contains("no implicit administrator"),
        "the error must rule out an implicit admin: {rendered}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_registry_gate_is_inert_while_multi_tenant_mode_is_off(pool: PgPool) {
    // The V1 default. An empty registry is completely normal here.
    assert_registry_ready_for_multi_tenant(&pool, false)
        .await
        .expect("must not refuse while multi-tenant mode is off");
}

#[sqlx::test(migrations = "./migrations")]
async fn one_active_credential_satisfies_the_startup_gate(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["admin"]))
        .await
        .expect("issuance");

    assert_registry_ready_for_multi_tenant(&pool, true)
        .await
        .expect("one active credential is enough");

    // …and a registry of only revoked credentials is an empty one.
    auth.revoke(issued.credential_id).await.unwrap();
    assert!(
        assert_registry_ready_for_multi_tenant(&pool, true)
            .await
            .is_err(),
        "revoked credentials do not count towards the precondition"
    );
}

// ── Attribution ──────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn last_used_is_recorded_on_a_cache_miss_and_not_on_every_request(pool: PgPool) {
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    let last_used = |id: Uuid| {
        let pool = pool.clone();
        async move {
            let (value,): (Option<DateTime<Utc>>,) =
                sqlx::query_as("SELECT last_used_at FROM credentials WHERE id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            value
        }
    };

    assert!(last_used(issued.credential_id).await.is_none());

    assert!(auth.authenticate(issued.presented()).await.result.is_ok());
    let first = last_used(issued.credential_id)
        .await
        .expect("a cache miss records attribution");

    // A cache hit deliberately does not write: this is a "seen around then"
    // marker, not a per-request audit log, and writing on every request would
    // turn authentication into a write path.
    clock.advance(std::time::Duration::from_secs(1));
    assert!(auth.authenticate(issued.presented()).await.result.is_ok());
    assert_eq!(last_used(issued.credential_id).await, Some(first));
}
