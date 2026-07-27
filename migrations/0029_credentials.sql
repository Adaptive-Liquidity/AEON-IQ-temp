-- ============================================================================
-- 0029  Credential registry  (AEON auth/tenancy plan §2, decision A-1)
-- ============================================================================
--
-- Step 2 of the plan accepted at
--   Adaptive-Liquidity/nexus-planning @ b1fe06505d400e435c3ef8d10dc197f15641bebd
-- and sequenced in §10 as "Credential registry — credentials (incl.
-- tenant_wide) + AeonAuthContext".
--
-- ADDITIVE AND INERT.  This migration creates one new table.  It alters no
-- existing table, drops nothing, and rewrites no data.  Nothing reads it until
-- an operator sets AEON_CREDENTIAL_AUTH_ENABLED=true, so applying it to a
-- running V1 deployment changes no behaviour.
--
-- What is deliberately NOT here (plan §10 steps 3 and 5):
--   * credential_agent_grants — needs this table's UNIQUE (id, tenant_id) as an
--     FK target, which is created below so step 3 stays additive.
--   * any per-route scope or grant enforcement.
--
-- Idempotency: every statement is IF NOT EXISTS and every constraint is inline
-- in the CREATE TABLE, so re-running the whole file is a no-op rather than a
-- duplicate-object error.  (Postgres has no ADD CONSTRAINT IF NOT EXISTS, which
-- is why the constraints are declared inline rather than added afterwards.)
-- ============================================================================

CREATE TABLE IF NOT EXISTS credentials (
    -- The public half of the presented credential "<id>.<secret>".  Not secret:
    -- it is an indexed lookup key, and knowing it proves nothing.
    id            UUID        PRIMARY KEY,

    -- The authoritative tenant.  AeonAuthContext.tenant_id is read from HERE
    -- and from nowhere else — never from a request body (plan §1).
    tenant_id     UUID        NOT NULL,

    -- Who or what the credential represents.  Opaque to AEON.
    principal_id  TEXT        NOT NULL,

    -- HMAC-SHA-256(pepper, secret).  Exactly 32 bytes, enforced below so a
    -- truncated MAC is unrepresentable rather than merely unexpected.
    --
    -- The pepper is NOT in this database (plan §2): a database-only compromise
    -- therefore yields no verifiable MACs and no way to mint one.
    --
    -- The plaintext secret is never stored here or anywhere else.  It exists in
    -- memory at issuance, is shown once, and is not recoverable afterwards.
    secret_mac    BYTEA       NOT NULL,

    -- Which recall endpoint the credential may use.  Enforcement is step 5.
    mode          TEXT        NOT NULL,

    -- Canonical scope tokens (plan A-5).  Constrained to the canonical set
    -- below; ordering and duplicate rejection are the application's job because
    -- a CHECK cannot express "no duplicates" without a subquery.
    scopes        TEXT[]      NOT NULL DEFAULT '{}',

    -- §2.2.  FALSE means "agent-restricted", which is the safe default: a
    -- credential is restricted unless someone deliberately says otherwise, so
    -- forgetting to add grants in step 3 denies rather than permits.
    --
    -- Inert in this migration.  Nothing reads it until credential_agent_grants
    -- exists (step 3), and it is deliberately NOT carried on AeonAuthContext —
    -- authentication establishes tenant, principal and scopes; agent authority
    -- is a separate step-3 decision.
    tenant_wide   BOOLEAN     NOT NULL DEFAULT FALSE,

    status        TEXT        NOT NULL DEFAULT 'active',

    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Best-effort attribution, updated at most once per cache miss rather than
    -- once per request, so it cannot become a write amplifier on a hot path.
    -- Treat it as "seen around then", not as an audit log.
    last_used_at  TIMESTAMPTZ,

    revoked_at    TIMESTAMPTZ,

    -- Not in the plan's minimum column list, but required by it: a credential
    -- has to be able to expire for "expired credential fails" to be testable at
    -- all.  NULL means "does not expire".
    expires_at    TIMESTAMPTZ,

    CONSTRAINT credentials_mode_ck
        CHECK (mode IN ('legacy_only', 'v2_required')),

    CONSTRAINT credentials_status_ck
        CHECK (status IN ('active', 'revoked', 'disabled')),

    -- HMAC-SHA-256 is 32 bytes.  Anything else is a bug or a truncation, and a
    -- short MAC is a weakened MAC, so make it unwritable.
    CONSTRAINT credentials_secret_mac_len_ck
        CHECK (octet_length(secret_mac) = 32),

    CONSTRAINT credentials_principal_id_ck
        CHECK (length(principal_id) BETWEEN 1 AND 255),

    -- Biconditional, not implication.  "status = revoked with no timestamp" and
    -- "revoked_at set while still active" are both incoherent states, and a
    -- revocation whose time is unknown cannot be reasoned about afterwards.
    CONSTRAINT credentials_revoked_ck
        CHECK ((status = 'revoked') = (revoked_at IS NOT NULL)),

    -- A credential that expires before it exists is a configuration error, and
    -- one that is born expired would authenticate nobody while looking valid.
    CONSTRAINT credentials_expires_ck
        CHECK (expires_at IS NULL OR expires_at > created_at),

    -- Membership in the canonical set (plan A-5).  `<@` is a plain operator, so
    -- unlike a subquery it is legal in a CHECK.  proxy:chat is listed because
    -- it is canonical-but-reserved: storable, and never satisfied by
    -- ScopeSet::allows until Path B (A-3) is explicitly enabled.
    CONSTRAINT credentials_scopes_ck
        CHECK (scopes <@ ARRAY[
            'recall:v2',
            'memory:read',
            'memory:write',
            'memory:export',
            'memory:history',
            'admin',
            'proxy:chat'
        ]::TEXT[]),

    -- Step 3's credential_agent_grants needs a composite FK target so a grant
    -- row's tenant_id is tied to the CREDENTIAL's tenant (plan §2.2).  Creating
    -- it here rather than in step 3 keeps step 3 purely additive.
    CONSTRAINT credentials_id_tenant_id_key UNIQUE (id, tenant_id)
);

-- Lookup by credential_id is served by the primary key: one index probe, then
-- one MAC verify.  No table scan and no per-row hashing (plan §2, "Lookup").

-- Administrative listing: "which credentials exist for this tenant/principal".
CREATE INDEX IF NOT EXISTS idx_credentials_tenant_principal
    ON credentials (tenant_id, principal_id);

-- Supports the §2 startup check ("registry empty AND multi-tenant enabled →
-- refuse to start", test #31) without scanning revoked rows.
CREATE INDEX IF NOT EXISTS idx_credentials_active
    ON credentials (tenant_id)
    WHERE status = 'active';

COMMENT ON TABLE credentials IS
    'AEON-owned credential registry (plan A-1). Stores HMAC-SHA-256(pepper, secret) only; '
    'the pepper lives outside this database and the plaintext secret is never persisted.';

COMMENT ON COLUMN credentials.secret_mac IS
    'HMAC-SHA-256(pepper, secret), 32 bytes. Not a password hash: the secret is 32 bytes of '
    'CSPRNG output, so there is no low-entropy input for a slow KDF to protect.';

COMMENT ON COLUMN credentials.tenant_wide IS
    'FALSE = agent-restricted (safe default). Inert until credential_agent_grants (plan step 3).';
