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
async fn the_decoy_is_charged_to_every_database_visit_and_only_those(pool: PgPool) {
    // The decoy now equalises *database visits*, not just misses. Charging it
    // only to a miss would leave a one-HMAC asymmetry in the other direction,
    // since the real MAC is computed before the lookup either way.
    //
    // That also makes the counter a direct probe for "did this request reach
    // PostgreSQL?", which is what the oracle tests below measure with.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");
    assert_eq!(auth.dummy_mac_count(), 0, "issuance is not a lookup");

    // Cold hit: reaches the database.
    assert!(auth.authenticate(issued.presented()).await.result.is_ok());
    assert_eq!(auth.dummy_mac_count(), 1);

    // Warm hit with the correct secret: served from cache, no visit.
    assert!(auth.authenticate(issued.presented()).await.result.is_ok());
    assert_eq!(
        auth.dummy_mac_count(),
        1,
        "a cache hit must not touch the database"
    );

    // Miss: reaches the database.
    let unknown = format!("{}.{}", Uuid::new_v4(), "cd".repeat(32));
    assert!(auth.authenticate(&unknown).await.result.is_err());
    assert_eq!(auth.dummy_mac_count(), 2);

    // Malformed: rejected before any lookup, so no visit and no decoy.
    assert!(auth.authenticate("not-a-credential").await.result.is_err());
    assert_eq!(auth.dummy_mac_count(), 2);
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

// ── Schema contract ──────────────────────────────────────────────────────────

/// Run the contract check and return the rendered failure, or `None` if it
/// passed. The whole error chain is flattened because `assert_schema_contract`
/// wraps the detail in context.
async fn schema_failure(pool: &PgPool) -> Option<String> {
    match assert_schema_contract(pool).await {
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
async fn the_migrated_schema_satisfies_the_contract(pool: PgPool) {
    // The control case. Without it, every rejection test below could be passing
    // because the checker rejects everything.
    assert_eq!(
        schema_failure(&pool).await,
        None,
        "the schema migration 0029 produces must satisfy its own contract"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_missing_primary_key_is_rejected(pool: PgPool) {
    // The drift that `CREATE TABLE IF NOT EXISTS` would wave through. Without a
    // primary key, `id` need not identify one row.
    sqlx::query("ALTER TABLE credentials DROP CONSTRAINT credentials_pkey CASCADE")
        .execute(&pool)
        .await
        .unwrap();

    let failure = schema_failure(&pool).await.expect("must be rejected");
    assert!(failure.contains("PRIMARY KEY"), "{failure}");
    assert!(
        failure.contains("duplicate credential id"),
        "the error should say why it matters: {failure}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_missing_mac_length_constraint_is_rejected(pool: PgPool) {
    sqlx::query("ALTER TABLE credentials DROP CONSTRAINT credentials_secret_mac_len_ck")
        .execute(&pool)
        .await
        .unwrap();

    let failure = schema_failure(&pool).await.expect("must be rejected");
    assert!(
        failure.contains("credentials_secret_mac_len_ck` is missing"),
        "{failure}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_unvalidated_constraint_is_rejected(pool: PgPool) {
    // NOT VALID is the subtle one: the constraint exists and `pg_constraint`
    // lists it, but it was never checked against existing rows — so a
    // 16-byte MAC written before it was added is still sitting there.
    sqlx::query("ALTER TABLE credentials DROP CONSTRAINT credentials_secret_mac_len_ck")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "ALTER TABLE credentials ADD CONSTRAINT credentials_secret_mac_len_ck \
         CHECK (octet_length(secret_mac) = 32) NOT VALID",
    )
    .execute(&pool)
    .await
    .unwrap();

    let failure = schema_failure(&pool).await.expect("must be rejected");
    assert!(failure.contains("NOT VALID"), "{failure}");
    assert!(failure.contains("never verified"), "{failure}");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_missing_composite_uniqueness_is_rejected(pool: PgPool) {
    sqlx::query("ALTER TABLE credentials DROP CONSTRAINT credentials_id_tenant_id_key")
        .execute(&pool)
        .await
        .unwrap();

    let failure = schema_failure(&pool).await.expect("must be rejected");
    assert!(failure.contains("UNIQUE (id, tenant_id)"), "{failure}");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_wrong_column_type_or_nullability_is_rejected(pool: PgPool) {
    sqlx::query("ALTER TABLE credentials ALTER COLUMN principal_id TYPE VARCHAR(64)")
        .execute(&pool)
        .await
        .unwrap();
    let failure = schema_failure(&pool)
        .await
        .expect("type drift must be rejected");
    assert!(
        failure.contains("`principal_id` has type `character varying`"),
        "{failure}"
    );

    sqlx::query("ALTER TABLE credentials ALTER COLUMN tenant_id DROP NOT NULL")
        .execute(&pool)
        .await
        .unwrap();
    let failure = schema_failure(&pool)
        .await
        .expect("nullability drift must be rejected");
    assert!(
        failure.contains("`tenant_id` is nullable, expected NOT NULL"),
        "{failure}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_missing_column_or_index_is_rejected(pool: PgPool) {
    sqlx::query("DROP INDEX idx_credentials_active")
        .execute(&pool)
        .await
        .unwrap();
    let failure = schema_failure(&pool).await.expect("must be rejected");
    assert!(
        failure.contains("index `idx_credentials_active` is missing"),
        "{failure}"
    );

    sqlx::query("ALTER TABLE credentials DROP COLUMN tenant_wide")
        .execute(&pool)
        .await
        .unwrap();
    let failure = schema_failure(&pool).await.expect("must be rejected");
    assert!(
        failure.contains("column `tenant_wide` is missing"),
        "{failure}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_absent_table_is_rejected_rather_than_assumed(pool: PgPool) {
    // Also covers "validation could not be performed": an unverifiable schema
    // is treated exactly like a wrong one.
    sqlx::query("DROP TABLE credentials")
        .execute(&pool)
        .await
        .unwrap();

    let failure = schema_failure(&pool).await.expect("must be rejected");
    assert!(failure.contains("does not exist"), "{failure}");
    assert!(
        failure.contains("silently accepts a pre-existing table"),
        "the context should explain why this check exists: {failure}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn every_reported_problem_is_listed_at_once(pool: PgPool) {
    // An operator repairing a drifted schema should see the whole list in one
    // startup attempt, not discover it one restart at a time.
    sqlx::query("ALTER TABLE credentials DROP CONSTRAINT credentials_mode_ck")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE credentials DROP CONSTRAINT credentials_status_ck")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP INDEX idx_credentials_tenant_principal")
        .execute(&pool)
        .await
        .unwrap();

    let failure = schema_failure(&pool).await.expect("must be rejected");
    assert!(failure.contains("credentials_mode_ck"), "{failure}");
    assert!(failure.contains("credentials_status_ck"), "{failure}");
    assert!(
        failure.contains("idx_credentials_tenant_principal"),
        "{failure}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_weakened_pre_existing_table_survives_the_migration_and_is_caught_here(pool: PgPool) {
    // The end-to-end drift scenario, not a synthetic one: drop the migrated
    // table, put a plausible weakened one in its place, and re-apply 0029. The
    // migration is a no-op — `IF NOT EXISTS` sees a table named `credentials`
    // and skips the entire definition — so `0029` reports success against a
    // table with no primary key and no constraints at all.
    sqlx::query("DROP TABLE credentials")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE credentials ( \
             id UUID, tenant_id UUID, principal_id TEXT, secret_mac BYTEA, \
             mode TEXT, scopes TEXT[], tenant_wide BOOLEAN, status TEXT, \
             created_at TIMESTAMPTZ, last_used_at TIMESTAMPTZ, \
             revoked_at TIMESTAMPTZ, expires_at TIMESTAMPTZ)",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(MIGRATION_0029)
        .execute(&pool)
        .await
        .expect("re-applying 0029 over an existing table is a silent no-op");

    // Proof that the migration alone would have accepted this.
    let (present,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM pg_constraint \
          WHERE conrelid = 'credentials'::regclass AND contype = 'p')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        !present,
        "the weakened table still has no primary key after 0029 re-ran"
    );

    // The contract check is what catches it.
    let failure = schema_failure(&pool).await.expect("must be rejected");
    assert!(failure.contains("PRIMARY KEY"), "{failure}");
    assert!(failure.contains("credentials_mode_ck"), "{failure}");
    assert!(failure.contains("NOT NULL"), "{failure}");
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

// ── Nothing is written before the MAC verifies ───────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_wrong_secret_never_populates_the_cache(pool: PgPool) {
    // Regression test for a timing oracle that survived the decoy MAC.
    //
    // An earlier revision cached the record during the lookup, before the MAC
    // comparison. Probing a *known* id with a wrong secret therefore warmed the
    // cache and skipped PostgreSQL from the second probe onwards, while an
    // unknown id paid a round trip every time — so repeated probing separated
    // real ids from invented ones despite identical responses and an equalised
    // HMAC cost.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    let wrong = format!("{}.{}", issued.credential_id, "ff".repeat(32));
    for attempt in 1..=5 {
        let error = auth
            .authenticate(&wrong)
            .await
            .result
            .expect_err("wrong secret");
        assert_eq!(error.reason(), FailureReason::BadSecret, "probe {attempt}");
    }

    // If any probe had cached the record, the *correct* secret would now be
    // servable with no database at all. Take the table away and find out.
    sqlx::query("DROP TABLE credentials")
        .execute(&pool)
        .await
        .unwrap();

    let error = auth
        .authenticate(issued.presented())
        .await
        .result
        .expect_err("a wrong-secret probe must leave nothing cached to measure");
    assert_eq!(error.reason(), FailureReason::Backend);
}

#[sqlx::test(migrations = "./migrations")]
async fn only_a_fully_successful_authentication_advances_last_used(pool: PgPool) {
    // An unauthenticated probe must have no database-write side effect, and
    // `last_used_at` must mean "last used", not "last guessed at".
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

    let wrong = format!("{}.{}", issued.credential_id, "ff".repeat(32));
    assert!(auth.authenticate(&wrong).await.result.is_err());
    assert_eq!(
        last_used(issued.credential_id).await,
        None,
        "a wrong secret must not stamp last_used_at"
    );

    // A correct secret on a *revoked* credential does not count either: the
    // attempt was rejected, so it was not a use.
    revoke_elsewhere(&pool, issued.credential_id).await;
    let cold = authenticator(&pool, &clock);
    assert!(cold.authenticate(issued.presented()).await.result.is_err());
    assert_eq!(last_used(issued.credential_id).await, None);

    // A successful one does.
    sqlx::query("UPDATE credentials SET status = 'active', revoked_at = NULL WHERE id = $1")
        .bind(issued.credential_id)
        .execute(&pool)
        .await
        .unwrap();
    let fresh = authenticator(&pool, &clock);
    assert!(fresh.authenticate(issued.presented()).await.result.is_ok());
    assert!(last_used(issued.credential_id).await.is_some());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_warmed_cache_entry_does_not_serve_a_wrong_secret(pool: PgPool) {
    // The second half of the timing oracle, and the one that survived the first
    // fix. Caching only after a successful authentication stops an *attacker*
    // from warming the entry — but a legitimate request warms it just the same,
    // and after that any wrong-secret probe for the same id was answered from
    // memory while an unknown id still paid an indexed query.
    //
    // Measured through the decoy counter, which increments exactly once per
    // database visit: the two probes must cost the same number of visits.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    // A legitimate use warms the cache.
    assert!(auth.authenticate(issued.presented()).await.result.is_ok());
    let after_warm = auth.dummy_mac_count();
    assert!(
        auth.authenticate(issued.presented()).await.result.is_ok(),
        "the correct secret may still be served from the warm entry"
    );
    assert_eq!(
        auth.dummy_mac_count(),
        after_warm,
        "the holder of the correct secret is served from cache"
    );

    // Now probe the *same* id with a wrong secret, and an unknown id.
    let wrong = format!("{}.{}", issued.credential_id, "ff".repeat(32));
    let unknown = format!("{}.{}", Uuid::new_v4(), "ff".repeat(32));

    let before = auth.dummy_mac_count();
    assert!(auth.authenticate(&wrong).await.result.is_err());
    let warmed_id_visits = auth.dummy_mac_count() - before;

    let before = auth.dummy_mac_count();
    assert!(auth.authenticate(&unknown).await.result.is_err());
    let unknown_id_visits = auth.dummy_mac_count() - before;

    assert_eq!(
        warmed_id_visits, unknown_id_visits,
        "a warmed credential id must cost the same number of database visits as \
         an invented one, or the cache reintroduces the existence oracle"
    );
    assert_eq!(warmed_id_visits, 1, "both must actually reach the database");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_wrong_secret_for_a_warmed_id_still_reaches_the_database(pool: PgPool) {
    // The same property proven structurally rather than by counter: with the
    // table gone, only something served from cache can succeed.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");

    assert!(
        auth.authenticate(issued.presented()).await.result.is_ok(),
        "warm the cache"
    );

    sqlx::query("DROP TABLE credentials")
        .execute(&pool)
        .await
        .unwrap();

    // The correct secret still rides the live entry — the cache is doing its job.
    assert!(
        auth.authenticate(issued.presented()).await.result.is_ok(),
        "the correct secret may use the warm entry"
    );

    // The wrong secret must not. It has to consult PostgreSQL, which is gone.
    let wrong = format!("{}.{}", issued.credential_id, "ff".repeat(32));
    let error = auth
        .authenticate(&wrong)
        .await
        .result
        .expect_err("a wrong secret must not be answered from a warmed entry");
    assert_eq!(
        error.reason(),
        FailureReason::Backend,
        "reaching the database is the proof; a BadSecret here would mean the \
         cache answered and the oracle is still open"
    );

    // An unknown id behaves identically.
    let unknown = format!("{}.{}", Uuid::new_v4(), "ff".repeat(32));
    assert_eq!(
        auth.authenticate(&unknown)
            .await
            .result
            .expect_err("unknown")
            .reason(),
        FailureReason::Backend
    );

    // And the TTL is still absolute: the warm entry dies on schedule.
    clock.advance(std::time::Duration::from_secs(31));
    assert_eq!(
        auth.authenticate(issued.presented())
            .await
            .result
            .expect_err("the entry expired")
            .reason(),
        FailureReason::Backend
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_failed_cache_probe_does_not_disturb_the_entry(pool: PgPool) {
    // A wrong-secret probe must not refresh the entry (which would slide the
    // TTL) nor evict it (which would let an attacker force database load, or
    // deny the legitimate holder their cache).
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    let issued = auth
        .issue(spec(Uuid::new_v4(), &["memory:read"]))
        .await
        .expect("issuance");
    assert!(auth.authenticate(issued.presented()).await.result.is_ok());

    let wrong = format!("{}.{}", issued.credential_id, "ff".repeat(32));

    // Hammer the id with wrong secrets across almost the whole TTL window.
    for _ in 0..29 {
        clock.advance(std::time::Duration::from_secs(1));
        assert!(auth.authenticate(&wrong).await.result.is_err());
    }

    // Still cached: the correct secret is served without the database.
    sqlx::query("DROP TABLE credentials")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        auth.authenticate(issued.presented()).await.result.is_ok(),
        "29 failed probes must not have evicted the entry"
    );

    // …and did not extend it either.
    clock.advance(std::time::Duration::from_secs(1));
    assert_eq!(
        auth.authenticate(issued.presented())
            .await
            .result
            .expect_err("the absolute TTL still expires on schedule")
            .reason(),
        FailureReason::Backend
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_rejected_issuance_leaves_no_row(pool: PgPool) {
    // An expiry at or before `created_at` violates `credentials_expires_ck`.
    // With issuance as one statement the whole write fails and nothing is
    // committed. The earlier INSERT-then-UPDATE version would have left an
    // active, *non-expiring* row whose plaintext secret the caller had already
    // discarded — unusable by anyone, yet counted by the readiness gate.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);

    let mut s = spec(Uuid::new_v4(), &["memory:read"]);
    s.expires_at = Some(start() - ChronoDuration::hours(1));
    assert!(
        auth.issue(s).await.is_err(),
        "an expiry in the past must be refused"
    );

    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM credentials")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "a failed issuance must leave nothing behind");
}

#[sqlx::test(migrations = "./migrations")]
async fn the_startup_gate_does_not_count_expired_credentials(pool: PgPool) {
    // `status = 'active'` alone would call this registry ready even though
    // nobody in it can authenticate — the exact condition the gate exists to
    // catch. Written directly so `created_at` can be backdated far enough for
    // an already-elapsed expiry to satisfy `credentials_expires_ck`.
    sqlx::query(
        "INSERT INTO credentials \
             (id, tenant_id, principal_id, secret_mac, mode, created_at, expires_at) \
         VALUES ($1, $2, 'stale', $3, 'v2_required', \
                 NOW() - INTERVAL '2 hours', NOW() - INTERVAL '1 hour')",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(vec![0u8; 32])
    .execute(&pool)
    .await
    .expect("an active-but-expired row is representable");

    let error = assert_registry_ready_for_multi_tenant(&pool, true)
        .await
        .expect_err("an all-expired registry is an empty one");
    assert!(error.to_string().contains("no active credentials"));

    // One unexpired credential is enough to clear it.
    let clock = Arc::new(TestClock::new(start()));
    let auth = authenticator(&pool, &clock);
    auth.issue(spec(Uuid::new_v4(), &["admin"]))
        .await
        .expect("issuance");
    assert_registry_ready_for_multi_tenant(&pool, true)
        .await
        .expect("an unexpired active credential satisfies the gate");
}
