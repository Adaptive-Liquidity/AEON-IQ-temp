-- ============================================================================
-- 0032  Tenancy tranche 1 PREPARE  (AEON auth/tenancy plan §4, decision A-6)
-- ============================================================================
--
-- Step 4B-1 of the plan accepted at
--   Adaptive-Liquidity/nexus-planning @ b1fe06505d400e435c3ef8d10dc197f15641bebd
-- implementing TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN from the typed
-- contract in src/tenancy/plan.rs.  Every object below is declared there; this
-- file creates nothing the contract does not name.
--
-- ADDITIVE AND REVERSIBLE.  Nothing here drops, renames or repurposes an
-- existing column, constraint or index, and no row is rewritten.
-- `rollback/0032_tenancy_tranche1_prepare_down.sql` returns the schema to the
-- 0031 baseline without data loss.
--
-- This is the PREPARE stage only.  It is deliberately split from the concurrent
-- index builds (0033-0040, one statement per file) and the constraint
-- attachments (0041), because CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction block — implicit or explicit — while this file must be
-- transactional: the checkpoint table, the ownership columns and the bridge
-- triggers have to land together or not at all.
--
-- What is deliberately NOT here:
--   * NOT NULL on any ownership column — that is FUTURE STEP 7, which cannot
--     begin until plan steps 5 and 6 merge (src/tenancy/plan.rs, Tranche
--     ::FinalConstraintTightening).  The columns are added NULL-able and the
--     audit, not the foreign key, is the ownership gate during the transition.
--   * Any VALIDATE CONSTRAINT — that is FINALIZE, and it is gated on a
--     COMPLETED backfill checkpoint.  See finalize/tranche1_finalize.sql.
--   * Any backfill of existing rows — that is an operational command, not a
--     migration, precisely so it can be stopped and resumed.
--   * Row-level security.  RLS is deferred to architecture v2 (decision A-4)
--     and is not a guarantee this migration provides or relies on.
--
-- Idempotency: every statement is IF NOT EXISTS or CREATE OR REPLACE, so
-- re-running the whole file is a no-op rather than a duplicate-object error.
-- That is only safe because of the provenance guards immediately below: they
-- prove that anything this file is about to skip-or-replace is this file's own
-- earlier work, and refuse otherwise.
-- ============================================================================

-- ── Provenance: this tranche creates, or it refuses.  It never adopts. ──────
--
-- `ADD COLUMN IF NOT EXISTS`, `CREATE OR REPLACE FUNCTION` and `CREATE OR
-- REPLACE TRIGGER` are what make this file re-runnable, and each of them is
-- also a silent adoption of somebody else's object:
--
--   * A pre-existing `entities.tenant_id` is skipped by IF NOT EXISTS and then
--     dropped unconditionally by rollback/0032, destroying data this tranche
--     never created.  The column holds rows, so an identically-shaped column is
--     NOT interchangeable -- this is the one place where the tranche's usual
--     identity-by-shape rule is insufficient and provenance is required.
--   * `CREATE OR REPLACE FUNCTION` preserves the OID, so replacing a
--     pre-existing `fn_entities_tenancy_bridge()` silently changes the behaviour
--     of every trigger that already calls it, and rollback/0032 then drops it.
--   * `CREATE OR REPLACE TRIGGER` replaces a same-named trigger on the same
--     table, and rollback/0032 then drops that too.  Same defect, third object
--     kind; fixed here rather than left for the next reviewer to find.
--
-- The handle is `pg_description`.  Every object this file creates is stamped
-- with the marker below, in the same transaction that creates it, and
-- rollback/0032 destroys only objects carrying exactly that comment.  A comment
-- is keyed by (objoid, objsubid), so it survives ALTER TABLE RENAME of either
-- the table or the column -- which is more than a name-based handle manages.
-- It cannot be created by accident, so it yields no false adoption.  Stripping
-- it yields a false REFUSAL, which preserves data; that direction is the safe
-- one, and rollback/0032 says how to resolve it.
--
-- The three guards run before anything is created, so a refusal leaves the
-- schema untouched rather than half-built.
--
-- WHAT THE MARKER IS AND IS NOT.  It is a provenance handle: it stops this
-- migration adopting an object it did not create, and so stops rollback/0032
-- destroying data belonging to whatever did.  It is NOT authentication.  A role
-- that can create objects in `public` can also write the marker onto them, so
-- each guard additionally requires `pg_has_role(current_user, <owner>,
-- 'USAGE')` -- an object owned by a role this migration cannot vouch for is
-- refused whatever its comment says.  Note honestly that this ownership test is
-- VACUOUS when migrations run as a superuser, because a superuser is a member
-- of every role; where that is the deployment, the load-bearing control is
-- PostgreSQL 15+'s default of not granting CREATE on `public` to PUBLIC, not
-- anything in this file.
--
-- The marker text appears three times in the tranche: the guard below, the
-- stamping block at the foot of this file, and the guard in
-- rollback/0032_tenancy_tranche1_prepare_down.sql.  All three must stay
-- byte-identical; `the_provenance_marker_is_identical_everywhere_it_appears`
-- fails if they drift.

-- ── THE TRUSTED RESOLUTION PATH, PINNED FOR THE WHOLE FILE ──────────────────
--
-- Everything below resolves under `pg_catalog` alone.  This is transaction-local
-- and 0032 is a transactional migration (no `-- no-transaction` first line), so
-- PostgreSQL restores the caller's path automatically at COMMIT or ROLLBACK.
-- Nothing in this file restores it early, and nothing should: an early restore
-- is exactly what left the ownership columns and the bridge declarations exposed.
--
-- WHY A PIN AND NOT ONLY QUALIFICATION.  `pg_catalog` is searched first only
-- while it is IMPLICIT.  A caller who names it explicitly and late --
-- `hostile, public, pg_catalog` -- demotes it, and every unqualified identifier
-- becomes capturable.  Three separate captures were measured on pg16 against
-- earlier heads of this file:
--
--   * `hostile.gen_random_uuid()` bound into the checkpoint table's `id`
--     DEFAULT, on BOTH the live table and the canonical reference, so the
--     comparator agreed while both sides were wrong.  Second defaulted insert
--     died on the primary key.
--   * `CREATE DOMAIN hostile.uuid AS pg_catalog.uuid CHECK (VALUE IS NULL)`
--     captured all ten `ADD COLUMN ... UUID` declarations.  The nullable ADDs
--     succeed, 0032 applies clean, and the first real ownership assignment then
--     fails the domain constraint.
--   * `CREATE DOMAIN hostile.trigger` captured `RETURNS TRIGGER`, so the bridge
--     function was created returning the wrong type and `CREATE TRIGGER` then
--     refused it with "must return type trigger".
--
-- WHICH TYPE NAMES ARE ACTUALLY CAPTURABLE was measured rather than assumed,
-- because it decides what qualification is worth writing.  On pg16, with a
-- shadow schema ahead of an explicitly-listed `pg_catalog`:
--
--   CAPTURED  : uuid, text, timestamptz, oid, jsonb, date, "char", trigger
--   NOT CAPTURED (the grammar maps these straight to pg_catalog, so a shadow
--   of the same name is simply not consulted):
--               bigint, boolean, integer, varchar, numeric
--
-- So every capturable type name below is written qualified as well, and the
-- grammar-level ones are left in their conventional spelling deliberately --
-- qualifying `BIGINT` would suggest a protection that is not what protects it.
--
-- OPERATORS AND CASTS are search_path-resolved too, and there is no readable way
-- to qualify every `=` as `OPERATOR(pg_catalog.=)` across a file this size.  The
-- pin is what covers them, which is the other reason it is file-wide rather than
-- wrapped around individual statements.
--
-- Relation and created-object targets stay explicitly schema-qualified
-- regardless -- `public.<object>`, and the resolved temporary schema for the
-- canonical reference.  Neither the pin nor the qualification is load-bearing
-- alone.
--
-- WHY THE CALLER'S PATH IS HANDED BACK AT THE FOOT OF THIS FILE, and not left
-- to COMMIT.  A transaction-local `SET LOCAL` is undone when the transaction
-- ends -- but sqlx's migrator records this migration by running
-- `INSERT INTO _sqlx_migrations ...`, UNQUALIFIED, inside this very
-- transaction, after the last statement here and before COMMIT.  Measured: with
-- the path left pinned to `pg_catalog` alone, every migration-applying test
-- fails with `relation "_sqlx_migrations" does not exist` (SQLSTATE 42P01).
-- So the pin covers the whole file and is released only once there is no more
-- of this file to protect.  The caller's exact path is stashed in a
-- transaction-local custom GUC rather than assumed to be `public`, so a
-- deployment that runs migrations under some other schema gets its own path
-- back rather than one this file guessed.
SELECT pg_catalog.set_config('aeon.saved_search_path',
                             pg_catalog.current_setting('search_path'), true);
SET LOCAL search_path = pg_catalog;

DO $provenance_guard$
DECLARE
    -- Declared once.  Duplicated verbatim in the stamping block at the foot of
    -- this file and in rollback/0032_tenancy_tranche1_prepare_down.sql; the test
    -- `the_provenance_marker_is_identical_everywhere_it_appears` fails on drift.
    -- Left UNQUALIFIED deliberately -- the only declaration in this file that
    -- is.  `the_provenance_marker_is_identical_everywhere_it_appears` parses
    -- this declaration's exact prefix out of three files to prove the marker
    -- text has not drifted, and expects to find exactly three of them, so the
    -- prefix is not written anywhere else in this file -- not even in prose --
    -- and qualifying it here alone would break the parse outright.  It is a
    -- PL/pgSQL local whose type is never persisted, so the file-wide pin is the
    -- whole of its protection, and is sufficient.
    marker CONSTANT TEXT :=
        'AEON tenancy tranche 1 (migration 0032). Provenance marker: rollback/0032 '
        'destroys only objects carrying exactly this comment, and refuses to touch '
        'any that do not.';
    -- The idempotency stamp for the checkpoint table.  Deliberately NOT named
    -- `marker`: `the_provenance_marker_is_identical_everywhere_it_appears`
    -- matches on a `marker` declaration prefix and would otherwise read
    -- this as a fourth declaration of a string that differs from the other
    -- three.  Duplicated verbatim in the stamping block at the foot of this
    -- file; `the_checkpoint_stamp_is_identical_in_both_places` fails on drift.
    checkpoint_stamp CONSTANT pg_catalog.text :=
        'AEON tenancy tranche 1 (migration 0032). Idempotency stamp for the checkpoint '
        'table: it records that this migration created this table, so that a re-run over '
        'its own work is a no-op. It is NOT evidence that the rows are authentic.';

    -- THE authoritative DDL.  Written once and used twice: to build the real
    -- table in `public`, and to build the canonical reference in `pg_temp`
    -- that an occupant is compared against.  Two hand-maintained definitions
    -- could drift, and a drifted reference would validate the wrong contract.
    checkpoint_body CONSTANT pg_catalog.text := $ckbody$(
    -- Every capturable type name and every default function is written
    -- qualified.  `BIGINT` below is deliberately NOT qualified: it is
    -- grammar-level and a shadow of that name is never consulted (measured).
    id              pg_catalog.uuid
                        DEFAULT pg_catalog.gen_random_uuid(),
    tranche         pg_catalog.text        NOT NULL,
    contract_digest pg_catalog.text        NOT NULL,
    status          pg_catalog.text        NOT NULL,
    -- NULL once the tranche completes: there is nothing left to resume.
    resume_cursor   pg_catalog.text,
    rows_total      BIGINT      NOT NULL DEFAULT 0,
    rows_backfilled BIGINT      NOT NULL DEFAULT 0,
    blocking_count  BIGINT      NOT NULL DEFAULT 0,
    started_at      pg_catalog.timestamptz NOT NULL DEFAULT pg_catalog.now(),
    updated_at      pg_catalog.timestamptz NOT NULL DEFAULT pg_catalog.now(),
    completed_at    pg_catalog.timestamptz,
    -- Named explicitly.  An implicit PRIMARY KEY would be called
    -- `<table>_pkey`, and the canonical reference is deliberately named
    -- differently, so the two sides would otherwise disagree about this
    -- constraint's name for no reason but the reference's name.
    CONSTRAINT tenancy_backfill_checkpoints_pkey PRIMARY KEY (id),
    CONSTRAINT tenancy_backfill_checkpoints_status_ck
        CHECK (status IN ('IN_PROGRESS', 'COMPLETED', 'ABANDONED')),
    -- `tranche` is backed by the closed `plan::Tranche` enum exactly as `status`
    -- is backed by `CheckpointStatus`, so it gets the same treatment.  Without
    -- this, a one-character typo produces a row no guard will ever match: the
    -- backfill reports success, FINALIZE keeps refusing, and nothing says why.
    -- It fails closed, which is why this is a legibility fix rather than a
    -- soundness one — but an unmatched checkpoint is a bad way to learn that.
    CONSTRAINT tenancy_backfill_checkpoints_tranche_ck
        CHECK (tranche IN (
            'TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN',
            'TRANCHE_2_SESSIONS',
            'TRANCHE_3_MEMORIES',
            'TRANCHE_4_LINEAGE_AND_ARCHIVAL',
            'TRANCHE_5_OPERATIONS',
            'FINAL_CONSTRAINT_TIGHTENING'
        )),
    -- A completion is the only state that carries a completion time, and it is
    -- the only state with nothing left to resume.  Stating both structurally
    -- stops a half-written row from reading as authoritative evidence.
    CONSTRAINT tenancy_backfill_checkpoints_completed_shape_ck
        CHECK ((status = 'COMPLETED') = (completed_at IS NOT NULL)),
    CONSTRAINT tenancy_backfill_checkpoints_completed_cursor_ck
        CHECK (status <> 'COMPLETED' OR resume_cursor IS NULL),
    CONSTRAINT tenancy_backfill_checkpoints_counts_ck
        CHECK (rows_total >= 0 AND rows_backfilled >= 0 AND blocking_count >= 0
               AND rows_backfilled <= rows_total),
    -- FINALIZE_PRECONDITION.max_blocking_count = 0 is the gate the whole
    -- three-stage protocol exists to protect.  Enforcing it here as well means
    -- a COMPLETED row cannot exist while the tranche still has blocking
    -- findings, so the guard is not the only thing standing between a dirty
    -- tranche and a validated constraint.
    CONSTRAINT tenancy_backfill_checkpoints_completed_clean_ck
        CHECK (status <> 'COMPLETED' OR blocking_count = 0),
    -- A completion claims the tranche is finished, so its accounting has to say
    -- so too.  Without this a row can read COMPLETED with 3 of 1000 rows done
    -- and satisfy every other constraint.  The backfill command reconciles
    -- rows_total against the final count before it completes.
    CONSTRAINT tenancy_backfill_checkpoints_completed_accounting_ck
        CHECK (status <> 'COMPLETED' OR rows_backfilled = rows_total)
)$ckbody$;

    idx_completed_tmpl CONSTANT pg_catalog.text :=
        'CREATE UNIQUE INDEX %s ON %s (tranche, contract_digest) '
        'WHERE status = ''COMPLETED''';
    idx_tranche_tmpl CONSTANT pg_catalog.text :=
        'CREATE INDEX %s ON %s (tranche, started_at DESC)';

    -- THE comparator, likewise written once and executed twice -- tolerant on
    -- the first pass, zero-tolerance on the second.  $1 live oid, $2 reference
    -- oid, $3 whether to tolerate an owned index that is absent entirely.
    checkpoint_diff_sql CONSTANT pg_catalog.text := $ckdiff$
WITH targets(side, relid) AS (
    VALUES ('reference'::text, $2::oid), ('live'::text, $1::oid)
),
ident AS (
    SELECT t.side, t.relid, n.nspname AS nsp, c.relname AS rel
      FROM targets t
      JOIN pg_catalog.pg_class c ON c.oid = t.relid
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
),
rel AS (
    SELECT i.side, 'relation'::text AS cat, 'self'::text AS obj,
           jsonb_build_object(
             'relkind',            c.relkind,
             'relpersistence',     CASE WHEN i.side = 'reference' THEN 'p' ELSE c.relpersistence::text END,
             'access_method',      COALESCE(am.amname, '<none>'),
             'owner',              pg_catalog.pg_get_userbyid(c.relowner),
             'replica_identity',   c.relreplident,
             'replica_index',      COALESCE((SELECT ri.relname FROM pg_catalog.pg_index x
                                               JOIN pg_catalog.pg_class ri ON ri.oid = x.indexrelid
                                              WHERE x.indrelid = c.oid AND x.indisreplident), '<none>'),
             'row_security',       c.relrowsecurity,
             'force_row_security', c.relforcerowsecurity,
             'is_partition',       c.relispartition,
             'of_type',            COALESCE(c.reloftype::regtype::text, '<none>'),
             'partition_key',      COALESCE(pg_catalog.pg_get_partkeydef(c.oid), '<none>')
           ) AS props
      FROM ident i
      JOIN pg_catalog.pg_class c ON c.oid = i.relid
      LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
),
col AS (
    SELECT i.side, 'column'::text AS cat,
           CASE WHEN a.attisdropped THEN 'attnum ' || a.attnum::text ELSE a.attname END AS obj,
           jsonb_build_object(
             'attnum',       a.attnum,
             'dropped',      a.attisdropped,
             'type',         CASE WHEN a.attisdropped THEN '<dropped>'
                                  ELSE pg_catalog.format_type(a.atttypid, a.atttypmod) END,
             'notnull',      a.attnotnull,
             'identity',     a.attidentity,
             'generated',    a.attgenerated,
             'ndims',        a.attndims,
             'islocal',      a.attislocal,
             'inhcount',     a.attinhcount,
             'collation',    COALESCE((SELECT cl.collname FROM pg_catalog.pg_collation cl
                                        WHERE cl.oid = a.attcollation), '<none>'),
             'default',      COALESCE(pg_catalog.pg_get_expr(ad.adbin, ad.adrelid), '<none>'),
             'has_missing',  a.atthasmissing,
             'missing_val',  COALESCE(a.attmissingval::text, '<none>')
           ) AS props
      FROM ident i
      JOIN pg_catalog.pg_attribute a ON a.attrelid = i.relid AND a.attnum > 0
      LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
),
con AS (
    SELECT i.side, 'constraint'::text AS cat, k.conname AS obj,
           jsonb_build_object(
             'contype',     k.contype,
             'definition',  pg_catalog.pg_get_constraintdef(k.oid),
             'validated',   k.convalidated,
             'deferrable',  k.condeferrable,
             'deferred',    k.condeferred,
             'columns',     COALESCE((SELECT pg_catalog.string_agg(at.attname, ',' ORDER BY o.ord)
                                        FROM pg_catalog.unnest(k.conkey) WITH ORDINALITY AS o(attnum, ord)
                                        JOIN pg_catalog.pg_attribute at
                                          ON at.attrelid = k.conrelid AND at.attnum = o.attnum), '<none>'),
             'index',       COALESCE((SELECT ci.relname FROM pg_catalog.pg_class ci
                                       WHERE ci.oid = k.conindid), '<none>')
           ) AS props
      FROM ident i
      JOIN pg_catalog.pg_constraint k ON k.conrelid = i.relid
),
idx_names AS (
    SELECT i.side, i.nsp, x.relname AS obj
      FROM ident i
      JOIN pg_catalog.pg_index ix ON ix.indrelid = i.relid
      JOIN pg_catalog.pg_class x ON x.oid = ix.indexrelid
    UNION
    SELECT i.side, i.nsp, cand.nm
      FROM ident i
      CROSS JOIN (VALUES ('tenancy_backfill_checkpoints_completed_key'),
                         ('idx_tenancy_backfill_checkpoints_tranche')) AS cand(nm)
     WHERE pg_catalog.to_regclass(pg_catalog.quote_ident(i.nsp) || '.' || pg_catalog.quote_ident(cand.nm))
           IS NOT NULL
),
idx AS (
    SELECT n.side, 'index'::text AS cat, n.obj,
           jsonb_build_object(
             'holder_kind',   COALESCE(hc.relkind::text, '<absent>'),
             'access_method', COALESCE(am.amname, '<none>'),
             'owner',         COALESCE(pg_catalog.pg_get_userbyid(hc.relowner), '<none>'),
             'definition',    COALESCE(pg_catalog.pg_get_indexdef(ix.indexrelid), '<none>'),
             'unique',        COALESCE(ix.indisunique::text, '-'),
             'primary',       COALESCE(ix.indisprimary::text, '-'),
             'valid',         COALESCE(ix.indisvalid::text, '-'),
             'ready',         COALESCE(ix.indisready::text, '-'),
             'live',          COALESCE(ix.indislive::text, '-'),
             'immediate',     COALESCE(ix.indimmediate::text, '-'),
             'replident',     COALESCE(ix.indisreplident::text, '-'),
             'keyatts',       COALESCE(ix.indnkeyatts::text, '-'),
             'totalatts',     COALESCE(ix.indnatts::text, '-'),
             'nulls_not_distinct', COALESCE(ix.indnullsnotdistinct::text, '-')
           ) AS props
      FROM idx_names n
      LEFT JOIN pg_catalog.pg_class hc
             ON hc.oid = pg_catalog.to_regclass(pg_catalog.quote_ident(n.nsp) || '.' || pg_catalog.quote_ident(n.obj))
      LEFT JOIN pg_catalog.pg_index ix ON ix.indexrelid = hc.oid
      LEFT JOIN pg_catalog.pg_am am ON am.oid = hc.relam
),
trg AS (
    SELECT i.side, 'trigger'::text AS cat, tg.tgname AS obj,
           jsonb_build_object('definition', pg_catalog.pg_get_triggerdef(tg.oid),
                              'enabled',    tg.tgenabled) AS props
      FROM ident i
      JOIN pg_catalog.pg_trigger tg ON tg.tgrelid = i.relid AND NOT tg.tgisinternal
),
pol AS (
    SELECT i.side, 'policy'::text AS cat, p.polname AS obj,
           jsonb_build_object(
             'command',    p.polcmd,
             'permissive', p.polpermissive,
             'roles',      COALESCE((SELECT pg_catalog.string_agg(pg_catalog.pg_get_userbyid(r), ',' ORDER BY r)
                                       FROM pg_catalog.unnest(p.polroles) AS r), '<public>'),
             'using',      COALESCE(pg_catalog.pg_get_expr(p.polqual, p.polrelid), '<none>'),
             'check',      COALESCE(pg_catalog.pg_get_expr(p.polwithcheck, p.polrelid), '<none>')
           ) AS props
      FROM ident i
      JOIN pg_catalog.pg_policy p ON p.polrelid = i.relid
),
rul AS (
    SELECT i.side, 'rule'::text AS cat, r.rulename AS obj,
           jsonb_build_object('definition', pg_catalog.pg_get_ruledef(r.oid),
                              'enabled',    r.ev_enabled,
                              'event',      r.ev_type,
                              'instead',    r.is_instead) AS props
      FROM ident i
      JOIN pg_catalog.pg_rewrite r ON r.ev_class = i.relid
),
inh AS (
    SELECT i.side, 'inheritance'::text AS cat, 'parent ' || p.relname AS obj,
           jsonb_build_object('seqno', h.inhseqno) AS props
      FROM ident i
      JOIN pg_catalog.pg_inherits h ON h.inhrelid = i.relid
      JOIN pg_catalog.pg_class p ON p.oid = h.inhparent
    UNION ALL
    SELECT i.side, 'inheritance'::text, 'child ' || ch.relname,
           jsonb_build_object('seqno', h.inhseqno)
      FROM ident i
      JOIN pg_catalog.pg_inherits h ON h.inhparent = i.relid
      JOIN pg_catalog.pg_class ch ON ch.oid = h.inhrelid
),
raw AS (
    SELECT * FROM rel UNION ALL SELECT * FROM col UNION ALL SELECT * FROM con
    UNION ALL SELECT * FROM idx UNION ALL SELECT * FROM trg UNION ALL SELECT * FROM pol
    UNION ALL SELECT * FROM rul UNION ALL SELECT * FROM inh
),
fp AS (
    SELECT r.side, r.cat, r.obj,
           (SELECT jsonb_object_agg(
                     e.key,
                     to_jsonb(pg_catalog.regexp_replace(
                                pg_catalog.regexp_replace(
                                pg_catalog.regexp_replace(
                                  CASE jsonb_typeof(e.value) WHEN 'string' THEN e.value #>> '{}'
                                                             ELSE e.value::text END,
                                  '\m' || i.rel || '\M', '<TARGET>', 'g'),
                                '\m' || i.nsp || '\M', '<SCHEMA>', 'g'),
                              '\mpg_temp\M', '<SCHEMA>', 'g')))
              FROM jsonb_each(r.props) AS e) AS props
      FROM raw r
      JOIN ident i ON i.side = r.side
),
live_fp AS (SELECT cat, obj, props FROM fp WHERE side = 'live'),
ref_fp  AS (SELECT cat, obj, props FROM fp WHERE side = 'reference'),
joined AS (
    SELECT COALESCE(l.cat, rf.cat) AS cat, COALESCE(l.obj, rf.obj) AS obj,
           l.props AS live_props, rf.props AS ref_props
      FROM live_fp l FULL OUTER JOIN ref_fp rf ON rf.cat = l.cat AND rf.obj = l.obj
),
lines AS (
    SELECT pg_catalog.format('missing:    %s %s', j.cat, j.obj) AS line
      FROM joined j
     WHERE j.live_props IS NULL
       AND NOT ($3 AND j.cat = 'index'
                    AND j.obj IN ('tenancy_backfill_checkpoints_completed_key',
                                  'idx_tenancy_backfill_checkpoints_tranche'))
    UNION ALL
    SELECT pg_catalog.format('unexpected: %s %s', j.cat, j.obj)
      FROM joined j
     WHERE j.ref_props IS NULL
    UNION ALL
    SELECT pg_catalog.format('differs:    %s %s %s: reference=%s live=%s',
                             j.cat, j.obj, e.key,
                             COALESCE(j.ref_props ->> e.key, '<absent>'),
                             COALESCE(j.live_props ->> e.key, '<absent>'))
      FROM joined j
      CROSS JOIN LATERAL jsonb_each(j.ref_props || j.live_props) AS e
     WHERE j.live_props IS NOT NULL AND j.ref_props IS NOT NULL
       AND (j.ref_props ->> e.key) IS DISTINCT FROM (j.live_props ->> e.key)
)
-- Aggregated here, not by the caller: `EXECUTE ... INTO` binds only the FIRST
-- row of a multi-row result, which would silently report one difference and
-- hide the rest.
SELECT pg_catalog.string_agg(line, pg_catalog.chr(10) ORDER BY line) FROM lines
$ckdiff$;

    live_oid pg_catalog.oid;
    ref_oid pg_catalog.oid;
    ref_ident pg_catalog.text;
    live_kind pg_catalog."char";
    live_persistence pg_catalog."char";
    diffs pg_catalog.text;
    privileges pg_catalog.text;
    live_has_rows BOOLEAN;
    live_is_stamped BOOLEAN;
    occupied pg_catalog.text;
BEGIN
    -- Ten ownership columns.
    SELECT pg_catalog.string_agg(
               pg_catalog.format('%s.%s', expected.tbl, expected.col),
               ', ' ORDER BY expected.tbl, expected.col)
      INTO occupied
      FROM (VALUES
              ('archival_batches', 'agent_uuid'), ('archival_batches', 'tenant_id'),
              ('audit_logs',       'agent_uuid'), ('audit_logs',       'tenant_id'),
              ('entities',         'agent_uuid'), ('entities',         'tenant_id'),
              ('memory_graph',     'agent_uuid'), ('memory_graph',     'tenant_id'),
              ('rmk_policies',     'agent_uuid'), ('rmk_policies',     'tenant_id')
           ) AS expected(tbl, col)
      JOIN pg_catalog.pg_attribute a
        ON a.attrelid = pg_catalog.to_regclass(pg_catalog.format('public.%I', expected.tbl))
       AND a.attname  = expected.col
       AND a.attnum   > 0
       AND NOT a.attisdropped
      JOIN pg_catalog.pg_class ct ON ct.oid = a.attrelid
     WHERE pg_catalog.col_description(a.attrelid, a.attnum::int) IS DISTINCT FROM marker
        OR NOT pg_catalog.pg_has_role(current_user, ct.relowner, 'USAGE');

    IF occupied IS NOT NULL THEN
        RAISE EXCEPTION
            'these ownership columns already exist and tenancy tranche 1 did not create '
            'them: %. Migration 0032 will not adopt a column it did not create, because '
            'rollback/0032 drops the ownership columns and would destroy data belonging to '
            'whatever does own them. Nothing has been changed. Either drop or rename the '
            'pre-existing columns, or -- if they really are this tranche''s and their '
            'provenance comment was removed -- restore the comment this migration stamps.',
            occupied
            USING ERRCODE = 'duplicate_column';
    END IF;

    -- Five bridge functions, matched by SIGNATURE rather than by name:
    -- PostgreSQL allows overloading, and only the zero-argument
    -- `trigger`-returning form is the one this file installs and rollback/0032
    -- drops.
    SELECT pg_catalog.string_agg(
               pg_catalog.format('public.%s()', expected.fn), ', ' ORDER BY expected.fn)
      INTO occupied
      FROM (VALUES
              ('fn_archival_batches_tenancy_bridge'),
              ('fn_audit_logs_tenancy_bridge'),
              ('fn_entities_tenancy_bridge'),
              ('fn_memory_graph_tenancy_bridge'),
              ('fn_rmk_policies_tenancy_bridge')
           ) AS expected(fn)
      JOIN pg_catalog.pg_proc p
        ON p.proname      = expected.fn
       AND p.pronamespace = 'public'::regnamespace
       AND p.pronargs     = 0
       AND p.prorettype   = 'pg_catalog.trigger'::regtype
     WHERE pg_catalog.obj_description(p.oid, 'pg_proc') IS DISTINCT FROM marker
        OR NOT pg_catalog.pg_has_role(current_user, p.proowner, 'USAGE');

    IF occupied IS NOT NULL THEN
        RAISE EXCEPTION
            'these bridge functions already exist and tenancy tranche 1 did not create '
            'them: %. CREATE OR REPLACE FUNCTION preserves the OID, so replacing them would '
            'silently change the behaviour of every trigger that already calls them, and '
            'rollback/0032 would then drop them outright. Nothing has been changed.',
            occupied
            USING ERRCODE = 'duplicate_function';
    END IF;

    -- Five bridge triggers, scoped to the table each one belongs on: trigger
    -- names are unique only per table.
    SELECT pg_catalog.string_agg(
               pg_catalog.format('%s on public.%s', expected.trg, expected.tbl),
               ', ' ORDER BY expected.trg)
      INTO occupied
      FROM (VALUES
              ('trg_archival_batches_tenancy_bridge', 'archival_batches'),
              ('trg_audit_logs_tenancy_bridge',       'audit_logs'),
              ('trg_entities_tenancy_bridge',         'entities'),
              ('trg_memory_graph_tenancy_bridge',     'memory_graph'),
              ('trg_rmk_policies_tenancy_bridge',     'rmk_policies')
           ) AS expected(trg, tbl)
      JOIN pg_catalog.pg_trigger t
        ON t.tgname  = expected.trg
       AND t.tgrelid = pg_catalog.to_regclass(pg_catalog.format('public.%I', expected.tbl))
       AND NOT t.tgisinternal
      JOIN pg_catalog.pg_class tt ON tt.oid = t.tgrelid
     WHERE pg_catalog.obj_description(t.oid, 'pg_trigger') IS DISTINCT FROM marker
        OR NOT pg_catalog.pg_has_role(current_user, tt.relowner, 'USAGE');

    IF occupied IS NOT NULL THEN
        RAISE EXCEPTION
            'these bridge triggers already exist and tenancy tranche 1 did not create '
            'them: %. CREATE OR REPLACE TRIGGER would replace them and rollback/0032 would '
            'then drop them, leaving nothing to restore. Nothing has been changed.',
            occupied
            USING ERRCODE = 'duplicate_object';
    END IF;

    -- ── The checkpoint protocol table: COMPARED, never enumerated ───────────
    --
    -- Six review rounds and about two dozen findings preceded this block, and
    -- every one was the same defect: the guard enumerated axes, and a reviewer
    -- found an axis it did not enumerate.  CHECK expressions, then
    -- `convalidated`, then schema qualification, then index shape, then
    -- `indrelid`, then column types, then defaults, then the primary key, then
    -- ordinal position, then the column SET, then expression index keys, then
    -- the primary key's NAME, then the CHECK set, then generated columns, then
    -- triggers, then non-index occupants of an index name.
    --
    -- Enumeration cannot terminate, because the set of ways a table can differ
    -- is open.  So this stops enumerating and COMPARES: it builds a canonical
    -- reference from the very DDL that builds the real table, and diffs the
    -- occupant against it in both directions.  An axis nobody has thought of is
    -- covered because it is present on one side and absent from the other, not
    -- because somebody remembered to check it.
    --
    -- Two things a reference cannot legitimately speak to get explicit policies
    -- instead: PRIVILEGES (ALTER DEFAULT PRIVILEGES can grant on the reference
    -- too, so a reference-versus-live comparison would be unsound) and the
    -- AUTHENTICITY OF EXISTING ROWS (no schema comparison can establish that).

    -- Serialise concurrent runs.  Transaction-scoped, and it covers what the
    -- table lock cannot: when the table does not exist there is nothing to
    -- LOCK, and two sessions would race between the existence probe and the
    -- bare CREATE.
    --
    -- Key allocation -- fixed literals rather than hashtext(), so a collision is
    -- auditable rather than opaque:
    --     1095979598 = 0x4145_4F4E  'AEON'  -- namespace for this repository
    --        3276801 = 0x0032_0001          -- migration 0032, object 1
    PERFORM pg_catalog.pg_advisory_xact_lock(1095979598, 3276801);

    live_oid := pg_catalog.to_regclass('public.tenancy_backfill_checkpoints');

    IF live_oid IS NOT NULL THEN
        -- Kind check BEFORE the lock, not after: LOCK TABLE on a view or a
        -- sequence fails with a message about the wrong problem, and a
        -- partitioned table would take a very different lock.
        SELECT c.relkind, c.relpersistence
          INTO live_kind, live_persistence
          FROM pg_catalog.pg_class c
         WHERE c.oid = live_oid;

        IF live_kind <> 'r' OR live_persistence <> 'p' THEN
            RAISE EXCEPTION
                'public.tenancy_backfill_checkpoints exists but is not an ordinary '
                'permanent table (relkind=%, relpersistence=%). This migration will not '
                'adopt it and will not lock it. Nothing has been changed.',
                live_kind, live_persistence
                USING ERRCODE = 'duplicate_table';
        END IF;

        -- Held for the rest of the transaction, so the occupant cannot change
        -- shape between the fingerprint, the decision, the index repair and the
        -- re-fingerprint.  rollback/0032 takes SHARE ROW EXCLUSIVE on this same
        -- table; nothing in this tranche takes the two in the opposite order.
        LOCK TABLE public.tenancy_backfill_checkpoints IN ACCESS EXCLUSIVE MODE;

        -- The name may have been dropped and recreated between the probe above
        -- and the lock being granted.  A different table wearing the same name
        -- is not the table that was inspected.
        IF pg_catalog.to_regclass('public.tenancy_backfill_checkpoints')
           IS DISTINCT FROM live_oid THEN
            RAISE EXCEPTION
                'public.tenancy_backfill_checkpoints was replaced while this migration '
                'waited for its lock. Nothing has been changed; re-run.'
                USING ERRCODE = 'duplicate_table';
        END IF;
    END IF;

    -- The canonical reference, built from the same DDL as the real table.
    --
    -- DISTINCTLY NAMED on purpose.  An identically named temp table would be
    -- resolved before `public` by every unqualified reference for the rest of
    -- the session, and making the comparison depend on the two names being
    -- equal is precisely the fragility this block exists to remove.  The cost is
    -- that the primary key can no longer be implicit -- PostgreSQL would derive
    -- `<table>_pkey` and the two sides would disagree about its name -- which is
    -- why the shared DDL below names every constraint explicitly.
    --
    -- ON COMMIT DROP is the cleanup proof: the reference cannot outlive this
    -- transaction on any path, because a RAISE aborts the transaction and the
    -- drop happens on abort exactly as it does on commit.
    EXECUTE pg_catalog.format('CREATE TEMPORARY TABLE %I %s ON COMMIT DROP',
                              'tenancy_backfill_checkpoints_reference', checkpoint_body);

    -- Resolved BEFORE the reference's indexes are built, so that they too can
    -- name their target explicitly.  `pg_my_temp_schema()` cannot be read any
    -- earlier: it returns 0 until this session actually owns a temp schema, and
    -- the CREATE above is what creates one.
    ref_ident := pg_catalog.quote_ident(
                     pg_catalog.pg_my_temp_schema()::regnamespace::text)
                 || '.' || pg_catalog.quote_ident(
                               'tenancy_backfill_checkpoints_reference');
    ref_oid := pg_catalog.to_regclass(ref_ident);

    IF ref_oid IS NULL THEN
        RAISE EXCEPTION
            'the canonical reference table could not be resolved after creation; this '
            'migration cannot validate the checkpoint table without it. Nothing has '
            'been changed.'
            USING ERRCODE = 'internal_error';
    END IF;

    -- Qualified by the resolved temp schema rather than left bare.  A bare name
    -- would resolve through the implicit pg_temp-first rule, which is a rule
    -- about precedence -- exactly the kind of ambient resolution the pinned
    -- search_path above exists to stop this block from depending on.
    EXECUTE pg_catalog.format(idx_completed_tmpl,
                              'tenancy_backfill_checkpoints_completed_key', ref_ident);
    EXECUTE pg_catalog.format(idx_tranche_tmpl,
                              'idx_tenancy_backfill_checkpoints_tranche', ref_ident);

    IF live_oid IS NULL THEN
        -- Fresh creation.  No IF NOT EXISTS anywhere below: that primitive IS
        -- the silent adoption this block exists to remove, and existence is
        -- already known.  A colliding CREATE raises, which is the correct
        -- outcome for a concurrent creator.
        EXECUTE pg_catalog.format('CREATE TABLE public.%I %s',
                                  'tenancy_backfill_checkpoints', checkpoint_body);
        EXECUTE pg_catalog.format(idx_completed_tmpl,
                                  'tenancy_backfill_checkpoints_completed_key',
                                  'public.tenancy_backfill_checkpoints');
        EXECUTE pg_catalog.format(idx_tranche_tmpl,
                                  'idx_tenancy_backfill_checkpoints_tranche',
                                  'public.tenancy_backfill_checkpoints');
        live_oid := pg_catalog.to_regclass('public.tenancy_backfill_checkpoints');
    ELSE
        -- PASS 1.  Tolerant of exactly ONE thing: one of the two indexes this
        -- migration owns resolving to nothing at all.  A table that is otherwise
        -- exactly right is not refused merely because its migration-owned
        -- indexes have not been built yet.  A wrong-shaped index, or a non-index
        -- relation wearing either name, is a difference like any other.
        --
        -- Rendered under the trusted path pinned above: `pg_get_indexdef` and
        -- friends qualify names according to search_path, so a hostile path
        -- would change how both sides are spelled.
        EXECUTE checkpoint_diff_sql INTO diffs USING live_oid, ref_oid, TRUE;

        IF diffs IS NOT NULL THEN
            RAISE EXCEPTION
                'public.tenancy_backfill_checkpoints exists but is not the table this '
                'migration builds. It is compared against a canonical reference built '
                'from this migration''s own DDL, so the list below is exhaustive rather '
                'than a set of remembered checks:
%
Nothing has been changed. Drop or rename the occupant, then re-run.',
                diffs
                USING ERRCODE = 'duplicate_table';
        END IF;

        -- Repair, restricted BY NAME to the two indexes this migration owns.
        -- Not "create whatever is missing": a general repair step would turn the
        -- single tolerance above into a fresh adoption hole.
        IF pg_catalog.to_regclass('public.tenancy_backfill_checkpoints_completed_key')
           IS NULL THEN
            EXECUTE pg_catalog.format(idx_completed_tmpl,
                                      'tenancy_backfill_checkpoints_completed_key',
                                      'public.tenancy_backfill_checkpoints');
        END IF;
        IF pg_catalog.to_regclass('public.idx_tenancy_backfill_checkpoints_tranche')
           IS NULL THEN
            EXECUTE pg_catalog.format(idx_tranche_tmpl,
                                      'idx_tenancy_backfill_checkpoints_tranche',
                                      'public.tenancy_backfill_checkpoints');
        END IF;
    END IF;

    -- The ZERO-TOLERANCE comparison, on EVERY path including fresh creation.
    --
    -- Running it after our own CREATE is not redundant: it proves the DDL
    -- actually produced the contract in `public`, rather than assuming it did.
    -- An event trigger rewriting DDL, or a shadow schema capturing a relation
    -- name, surfaces here.  A hostile search_path is NOT among the things this
    -- comparison can catch -- it poisons both sides identically -- which is why
    -- the trusted path is pinned before either side is built instead.
    --
    -- Rendered under that same pinned path, because `pg_get_indexdef` and
    -- friends qualify names according to search_path.
    EXECUTE checkpoint_diff_sql INTO diffs USING live_oid, ref_oid, FALSE;

    IF diffs IS NOT NULL THEN
        RAISE EXCEPTION
            'the checkpoint table does not match the contract this migration builds:
%
Nothing has been changed.',
            diffs
            USING ERRCODE = 'duplicate_table';
    END IF;

    -- PRIVILEGES, by absolute policy rather than by comparison.
    --
    -- The reference cannot serve as the standard here: ALTER DEFAULT PRIVILEGES
    -- -- global, or scoped to a schema -- grants on newly created tables, so the
    -- reference can acquire the very grants that ought to be refused, and the
    -- two sides would agree while both were wrong.  The live object is therefore
    -- held to an absolute rule: the only privileges permitted are the owner's
    -- own, granted by the owner, not grantable onward.  A NULL acl (the
    -- owner-implicit default) satisfies this trivially; PUBLIC appears as
    -- grantee 0 and is refused.
    SELECT pg_catalog.string_agg(p.line, E'\n' ORDER BY p.line)
      INTO privileges
      FROM (
            SELECT pg_catalog.format(
                       'privilege:  table grants %s to %s',
                       a.privilege_type,
                       CASE WHEN a.grantee = 0 THEN 'PUBLIC'
                            ELSE pg_catalog.pg_get_userbyid(a.grantee) END) AS line
              FROM pg_catalog.pg_class c
              CROSS JOIN pg_catalog.aclexplode(c.relacl) AS a
             WHERE c.oid = live_oid
               AND NOT (a.grantee = c.relowner AND a.grantor = c.relowner
                        AND NOT a.is_grantable)
            UNION ALL
            SELECT pg_catalog.format(
                       'privilege:  column %s grants %s to %s',
                       at.attname, a.privilege_type,
                       CASE WHEN a.grantee = 0 THEN 'PUBLIC'
                            ELSE pg_catalog.pg_get_userbyid(a.grantee) END)
              FROM pg_catalog.pg_attribute at
              JOIN pg_catalog.pg_class c ON c.oid = at.attrelid
              CROSS JOIN pg_catalog.aclexplode(at.attacl) AS a
             WHERE at.attrelid = live_oid AND at.attnum > 0 AND NOT at.attisdropped
               AND NOT (a.grantee = c.relowner AND a.grantor = c.relowner
                        AND NOT a.is_grantable)
           ) AS p;

    IF privileges IS NOT NULL THEN
        RAISE EXCEPTION
            'the checkpoint table carries privileges this migration does not grant. Only '
            'the owner''s own, non-grantable privileges are permitted, because this table '
            'holds the evidence FINALIZE reads:
%
Nothing has been changed. Revoke them, or drop the occupant, then re-run.',
            privileges
            USING ERRCODE = 'insufficient_privilege';
    END IF;

    -- EXISTING ROWS.  Structure and privileges are settled by this point; what
    -- remains is whether rows already in the table may be left in place.
    --
    -- WHAT THE STAMP IS: migration-idempotency provenance, and nothing else.  It
    -- records that THIS migration created THIS table, so that re-running 0032
    -- after BACKFILL has written real rows stays the no-op this file's
    -- idempotency contract promises.
    --
    -- WHAT THE STAMP IS NOT: it is NOT evidence that the rows are authentic, and
    -- must never be read as such.  A role able to create objects in `public` can
    -- also write the comment, so a privileged actor can forge it -- the same
    -- limitation already documented for the other three markers, where the
    -- load-bearing control is PostgreSQL 15+ not granting CREATE on `public` to
    -- PUBLIC rather than anything in this file.
    --
    -- FINALIZE's writer-authority requirements are SEPARATE and UNCHANGED.  This
    -- stamp does not satisfy, weaken or substitute for any part of
    -- FINALIZE_PRECONDITION, which continues to assert its own conditions over
    -- checkpoint evidence independently of anything decided here.
    EXECUTE 'SELECT EXISTS (SELECT 1 FROM public.tenancy_backfill_checkpoints)'
       INTO live_has_rows;

    IF live_has_rows THEN
        SELECT pg_catalog.col_description(live_oid, a.attnum) IS NOT DISTINCT FROM
               checkpoint_stamp
          INTO live_is_stamped
          FROM pg_catalog.pg_attribute a
         WHERE a.attrelid = live_oid AND a.attname = 'id';

        IF NOT COALESCE(live_is_stamped, FALSE) THEN
            RAISE EXCEPTION
                'public.tenancy_backfill_checkpoints already holds rows and does not carry '
                'this migration''s idempotency stamp. Its structure matches, but this '
                'migration did not create it, so the rows in it are checkpoint evidence of '
                'unknown origin and FINALIZE reads exactly this table. Nothing has been '
                'changed. Empty it, drop it, or rename it, then re-run.'
                USING ERRCODE = 'duplicate_table';
        END IF;
    END IF;
END $provenance_guard$;

COMMENT ON TABLE public.tenancy_backfill_checkpoints IS
    'Per-tranche, per-contract backfill evidence (plan §4 step 4B). BACKFILL writes progress '
    'here and FINALIZE refuses to proceed without a COMPLETED row for its own tranche against '
    'the current contract digest with blocking_count = 0. ABANDONED rows are retained as '
    'history and are never treated as completion.';

COMMENT ON COLUMN public.tenancy_backfill_checkpoints.contract_digest IS
    'report::inventory_digest() at the time the backfill ran. A backfill that ran against a '
    'superseded plan proves nothing about the current one, so FINALIZE compares this exactly.';

COMMENT ON COLUMN public.tenancy_backfill_checkpoints.blocking_count IS
    'Blocking findings the authoritative audit reported for THIS tranche at completion time. '
    'Per-tranche rather than global: a later tranche is not evidence about this one.';

-- ── Ownership columns ───────────────────────────────────────────────────────
-- Added NULL-able.  ADD COLUMN with no default is metadata-only on PostgreSQL
-- 11+: catalog update, no table rewrite, no scan.
--
-- archival_batches, entities, memory_graph and rmk_policies are
-- NullableThenTightened — NULL only until FUTURE STEP 7.  audit_logs is
-- RemainsNullable: it records agentless events (startup, configuration change,
-- administrative action) which are legitimate rows with no owning agent, so
-- tightening it would make the schema reject valid audit history.

ALTER TABLE public.archival_batches
    ADD COLUMN IF NOT EXISTS agent_uuid pg_catalog.uuid,
    ADD COLUMN IF NOT EXISTS tenant_id  pg_catalog.uuid;

ALTER TABLE public.audit_logs
    ADD COLUMN IF NOT EXISTS agent_uuid pg_catalog.uuid,
    ADD COLUMN IF NOT EXISTS tenant_id  pg_catalog.uuid;

ALTER TABLE public.entities
    ADD COLUMN IF NOT EXISTS agent_uuid pg_catalog.uuid,
    ADD COLUMN IF NOT EXISTS tenant_id  pg_catalog.uuid;

ALTER TABLE public.memory_graph
    ADD COLUMN IF NOT EXISTS agent_uuid pg_catalog.uuid,
    ADD COLUMN IF NOT EXISTS tenant_id  pg_catalog.uuid;

ALTER TABLE public.rmk_policies
    ADD COLUMN IF NOT EXISTS agent_uuid pg_catalog.uuid,
    ADD COLUMN IF NOT EXISTS tenant_id  pg_catalog.uuid;

-- ── Transitional write bridges ──────────────────────────────────────────────
-- A database bridge trigger is chosen over exhaustive application dual-write.
-- Dual-write requires enumerating every writer and keeping that enumeration
-- exhaustive; it is defeated by a rollback to the previous release, by a second
-- service, by a maintenance script, and by any psql session.  The trigger is
-- attached to the table, so it holds for every writer including ones nobody
-- enumerated, and it survives an application rollback because it is schema
-- rather than code.  It is installed here, in PREPARE and before any backfill,
-- so no window exists in which new rows are unowned.
--
-- All five are SECURITY INVOKER with a pinned search_path and fully-qualified
-- bodies: a caller with a hostile search_path could otherwise make `agents`
-- resolve to a shadow table and have the bridge resolve ownership from it.
-- Neither the pin nor the qualification is load-bearing alone.

CREATE OR REPLACE FUNCTION public.fn_archival_batches_tenancy_bridge()
RETURNS pg_catalog.trigger
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, public
AS $$
DECLARE
    resolved_uuid   pg_catalog.uuid;
    resolved_tenant pg_catalog.uuid;
BEGIN
    SELECT a.id, a.tenant_id
      INTO resolved_uuid, resolved_tenant
      FROM public.agents a
     WHERE a.agent_id = NEW.agent_id;

    IF NEW.agent_uuid IS NOT NULL AND resolved_uuid IS NOT NULL
       AND NEW.agent_uuid <> resolved_uuid THEN
        RAISE EXCEPTION
            'archival_batches row for agent % supplies an agent_uuid that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.tenant_id IS NOT NULL AND resolved_tenant IS NOT NULL
       AND NEW.tenant_id <> resolved_tenant THEN
        RAISE EXCEPTION
            'archival_batches row for agent % supplies a tenant_id that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- `agents` is the sole authority, so assign unconditionally rather than
    -- only filling NULLs.
    --
    -- Filling only NULLs left a cross-tenant injection path, measured on pg16:
    -- name an agent that does not exist and supply any tenant_id, and the row
    -- was accepted carrying that tenant. Neither guard above can fire in that
    -- case — both contradiction checks require the RESOLVED side to be
    -- non-NULL, and the composite foreign key is skipped entirely because
    -- MATCH SIMPLE ignores a key with a NULL component. Only archival_batches
    -- was protected, and only because it happens to carry an agent_id foreign
    -- key from migration 0006; entities, memory_graph, rmk_policies and
    -- audit_logs carry none.
    --
    -- Unconditional assignment closes it: when the agent does not resolve,
    -- resolved_* are NULL and any caller-supplied ownership is overwritten with
    -- NULL, which is exactly the PRESERVED_UNRESOLVED outcome the contract
    -- names and which the audit then reports. When the agent does resolve, the
    -- checks above have already refused any value that disagrees, so this
    -- overwrite is a no-op for honest writers.
    NEW.agent_uuid := resolved_uuid;
    NEW.tenant_id  := resolved_tenant;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE TRIGGER trg_archival_batches_tenancy_bridge
    BEFORE INSERT OR UPDATE ON public.archival_batches
    FOR EACH ROW EXECUTE FUNCTION public.fn_archival_batches_tenancy_bridge();

CREATE OR REPLACE FUNCTION public.fn_entities_tenancy_bridge()
RETURNS pg_catalog.trigger
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, public
AS $$
DECLARE
    resolved_uuid   pg_catalog.uuid;
    resolved_tenant pg_catalog.uuid;
BEGIN
    SELECT a.id, a.tenant_id
      INTO resolved_uuid, resolved_tenant
      FROM public.agents a
     WHERE a.agent_id = NEW.agent_id;

    IF NEW.agent_uuid IS NOT NULL AND resolved_uuid IS NOT NULL
       AND NEW.agent_uuid <> resolved_uuid THEN
        RAISE EXCEPTION
            'entities row for agent % supplies an agent_uuid that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.tenant_id IS NOT NULL AND resolved_tenant IS NOT NULL
       AND NEW.tenant_id <> resolved_tenant THEN
        RAISE EXCEPTION
            'entities row for agent % supplies a tenant_id that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- `agents` is the sole authority, so assign unconditionally rather than
    -- only filling NULLs.
    --
    -- Filling only NULLs left a cross-tenant injection path, measured on pg16:
    -- name an agent that does not exist and supply any tenant_id, and the row
    -- was accepted carrying that tenant. Neither guard above can fire in that
    -- case — both contradiction checks require the RESOLVED side to be
    -- non-NULL, and the composite foreign key is skipped entirely because
    -- MATCH SIMPLE ignores a key with a NULL component. Only archival_batches
    -- was protected, and only because it happens to carry an agent_id foreign
    -- key from migration 0006; entities, memory_graph, rmk_policies and
    -- audit_logs carry none.
    --
    -- Unconditional assignment closes it: when the agent does not resolve,
    -- resolved_* are NULL and any caller-supplied ownership is overwritten with
    -- NULL, which is exactly the PRESERVED_UNRESOLVED outcome the contract
    -- names and which the audit then reports. When the agent does resolve, the
    -- checks above have already refused any value that disagrees, so this
    -- overwrite is a no-op for honest writers.
    NEW.agent_uuid := resolved_uuid;
    NEW.tenant_id  := resolved_tenant;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE TRIGGER trg_entities_tenancy_bridge
    BEFORE INSERT OR UPDATE ON public.entities
    FOR EACH ROW EXECUTE FUNCTION public.fn_entities_tenancy_bridge();

CREATE OR REPLACE FUNCTION public.fn_memory_graph_tenancy_bridge()
RETURNS pg_catalog.trigger
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, public
AS $$
DECLARE
    resolved_uuid   pg_catalog.uuid;
    resolved_tenant pg_catalog.uuid;
BEGIN
    SELECT a.id, a.tenant_id
      INTO resolved_uuid, resolved_tenant
      FROM public.agents a
     WHERE a.agent_id = NEW.agent_id;

    IF NEW.agent_uuid IS NOT NULL AND resolved_uuid IS NOT NULL
       AND NEW.agent_uuid <> resolved_uuid THEN
        RAISE EXCEPTION
            'memory_graph row for agent % supplies an agent_uuid that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.tenant_id IS NOT NULL AND resolved_tenant IS NOT NULL
       AND NEW.tenant_id <> resolved_tenant THEN
        RAISE EXCEPTION
            'memory_graph row for agent % supplies a tenant_id that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- `agents` is the sole authority, so assign unconditionally rather than
    -- only filling NULLs.
    --
    -- Filling only NULLs left a cross-tenant injection path, measured on pg16:
    -- name an agent that does not exist and supply any tenant_id, and the row
    -- was accepted carrying that tenant. Neither guard above can fire in that
    -- case — both contradiction checks require the RESOLVED side to be
    -- non-NULL, and the composite foreign key is skipped entirely because
    -- MATCH SIMPLE ignores a key with a NULL component. Only archival_batches
    -- was protected, and only because it happens to carry an agent_id foreign
    -- key from migration 0006; entities, memory_graph, rmk_policies and
    -- audit_logs carry none.
    --
    -- Unconditional assignment closes it: when the agent does not resolve,
    -- resolved_* are NULL and any caller-supplied ownership is overwritten with
    -- NULL, which is exactly the PRESERVED_UNRESOLVED outcome the contract
    -- names and which the audit then reports. When the agent does resolve, the
    -- checks above have already refused any value that disagrees, so this
    -- overwrite is a no-op for honest writers.
    NEW.agent_uuid := resolved_uuid;
    NEW.tenant_id  := resolved_tenant;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE TRIGGER trg_memory_graph_tenancy_bridge
    BEFORE INSERT OR UPDATE ON public.memory_graph
    FOR EACH ROW EXECUTE FUNCTION public.fn_memory_graph_tenancy_bridge();

CREATE OR REPLACE FUNCTION public.fn_rmk_policies_tenancy_bridge()
RETURNS pg_catalog.trigger
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, public
AS $$
DECLARE
    resolved_uuid   pg_catalog.uuid;
    resolved_tenant pg_catalog.uuid;
BEGIN
    SELECT a.id, a.tenant_id
      INTO resolved_uuid, resolved_tenant
      FROM public.agents a
     WHERE a.agent_id = NEW.agent_id;

    IF NEW.agent_uuid IS NOT NULL AND resolved_uuid IS NOT NULL
       AND NEW.agent_uuid <> resolved_uuid THEN
        RAISE EXCEPTION
            'rmk_policies row for agent % supplies an agent_uuid that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.tenant_id IS NOT NULL AND resolved_tenant IS NOT NULL
       AND NEW.tenant_id <> resolved_tenant THEN
        RAISE EXCEPTION
            'rmk_policies row for agent % supplies a tenant_id that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- `agents` is the sole authority, so assign unconditionally rather than
    -- only filling NULLs.
    --
    -- Filling only NULLs left a cross-tenant injection path, measured on pg16:
    -- name an agent that does not exist and supply any tenant_id, and the row
    -- was accepted carrying that tenant. Neither guard above can fire in that
    -- case — both contradiction checks require the RESOLVED side to be
    -- non-NULL, and the composite foreign key is skipped entirely because
    -- MATCH SIMPLE ignores a key with a NULL component. Only archival_batches
    -- was protected, and only because it happens to carry an agent_id foreign
    -- key from migration 0006; entities, memory_graph, rmk_policies and
    -- audit_logs carry none.
    --
    -- Unconditional assignment closes it: when the agent does not resolve,
    -- resolved_* are NULL and any caller-supplied ownership is overwritten with
    -- NULL, which is exactly the PRESERVED_UNRESOLVED outcome the contract
    -- names and which the audit then reports. When the agent does resolve, the
    -- checks above have already refused any value that disagrees, so this
    -- overwrite is a no-op for honest writers.
    NEW.agent_uuid := resolved_uuid;
    NEW.tenant_id  := resolved_tenant;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE TRIGGER trg_rmk_policies_tenancy_bridge
    BEFORE INSERT OR UPDATE ON public.rmk_policies
    FOR EACH ROW EXECUTE FUNCTION public.fn_rmk_policies_tenancy_bridge();

-- ── The conditional bridge: audit_logs ──────────────────────────────────────
-- audit_logs.agent_id is NULL-able and legitimately so.  Declining to resolve
-- ownership at all would also decline it for the rows that DO name a resolvable
-- agent, which would then stay NULL for no reason and be indistinguishable from
-- genuinely agentless ones.  The bridge is therefore conditional rather than
-- absent, and it decides all four states named by
-- `plan::ConditionalOwnership::ALL`:
--
--   AGENTLESS_ALLOWED      agent_id IS NULL.  Ownership stays NULL and nothing
--                          is reported, because nothing is wrong.  Ownership
--                          arriving on an agentless row is dropped rather than
--                          trusted: `agents` says nothing about a row that
--                          names no agent, so accepting it would let a caller
--                          write history under any tenant it liked.
--   RESOLVED_AND_VERIFIED  agent_id resolves.  Both columns are populated from
--                          agents and any supplied values are verified.
--   PRESERVED_UNRESOLVED   agent_id names an agent that does not resolve, or an
--                          agent whose own tenant_id is NULL.  The row is
--                          written with NULL ownership rather than rejected, so
--                          the audit reports ORPHANED_AGENT_REFERENCE or
--                          UNMAPPED_AGENT against it.  Losing audit history is
--                          a worse outcome than an unowned audit row, and a
--                          silently dropped event is worse than both.
--   CONTRADICTION_REJECTED The row supplies ownership contradicting agents.
--                          Refused: a contradicted audit row is not evidence of
--                          anything.
CREATE OR REPLACE FUNCTION public.fn_audit_logs_tenancy_bridge()
RETURNS pg_catalog.trigger
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, public
AS $$
DECLARE
    resolved_uuid   pg_catalog.uuid;
    resolved_tenant pg_catalog.uuid;
BEGIN
    -- AGENTLESS_ALLOWED.
    IF NEW.agent_id IS NULL THEN
        NEW.agent_uuid := NULL;
        NEW.tenant_id  := NULL;
        RETURN NEW;
    END IF;

    SELECT a.id, a.tenant_id
      INTO resolved_uuid, resolved_tenant
      FROM public.agents a
     WHERE a.agent_id = NEW.agent_id;

    -- CONTRADICTION_REJECTED.
    IF NEW.agent_uuid IS NOT NULL AND resolved_uuid IS NOT NULL
       AND NEW.agent_uuid <> resolved_uuid THEN
        RAISE EXCEPTION
            'audit_logs row for agent % supplies an agent_uuid that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.tenant_id IS NOT NULL AND resolved_tenant IS NOT NULL
       AND NEW.tenant_id <> resolved_tenant THEN
        RAISE EXCEPTION
            'audit_logs row for agent % supplies a tenant_id that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- RESOLVED_AND_VERIFIED when the agent resolves; PRESERVED_UNRESOLVED when
    -- it does not, in which case both assignments below write NULL and the
    -- audit is left to report the row.
    -- `agents` is the sole authority, so assign unconditionally rather than
    -- only filling NULLs.
    --
    -- Filling only NULLs left a cross-tenant injection path, measured on pg16:
    -- name an agent that does not exist and supply any tenant_id, and the row
    -- was accepted carrying that tenant. Neither guard above can fire in that
    -- case — both contradiction checks require the RESOLVED side to be
    -- non-NULL, and the composite foreign key is skipped entirely because
    -- MATCH SIMPLE ignores a key with a NULL component. Only archival_batches
    -- was protected, and only because it happens to carry an agent_id foreign
    -- key from migration 0006; entities, memory_graph, rmk_policies and
    -- audit_logs carry none.
    --
    -- Unconditional assignment closes it: when the agent does not resolve,
    -- resolved_* are NULL and any caller-supplied ownership is overwritten with
    -- NULL, which is exactly the PRESERVED_UNRESOLVED outcome the contract
    -- names and which the audit then reports. When the agent does resolve, the
    -- checks above have already refused any value that disagrees, so this
    -- overwrite is a no-op for honest writers.
    NEW.agent_uuid := resolved_uuid;
    NEW.tenant_id  := resolved_tenant;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE TRIGGER trg_audit_logs_tenancy_bridge
    BEFORE INSERT OR UPDATE ON public.audit_logs
    FOR EACH ROW EXECUTE FUNCTION public.fn_audit_logs_tenancy_bridge();

-- ── Provenance stamps ───────────────────────────────────────────────────────
-- Every object created above is stamped here, in the same transaction that
-- created it, so no window exists in which one of them is unattributed.
--
-- Written as a loop over the object lists rather than as twenty literal COMMENT
-- statements so the marker appears exactly once more in this file, and so a
-- stamp cannot be forgotten when the lists change: the same lists the guard at
-- the head of the file checks are the lists stamped here.
--
-- Re-running this file re-issues identical comments, which is a no-op.
DO $provenance_stamp$
DECLARE
    -- Byte-identical to the guard at the head of this file and to the guard in
    -- rollback/0032_tenancy_tranche1_prepare_down.sql.
    marker CONSTANT TEXT :=
        'AEON tenancy tranche 1 (migration 0032). Provenance marker: rollback/0032 '
        'destroys only objects carrying exactly this comment, and refuses to touch '
        'any that do not.';
    target RECORD;
BEGIN
    FOR target IN
        SELECT * FROM (VALUES
              ('archival_batches', 'agent_uuid'), ('archival_batches', 'tenant_id'),
              ('audit_logs',       'agent_uuid'), ('audit_logs',       'tenant_id'),
              ('entities',         'agent_uuid'), ('entities',         'tenant_id'),
              ('memory_graph',     'agent_uuid'), ('memory_graph',     'tenant_id'),
              ('rmk_policies',     'agent_uuid'), ('rmk_policies',     'tenant_id')
        ) AS c(tbl, col)
    LOOP
        EXECUTE pg_catalog.format('COMMENT ON COLUMN public.%I.%I IS %L',
                                  target.tbl, target.col, marker);
    END LOOP;

    FOR target IN
        SELECT * FROM (VALUES
              ('fn_archival_batches_tenancy_bridge'),
              ('fn_audit_logs_tenancy_bridge'),
              ('fn_entities_tenancy_bridge'),
              ('fn_memory_graph_tenancy_bridge'),
              ('fn_rmk_policies_tenancy_bridge')
        ) AS f(fn)
    LOOP
        EXECUTE pg_catalog.format('COMMENT ON FUNCTION public.%I() IS %L',
                                  target.fn, marker);
    END LOOP;

    FOR target IN
        SELECT * FROM (VALUES
              ('trg_archival_batches_tenancy_bridge', 'archival_batches'),
              ('trg_audit_logs_tenancy_bridge',       'audit_logs'),
              ('trg_entities_tenancy_bridge',         'entities'),
              ('trg_memory_graph_tenancy_bridge',     'memory_graph'),
              ('trg_rmk_policies_tenancy_bridge',     'rmk_policies')
        ) AS t(trg, tbl)
    LOOP
        EXECUTE pg_catalog.format('COMMENT ON TRIGGER %I ON public.%I IS %L',
                                  target.trg, target.tbl, marker);
    END LOOP;

    -- The checkpoint table's idempotency stamp.
    --
    -- On the `id` COLUMN rather than the table, deliberately: the table already
    -- carries a documented `COMMENT ON TABLE` that a stamp would have to
    -- overwrite, and that prose is exactly why a marker was originally declined
    -- for this object.  A column comment leaves it intact.
    --
    -- Duplicated verbatim from the guard block at the head of this file.  This
    -- is idempotency provenance only: it says this migration created this
    -- table, so a re-run over its own rows is a no-op.  It says NOTHING about
    -- whether those rows are authentic -- a role that can create objects in
    -- `public` can write this comment too -- and it neither satisfies nor
    -- weakens any part of FINALIZE_PRECONDITION, which asserts its own
    -- conditions over checkpoint evidence independently.
    EXECUTE pg_catalog.format(
        'COMMENT ON COLUMN public.tenancy_backfill_checkpoints.id IS %L',
        'AEON tenancy tranche 1 (migration 0032). Idempotency stamp for the checkpoint '
        'table: it records that this migration created this table, so that a re-run over '
        'its own work is a no-op. It is NOT evidence that the rows are authentic.');
END $provenance_stamp$;

-- ── The caller's search_path, handed back ───────────────────────────────────
-- Everything this file creates or inspects is above this line, so the pin has
-- nothing left to protect.  It is released HERE rather than at COMMIT because
-- sqlx's own `INSERT INTO _sqlx_migrations` is unqualified and runs inside this
-- transaction -- see the note at the head of this file.  Transaction-local, so
-- a rollback still discards it.
SELECT pg_catalog.set_config('search_path',
                             pg_catalog.current_setting('aeon.saved_search_path'),
                             true);
