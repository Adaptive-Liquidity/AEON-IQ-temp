# Agent identity migration

Tenant-scoped agent identity, step 1 of the AEON authentication & tenancy plan
(§10 step 1). The plan is accepted at `Adaptive-Liquidity/nexus-planning`
@ `b1fe06505d400e435c3ef8d10dc197f15641bebd`; the AEON baseline it was written
against is `35e77badae1bab79e5bd543a3bda26afd7e0767b`.

Implemented by `migrations/0028_agent_tenancy_identity.sql` and `src/tenancy.rs`.

## What this step establishes

| Object | State after 0028 |
|---|---|
| `agents.id` | UUID primary key. **Already existed** at the baseline (`0001_initial.sql:11`); the plan's §3 table marks it "(new)", which is not accurate at commit `35e77ba`. Every existing agent therefore already had a stable UUID and none had to be generated. |
| `agents.tenant_id` | New `UUID`, nullable. `NULL` is the explicit *unmapped* state. |
| `agents.external_agent_id` | New `TEXT NOT NULL`, backfilled verbatim from `agent_id`. |
| `agents.agent_id` | **Unchanged**, still `TEXT UNIQUE NOT NULL`. |
| `UNIQUE (tenant_id, external_agent_id)` | New — `agents_tenant_id_external_agent_id_key`. |
| `UNIQUE (tenant_id, id)` | New — `agents_tenant_id_id_key`. The FK target §2.2 and §4.1 need. |
| `agent_tenancy_migrations` | New record table: `mode`, `legacy_tenant_id`, `agents_total`, `agents_assigned`, `agents_unmapped`, `applied_at TIMESTAMPTZ`. |

Nothing else changes. No dependent table is migrated, no credential or grant
table exists yet, no endpoint authorization is altered, and multi-tenant mode
cannot be turned on.

## The exact later step that removes the global `agent_id` uniqueness

**Plan §4 migration-order step 7, executed as §10 step 7 — "Constraint
tightening + old-column drop. Irreversible; last."**

That step, and no earlier one, drops:

- `agents_agent_id_key`, the global `UNIQUE` on `agents.agent_id` created inline
  by `0001_initial.sql:12`; and
- the `agents.agent_id` column itself.

It cannot happen before **§10 step 4** (the additive dependent-table migration),
because two foreign keys still target that constraint:

| Referencing column | Declared at | Clause |
|---|---|---|
| `sessions.agent_id` | `0001_initial.sql:23` | `REFERENCES agents(agent_id) ON DELETE CASCADE` |
| `archival_batches.agent_id` | `0006_archival_versioning.sql:10` | `REFERENCES agents(agent_id) ON DELETE CASCADE` |

PostgreSQL requires a unique index on the referenced columns, so dropping
`agents_agent_id_key` while either FK exists fails. §10 step 4 repoints both at
`agents(id)`; only then is step 7 applicable.

Two further tightenings belong to the same step-7 window and are listed here so
they are not rediscovered later:

1. **`agents.tenant_id` becomes `NOT NULL`** (plan §6's target model). Possible
   only once no row is unmapped — which the enablement gate already requires.
2. **The `agents_bridge_identity_columns` trigger is dropped** together with
   `agents.agent_id`, since it exists only to keep the legacy column populated.

Until then, `external_agent_id` and `agent_id` diverge for any agent created
through the tenant-aware path — see *Legacy compatibility key* below.

## Legacy compatibility key

Two tenants may now hold the same `external_agent_id`. The retained global
`UNIQUE` on `agent_id` cannot represent that, so rows created through
`src/tenancy.rs::insert_agent` put the agent's **own UUID** in the legacy
column:

```
agent_id = agents.id::text
```

It is deliberately **not** derived from `external_agent_id`. `agents.agent_id`
is arbitrary text, so any scheme that embeds the caller-supplied identifier can
be made to collide: a legacy V1 agent may already be named exactly the string
such a scheme would generate, and an otherwise-valid
`(tenant_id, external_agent_id)` pair would then be rejected by
`agents_agent_id_key`. Deriving the key from the row's own UUID puts it outside
caller control, and keeps the tenant UUID out of a field that `GET /agents`
currently returns.

Rows written through the unchanged V1 path keep their identifier **verbatim** —
`INSERT INTO agents (agent_id) VALUES ($1)` still produces
`agent_id = external_agent_id = $1`, mirrored by the bridge trigger. The
compatibility key applies only to rows V1 could not have created in the first
place, and the rollback script restores `agent_id` from `external_agent_id`
before dropping the column. The whole mechanism disappears at step 7 with the
column it exists to feed.

## Migration modes

Frozen as decision A-2. **The operator must select one; there is no default and
no implicit tenant assignment.** With `LEGACY_MIGRATION_MODE` unset, the backfill
does not run at all and every agent stays unmapped.

### Mode 1 — single-tenant assignment

```bash
LEGACY_MIGRATION_MODE=single_tenant
LEGACY_TENANT_ID=11111111-1111-1111-1111-111111111111
```

Every agent with no tenant is assigned `LEGACY_TENANT_ID`. Startup fails if
`LEGACY_TENANT_ID` is absent or is not a UUID.

### Mode 2 — mapped migration

```bash
LEGACY_MIGRATION_MODE=mapped
LEGACY_TENANT_MAP='{"support-bot":"11111111-1111-1111-1111-111111111111"}'
# or
LEGACY_TENANT_MAP_FILE=/etc/aeon/tenant-map.json
```

Only agents named in the map are assigned. **Unmapped agents keep
`tenant_id IS NULL` and are served to nobody** — not to a legacy tenant, not to
an admin, not to a default. Setting both the inline map and the file is an error,
so the effective mapping is never ambiguous.

### Why unmapped is `NULL` rather than a sentinel

`NULL` never satisfies `WHERE tenant_id = $1`, so an unmapped agent is
unreachable from every tenant-scoped query by construction. A sentinel UUID
would instead be a real, claimable tenant value — exactly the implicit default
assignment the plan forbids. The plan's §6 note that `NULL` makes rows "vanish
from every tenant-scoped query" describes the intended behaviour for unmapped
rows, and its `NOT NULL` target model is reached at step 7, once nothing is
unmapped.

### Idempotency and mode changes

The backfill only ever writes rows where `tenant_id IS NULL`, so re-running it
with the same plan assigns nothing and adds no record row. A run whose mode or
declared tenant disagrees with the recorded migration is **rejected**, as is a
mapped run whose map contradicts an already-assigned agent. Changing your mind
means rolling back first.

## Multi-tenant enablement gate

`MULTI_TENANT_ENABLED=true` makes startup **refuse** unless all of the following
hold (plan §6):

1. a migration mode is set;
2. a migration record exists and matches the configured mode and tenant;
3. no agent is unmapped;
4. the management API is not unauthenticated — `MANAGEMENT_API_KEY` unset
   together with `ALLOW_UNAUTH_MANAGEMENT=true` leaves every `/api/v1/*` route
   open, because `auth::check_management_key` performs no check at all when no
   key is configured (`auth.rs:22`);
5. `MANAGEMENT_API_KEY` is no longer accepted — legacy-key retirement (§10
   step 6) must complete first, because that key authorises every route and has
   no tenant of its own.

Conditions 4 and 5 are separate on purpose. Removing the key satisfies 5 but not
4, and treating "no legacy key" as retirement would let the gate bless a
multi-tenant deployment with no authentication whatsoever. Retirement counts
only once a replacement exists, and the replacement is the credential registry
of §10 steps 2–3.

With `MULTI_TENANT_ENABLED` unset or false — the default — the gate returns
immediately and changes nothing.

## Rollback

```bash
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 \
  -f rollback/0028_agent_tenancy_identity_down.sql
```

Everything in step 1 is additive, so this restores the 0027 baseline without
losing an identifier: `agents.agent_id`, its global `UNIQUE`, both dependent
foreign keys and every agent row are untouched by both the migration and the
rollback. What is discarded is the tenant assignments and the record rows, which
the same operator inputs reproduce exactly.

Rows created through the tenant-aware path need one extra step, which the script
performs first: their `agent_id` is the UUID compatibility key, so the script
moves `external_agent_id` back into `agent_id` before dropping the column.
Without that, the agent would come back reachable only by a UUID it never
advertised.

That rename cannot be done in isolation. `sessions.agent_id` and
`archival_batches.agent_id` reference the value being changed, and both are
`ON UPDATE NO ACTION` — PostgreSQL's default, which neither declaration
overrides — so renaming the parent while a child still points at the old value
is rejected outright. The script therefore drops those foreign keys, repoints
the children, renames the parents, and restores each constraint from
`pg_get_constraintdef`, so the baseline definition comes back exactly as it was.

Where the baseline genuinely cannot hold the data — two tenants sharing one
`external_agent_id`, exactly what the global `UNIQUE` forbids — the script
**raises and changes nothing** rather than picking a winner.

None of this is reachable in a step-1 deployment, because `insert_agent` is not
yet wired into request handling; the script short-circuits when no identifier
has moved. It matters from §10 step 5 onward.

The script also deletes the `_sqlx_migrations` row for version 28 so
`sqlx migrate run` re-applies 0028 cleanly afterwards.

> Reversibility ends at §10 step 7. Everything up to and including step 4 is
> additive by design (plan §4, "Rollback": keep legacy columns for at least one
> release).

## Tests

`src/tenancy.rs` holds both suites:

- `mod tests` — configuration parsing, no database. Runs in every CI job.
- `mod db_tests` — `#[sqlx::test]` against a pgvector Postgres, one isolated
  database per test. Skipped by the no-database CI job via
  `--skip tenancy::db_tests`, and run in full by the `integration` job.
