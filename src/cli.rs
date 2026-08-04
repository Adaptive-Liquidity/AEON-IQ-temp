//! The operator command line.
//!
//! `memoryos` with no subcommand starts the server, exactly as it always has.
//! A subcommand runs an operator task instead and exits — parsed and dispatched
//! in `main` **before** any startup check, because those checks are the server's
//! preconditions and not this command's.
//!
//! That ordering is the whole point of the module. A tenancy backfill needs a
//! database and nothing else: no upstream provider, no `OPENAI_API_KEY`, no
//! management key, no HTTP listener and no background workers. Running it
//! through `Config::from_env` would make it demand all of them, so an operator
//! migrating a database would have to invent provider credentials to satisfy a
//! check that has no bearing on the work. [`DbSettings::from_env`] therefore
//! reads only the database variables, and is the only configuration this path
//! touches.
//!
//! There is no backfill logic here. The engine is [`crate::tenancy::backfill`],
//! and this module parses arguments, calls it, and renders the result. A second
//! copy of the batching or the checkpoint lifecycle behind the CLI is exactly
//! what the library-first split exists to prevent — the management endpoint that
//! comes later is a third caller of the same function, not a third
//! implementation.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use sqlx::PgPool;

use crate::tenancy::backfill::{
    self, BackfillOptions, TrancheBackfillReport, DEFAULT_BATCH_SIZE, MAX_BATCH_SIZE,
    MIN_BACKFILL_CONNECTIONS,
};
use crate::tenancy::inventory::Tranche;

/// The reason recorded when an operator abandons a checkpoint without giving
/// one. The engine refuses an empty reason outright — an unexplained
/// `ABANDONED` row is history nobody can read — so the documented
/// `backfill-abandon --tranche tranche-1` invocation needs a default rather than
/// a way around the rule.
const DEFAULT_ABANDON_REASON: &str = "abandoned by operator via the tenancy CLI";

#[derive(Debug, Parser)]
#[command(
    name = "memoryos",
    version,
    about = "AEON-IQ memory kernel",
    long_about = "AEON-IQ memory kernel.\n\nWith no subcommand, starts the HTTP server. \
                  Subcommands run a single operator task and exit."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Tenancy migration operations.
    Tenancy {
        #[command(subcommand)]
        command: TenancyCommand,
    },
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum TenancyCommand {
    /// Resolve ownership for rows that predate the tranche's PREPARE migration.
    Backfill {
        #[arg(long, value_enum)]
        tranche: TrancheArg,
        /// Rows per batch.
        ///
        /// Bounded on both sides by clap so an out-of-range value is refused
        /// before a database connection is opened. The ceiling is not a tuning
        /// preference: the bridges sit on tables the application writes to, so
        /// batch size is how long one `UPDATE` holds row locks against the live
        /// write path.
        #[arg(
            long,
            default_value_t = DEFAULT_BATCH_SIZE,
            value_parser = clap::value_parser!(i64).range(1..=MAX_BATCH_SIZE),
        )]
        batch_size: i64,
        /// Stop after this many batches, leaving the checkpoint resumable.
        ///
        /// For bounding a maintenance window. The next run continues from the
        /// persisted cursor.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        max_batches: Option<u64>,
    },
    /// Report the checkpoint a tranche is currently sitting on.
    BackfillStatus {
        #[arg(long, value_enum)]
        tranche: TrancheArg,
    },
    /// Retire an in-progress checkpoint without completing it.
    BackfillAbandon {
        #[arg(long, value_enum)]
        tranche: TrancheArg,
        /// Why. Recorded in the log alongside the retired checkpoint.
        #[arg(long, default_value = DEFAULT_ABANDON_REASON)]
        reason: String,
    },
}

impl TenancyCommand {
    /// Connections this command needs before it is worth opening a pool.
    ///
    /// Only the backfill holds a connection for the length of the run: its
    /// session advisory lock sits on one while the batches use another. `status`
    /// reads a single row and `abandon` does its work inside one transaction
    /// with `pg_advisory_xact_lock`, so both are genuinely fine on a pool of
    /// one, and demanding two from them would refuse a deployment that can
    /// actually serve them.
    const fn min_connections(&self) -> u32 {
        match self {
            Self::Backfill { .. } => MIN_BACKFILL_CONNECTIONS,
            Self::BackfillStatus { .. } | Self::BackfillAbandon { .. } => 1,
        }
    }
}

/// The tranche argument.
///
/// Every tranche is accepted as a *value* even though only the first can run
/// today, and the refusal happens in dispatch instead. Restricting the enum to
/// one variant would make `--tranche tranche-2` fail with clap's generic
/// "invalid value ... [possible values: tranche-1]", which says the argument is
/// wrong rather than that the tranche has no schema yet. The variants mirror
/// [`Tranche`] one-for-one, and `every_tranche_has_a_cli_value` fails if they
/// stop doing so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TrancheArg {
    #[value(name = "tranche-1")]
    Tranche1,
    #[value(name = "tranche-2")]
    Tranche2,
    #[value(name = "tranche-3")]
    Tranche3,
    #[value(name = "tranche-4")]
    Tranche4,
    #[value(name = "tranche-5")]
    Tranche5,
    #[value(name = "final-constraint-tightening")]
    FinalConstraintTightening,
}

impl TrancheArg {
    pub const fn tranche(self) -> Tranche {
        match self {
            Self::Tranche1 => Tranche::RootsAndDirectAgentChildren,
            Self::Tranche2 => Tranche::Sessions,
            Self::Tranche3 => Tranche::Memories,
            Self::Tranche4 => Tranche::LineageAndArchival,
            Self::Tranche5 => Tranche::Operations,
            Self::FinalConstraintTightening => Tranche::FinalConstraintTightening,
        }
    }

    /// Resolve to a tranche this build can actually act on.
    ///
    /// Only tranche 1 has a PREPARE migration (`0032` and the index migrations
    /// after it). The others have no ownership columns and no bridges, so a
    /// backfill against them would have nothing to walk and no checkpoint that
    /// meant anything. Refusing here — with the reason — is the difference
    /// between an operator learning that the stage has not shipped and one
    /// watching a command report a clean completion over an empty plan.
    fn supported(self) -> Result<Tranche> {
        let tranche = self.tranche();
        if tranche != Tranche::RootsAndDirectAgentChildren {
            bail!(
                "only tranche 1 ({}) has shipped its PREPARE migration; {} has no ownership \
                 columns and no bridge triggers yet, so there is nothing for a backfill to \
                 resolve and a checkpoint for it would record work that does not exist",
                Tranche::RootsAndDirectAgentChildren,
                tranche,
            );
        }
        Ok(tranche)
    }
}

// ── Database-only configuration ──────────────────────────────────────────────

/// What an operator command needs from the environment, and nothing more.
///
/// Deliberately not `Config`: that type is the server's configuration and
/// requires an upstream provider, an embedding endpoint and a management-key
/// decision, none of which a database migration has any use for. Reading only
/// these four keeps "the backfill needs a database" a property of the code
/// rather than a claim in a comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbSettings {
    pub database_url: String,
    pub max_connections: u32,
    pub acquire_timeout_secs: u64,
    pub idle_timeout_secs: u64,
}

impl DbSettings {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_url: std::env::var("DATABASE_URL")
                .context("DATABASE_URL is required to run a tenancy command")?,
            // A smaller default pool than the server's: an operator command runs
            // its batches sequentially and holds one extra connection for the
            // advisory lock, so a server-sized pool would reserve connections it
            // can never use. Still >= MIN_BACKFILL_CONNECTIONS, so the default
            // can run every command.
            max_connections: positive("DB_MAX_CONNECTIONS", read_env, 4)?,
            acquire_timeout_secs: positive("DB_ACQUIRE_TIMEOUT_SECS", read_env, 5)?,
            idle_timeout_secs: positive("DB_IDLE_TIMEOUT_SECS", read_env, 300)?,
        })
    }
}

fn read_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Parse a positive integer, and mean it.
///
/// `u32` and `u64` both parse `"0"` quite happily, so the error text's promise
/// of "a positive integer" used to be advisory. Zero is not a harmless setting
/// for any of the three variables this reads: `DB_MAX_CONNECTIONS=0` builds a
/// pool that can never hand out a connection, so the command fails an acquire
/// timeout several seconds later instead of a configuration error immediately,
/// and a zero timeout is a pool that gives up before it has tried.
///
/// The lookup is a parameter rather than a direct `std::env::var` so the rule
/// can be tested without mutating process-global state — env-var tests race
/// against every other test in the binary, and a flaky guard is worse than the
/// bug it guards.
fn positive<T>(key: &str, lookup: impl Fn(&str) -> Option<String>, default: T) -> Result<T>
where
    T: std::str::FromStr + PartialEq + Default + std::fmt::Display,
    T::Err: std::fmt::Display,
{
    let Some(raw) = lookup(key) else {
        return Ok(default);
    };
    let value: T = raw
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("{key} must be a positive integer, got {raw:?}: {e}"))?;
    if value == T::default() {
        bail!(
            "{key} must be a positive integer, got {value}. Zero would build a pool that can \
             never serve a request, which surfaces as an acquire timeout rather than as the \
             configuration error it is."
        );
    }
    Ok(value)
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

/// Run one operator command against a database built from the environment.
///
/// Migrations are **not** run here. BACKFILL is the stage after PREPARE, so the
/// schema it needs is already deployed by definition; a CLI that migrated on the
/// way past would let an operator apply schema changes by asking for a status
/// report. A missing checkpoint table is reported as the missing stage it is.
pub async fn dispatch(command: Command) -> Result<()> {
    let settings = DbSettings::from_env()?;

    // Checked before the pool is built, so an under-provisioned deployment gets
    // a sentence naming the variable rather than an acquire timeout several
    // seconds later. The engine enforces its own minimum too — this one exists
    // so the operator hears about it before anything connects.
    let Command::Tenancy { command: ref inner } = command;
    let required = inner.min_connections();
    if settings.max_connections < required {
        bail!(
            "this command needs at least {required} database connection(s) but \
             DB_MAX_CONNECTIONS is {}. Raise it to {required} or more.",
            settings.max_connections,
        );
    }

    let pool = crate::db::connect(
        &settings.database_url,
        settings.max_connections,
        settings.acquire_timeout_secs,
        settings.idle_timeout_secs,
    )
    .await
    .context("connecting to the database")?;

    let rendered = run(&pool, command).await;
    pool.close().await;
    println!("{}", rendered?);
    Ok(())
}

/// The dispatch body, against an already-open pool.
///
/// Split from [`dispatch`] so the database tests can drive the real command path
/// against the isolated database `#[sqlx::test]` gives them, rather than
/// asserting against a hand-rolled imitation of it. The rendered report is
/// returned instead of printed for the same reason: what an operator reads is
/// then the thing under test.
pub async fn run(pool: &PgPool, command: Command) -> Result<String> {
    let Command::Tenancy { command } = command;

    match command {
        TenancyCommand::Backfill {
            tranche,
            batch_size,
            max_batches,
        } => {
            let tranche = tranche.supported()?;
            require_prepare_applied(pool).await?;
            let report = backfill::run_tranche_backfill(
                pool,
                tranche,
                BackfillOptions {
                    batch_size,
                    max_batches,
                    // The reconciliation timeouts are engine defaults. The CLI
                    // carries no opinion of its own about them, so there is no
                    // second place for them to drift from.
                    ..BackfillOptions::default()
                },
            )
            .await?;
            Ok(render_run(&report))
        }

        TenancyCommand::BackfillStatus { tranche } => {
            let tranche = tranche.supported()?;
            require_prepare_applied(pool).await?;
            match backfill::tranche_backfill_status(pool, tranche).await? {
                Some(row) => Ok(render_checkpoint(tranche, &row)),
                None => Ok(format!(
                    "tranche:          {tranche}\n\
                     checkpoint:       none — no backfill has been started for this tranche"
                )),
            }
        }

        TenancyCommand::BackfillAbandon { tranche, reason } => {
            let tranche = tranche.supported()?;
            require_prepare_applied(pool).await?;
            match backfill::abandon_tranche_backfill(pool, tranche, &reason).await? {
                Some(row) => Ok(render_checkpoint(tranche, &row)),
                None => Ok(format!(
                    "tranche:          {tranche}\n\
                     abandoned:        nothing — no in-progress checkpoint for this tranche"
                )),
            }
        }
    }
}

/// Refuse with the missing stage named, rather than with a bare `42P01`.
///
/// The checkpoint table is created by PREPARE. Its absence means the migration
/// has not been applied to this database, which is a deployment-ordering fact an
/// operator can act on; "relation does not exist" is not.
async fn require_prepare_applied(pool: &PgPool) -> Result<()> {
    let present: bool =
        sqlx::query_scalar("SELECT to_regclass('public.tenancy_backfill_checkpoints') IS NOT NULL")
            .fetch_one(pool)
            .await
            .context("checking whether the tranche PREPARE migration has been applied")?;

    if !present {
        bail!(
            "`tenancy_backfill_checkpoints` does not exist, so the tranche PREPARE migration \
             (0032) has not been applied to this database. BACKFILL is the stage after PREPARE; \
             deploy the kernel so migrations run, then retry."
        );
    }
    Ok(())
}

// ── Rendering ────────────────────────────────────────────────────────────────

fn or_dash(value: Option<&str>) -> &str {
    value.unwrap_or("-")
}

fn render_run(report: &TrancheBackfillReport) -> String {
    let mut out = format!(
        "tranche:          {}\n\
         outcome:          {}\n\
         checkpoint:       {}\n\
         status:           {}\n\
         contract digest:  {}\n\
         rows total:       {}\n\
         rows backfilled:  {}\n\
         settled this run: {}\n\
         batches executed: {}\n\
         blocking count:   {}\n\
         resume cursor:    {}\n\
         finalizable:      {}",
        report.tranche,
        report.outcome.as_str(),
        report.checkpoint_id,
        report.status.as_str(),
        report.contract_digest,
        report.rows_total,
        report.rows_backfilled,
        report.rows_settled_this_run,
        report.batches_executed,
        report.blocking_count,
        or_dash(report.resume_cursor.as_deref()),
        report.is_finalizable(),
    );

    // The reasons, not just the count. A blocked run whose output is a bare
    // number sends an operator back to re-run the audit by hand to learn what
    // the backfill already knows.
    if !report.blocking_reasons.is_empty() {
        out.push_str("\nblocking reasons:");
        for reason in &report.blocking_reasons {
            out.push_str("\n  - ");
            out.push_str(reason);
        }
    }
    out
}

/// One checkpoint, in the same field order and alignment as [`render_run`].
///
/// The action that produced it is *not* a field. An earlier version made the
/// caller's label the first field name, which rendered `backfill-status` with
/// two different lines both labelled `status:` -- the checkpoint id under one
/// and the lifecycle status under the other. `status` is the checkpoint's own
/// column and `ABANDONED` already says what happened to it, so the label had
/// nothing to add and one thing to collide with.
fn render_checkpoint(tranche: Tranche, row: &backfill::CheckpointRow) -> String {
    format!(
        "tranche:          {tranche}\n\
         checkpoint:       {id}\n\
         status:           {status}\n\
         contract digest:  {digest}\n\
         rows total:       {total}\n\
         rows backfilled:  {done}\n\
         blocking count:   {blocking}\n\
         resume cursor:    {cursor}",
        id = row.id,
        status = row.status,
        digest = row.contract_digest,
        total = row.rows_total,
        done = row.rows_backfilled,
        blocking = row.blocking_count,
        cursor = or_dash(row.resume_cursor.as_deref()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn the_parser_itself_is_well_formed() {
        // Catches duplicated long flags, conflicting defaults and the like at
        // test time rather than on an operator's first invocation.
        Cli::command().debug_assert();
    }

    #[test]
    fn no_subcommand_still_means_start_the_server() {
        let cli = parse(&["memoryos"]).expect("bare invocation parses");
        assert_eq!(
            cli.command, None,
            "a bare `memoryos` must stay the server entry point"
        );
    }

    #[test]
    fn unknown_arguments_are_refused_rather_than_ignored() {
        assert!(parse(&["memoryos", "--not-a-flag"]).is_err());
        assert!(parse(&["memoryos", "tenancy", "not-a-command"]).is_err());
    }

    #[test]
    fn the_three_documented_invocations_parse() {
        let cli = parse(&[
            "memoryos",
            "tenancy",
            "backfill",
            "--tranche",
            "tranche-1",
            "--batch-size",
            "500",
        ])
        .expect("backfill parses");
        assert_eq!(
            cli.command,
            Some(Command::Tenancy {
                command: TenancyCommand::Backfill {
                    tranche: TrancheArg::Tranche1,
                    batch_size: 500,
                    max_batches: None,
                }
            })
        );

        let cli = parse(&[
            "memoryos",
            "tenancy",
            "backfill-status",
            "--tranche",
            "tranche-1",
        ])
        .expect("status parses");
        assert_eq!(
            cli.command,
            Some(Command::Tenancy {
                command: TenancyCommand::BackfillStatus {
                    tranche: TrancheArg::Tranche1
                }
            })
        );

        // Documented without `--reason`, so the default has to make the command
        // usable rather than trip the engine's empty-reason refusal.
        let cli = parse(&[
            "memoryos",
            "tenancy",
            "backfill-abandon",
            "--tranche",
            "tranche-1",
        ])
        .expect("abandon parses");
        assert_eq!(
            cli.command,
            Some(Command::Tenancy {
                command: TenancyCommand::BackfillAbandon {
                    tranche: TrancheArg::Tranche1,
                    reason: DEFAULT_ABANDON_REASON.to_string(),
                }
            })
        );
        assert!(
            !DEFAULT_ABANDON_REASON.trim().is_empty(),
            "the default reason must satisfy the engine's refusal of an empty one"
        );
    }

    #[test]
    fn batch_size_is_bounded_on_both_sides_before_a_connection_is_opened() {
        for bad in ["0", "-1", &(MAX_BATCH_SIZE + 1).to_string()] {
            assert!(
                parse(&[
                    "memoryos",
                    "tenancy",
                    "backfill",
                    "--tranche",
                    "tranche-1",
                    "--batch-size",
                    bad,
                ])
                .is_err(),
                "batch size {bad} must be refused by the parser"
            );
        }

        for good in ["1", &MAX_BATCH_SIZE.to_string()] {
            assert!(
                parse(&[
                    "memoryos",
                    "tenancy",
                    "backfill",
                    "--tranche",
                    "tranche-1",
                    "--batch-size",
                    good,
                ])
                .is_ok(),
                "batch size {good} is in range and must be accepted"
            );
        }
    }

    #[test]
    fn the_default_batch_size_is_the_engines_own() {
        let cli = parse(&["memoryos", "tenancy", "backfill", "--tranche", "tranche-1"])
            .expect("parses without an explicit batch size");
        let Some(Command::Tenancy {
            command: TenancyCommand::Backfill { batch_size, .. },
        }) = cli.command
        else {
            panic!("expected a backfill command");
        };
        assert_eq!(
            batch_size, DEFAULT_BATCH_SIZE,
            "the CLI must not carry a second opinion about the default batch size"
        );
    }

    #[test]
    fn max_batches_must_be_positive() {
        assert!(parse(&[
            "memoryos",
            "tenancy",
            "backfill",
            "--tranche",
            "tranche-1",
            "--max-batches",
            "0",
        ])
        .is_err());
        assert!(parse(&[
            "memoryos",
            "tenancy",
            "backfill",
            "--tranche",
            "tranche-1",
            "--max-batches",
            "3",
        ])
        .is_ok());
    }

    #[test]
    fn every_tranche_has_a_cli_value() {
        // The argument enum mirrors `Tranche`, so a tranche added to the plan
        // without a CLI value fails here rather than becoming unreachable from
        // the command line.
        let mapped: Vec<Tranche> = [
            TrancheArg::Tranche1,
            TrancheArg::Tranche2,
            TrancheArg::Tranche3,
            TrancheArg::Tranche4,
            TrancheArg::Tranche5,
            TrancheArg::FinalConstraintTightening,
        ]
        .into_iter()
        .map(TrancheArg::tranche)
        .collect();
        assert_eq!(mapped, Tranche::ALL.to_vec());
    }

    #[test]
    fn only_tranche_one_is_supported_and_the_refusal_says_why() {
        assert_eq!(
            TrancheArg::Tranche1.supported().expect("tranche 1 runs"),
            Tranche::RootsAndDirectAgentChildren
        );

        for arg in [
            TrancheArg::Tranche2,
            TrancheArg::Tranche3,
            TrancheArg::Tranche4,
            TrancheArg::Tranche5,
            TrancheArg::FinalConstraintTightening,
        ] {
            let err = arg
                .supported()
                .expect_err("only tranche 1 has shipped its PREPARE migration");
            let message = err.to_string();
            assert!(
                message.contains("PREPARE") && message.contains(arg.tranche().as_str()),
                "the refusal must name the stage and the tranche: {message}"
            );
        }
    }

    #[test]
    fn an_unknown_tranche_value_is_refused() {
        assert!(parse(&["memoryos", "tenancy", "backfill", "--tranche", "tranche-99"]).is_err());
        assert!(
            parse(&["memoryos", "tenancy", "backfill"]).is_err(),
            "--tranche is required"
        );
    }

    #[test]
    fn zero_is_refused_for_every_pool_setting() {
        // `u32`/`u64` parse "0" successfully, so nothing but this check stands
        // between DB_MAX_CONNECTIONS=0 and a pool that can never hand out a
        // connection. The failure that produces is an acquire timeout seconds
        // later, which reads as a busy database rather than a typo.
        for key in [
            "DB_MAX_CONNECTIONS",
            "DB_ACQUIRE_TIMEOUT_SECS",
            "DB_IDLE_TIMEOUT_SECS",
        ] {
            let err = positive::<u64>(key, |_| Some("0".into()), 7).expect_err("zero is refused");
            let message = err.to_string();
            assert!(
                message.contains(key) && message.contains("positive"),
                "the refusal must name the variable and the rule: {message}"
            );
        }
    }

    #[test]
    fn positive_accepts_values_and_falls_back_to_the_default() {
        assert_eq!(positive::<u32>("K", |_| Some("12".into()), 4).unwrap(), 12);
        assert_eq!(positive::<u32>("K", |_| None, 4).unwrap(), 4);
        assert_eq!(positive::<u32>("K", |_| Some(" 9 ".into()), 4).unwrap(), 9);
        assert!(positive::<u32>("K", |_| Some("nope".into()), 4).is_err());
        assert!(positive::<u32>("K", |_| Some("-1".into()), 4).is_err());
    }

    #[test]
    fn only_the_backfill_demands_a_second_connection() {
        // The backfill parks one connection on the session advisory lock for the
        // whole run. `status` and `abandon` do not, and demanding two from them
        // would refuse a deployment that can actually serve them.
        assert_eq!(
            TenancyCommand::Backfill {
                tranche: TrancheArg::Tranche1,
                batch_size: DEFAULT_BATCH_SIZE,
                max_batches: None,
            }
            .min_connections(),
            MIN_BACKFILL_CONNECTIONS,
        );
        const { assert!(MIN_BACKFILL_CONNECTIONS >= 2, "one connection is the bug") };

        for command in [
            TenancyCommand::BackfillStatus {
                tranche: TrancheArg::Tranche1,
            },
            TenancyCommand::BackfillAbandon {
                tranche: TrancheArg::Tranche1,
                reason: "r".into(),
            },
        ] {
            assert_eq!(command.min_connections(), 1, "{command:?}");
        }
    }

    #[test]
    fn the_default_pool_can_run_every_command() {
        // The default has to satisfy the strictest command, or an operator who
        // sets nothing at all gets a refusal for a pool they never chose.
        let default_max = positive::<u32>("DB_MAX_CONNECTIONS", |_| None, 4).unwrap();
        assert!(
            default_max >= MIN_BACKFILL_CONNECTIONS,
            "default pool of {default_max} cannot run a backfill"
        );
    }

    #[test]
    fn no_rendered_report_repeats_a_field_label() {
        // `backfill-status` once printed the checkpoint id under a `status:`
        // label and the lifecycle status under a second one, so the same output
        // said `status:` twice and meant two different things. Field labels are
        // the only structure this output has; a duplicate makes it ambiguous to
        // read and impossible to grep.
        let checkpoint = backfill::CheckpointRow {
            id: uuid::Uuid::nil(),
            tranche: Tranche::RootsAndDirectAgentChildren.as_str().to_string(),
            contract_digest: "sha256:abc".into(),
            status: "ABANDONED".into(),
            resume_cursor: Some("entities|".into()),
            rows_total: 4,
            rows_backfilled: 2,
            blocking_count: 0,
        };

        for rendered in [
            render_checkpoint(Tranche::RootsAndDirectAgentChildren, &checkpoint),
            render_run(&blocked_report()),
        ] {
            let mut labels: Vec<&str> = rendered
                .lines()
                .filter_map(|l| l.split_once(':').map(|(label, _)| label))
                .filter(|label| !label.starts_with("  "))
                .collect();
            let before = labels.len();
            labels.sort_unstable();
            labels.dedup();
            assert_eq!(
                labels.len(),
                before,
                "a field label is repeated in:\n{rendered}"
            );
        }
    }

    fn blocked_report() -> TrancheBackfillReport {
        TrancheBackfillReport {
            tranche: Tranche::RootsAndDirectAgentChildren,
            contract_digest: "sha256:abc".into(),
            checkpoint_id: uuid::Uuid::nil(),
            status: crate::tenancy::plan::CheckpointStatus::InProgress,
            outcome: backfill::BackfillOutcome::Blocked,
            rows_total: 10,
            rows_backfilled: 7,
            blocking_count: 1,
            blocking_reasons: vec!["entities: LEGACY_UNMAPPED".into()],
            resume_cursor: None,
            batches_executed: 2,
            rows_settled_this_run: 7,
        }
    }

    #[test]
    fn the_rendered_report_carries_every_field_an_operator_needs() {
        let rendered = render_run(&blocked_report());

        for expected in [
            "TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN",
            "BLOCKED",
            "IN_PROGRESS",
            "sha256:abc",
            "rows total:       10",
            "rows backfilled:  7",
            "blocking count:   1",
            "resume cursor:    -",
            "entities: LEGACY_UNMAPPED",
        ] {
            assert!(
                rendered.contains(expected),
                "rendered report is missing {expected:?}:\n{rendered}"
            );
        }
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;

    const TENANT: &str = "11111111-1111-1111-1111-111111111111";

    fn backfill_cmd(batch_size: i64, max_batches: Option<u64>) -> Command {
        Command::Tenancy {
            command: TenancyCommand::Backfill {
                tranche: TrancheArg::Tranche1,
                batch_size,
                max_batches,
            },
        }
    }

    fn status_cmd() -> Command {
        Command::Tenancy {
            command: TenancyCommand::BackfillStatus {
                tranche: TrancheArg::Tranche1,
            },
        }
    }

    fn abandon_cmd() -> Command {
        Command::Tenancy {
            command: TenancyCommand::BackfillAbandon {
                tranche: TrancheArg::Tranche1,
                reason: DEFAULT_ABANDON_REASON.to_string(),
            },
        }
    }

    /// Seed entities that predate PREPARE, with the bridge disabled.
    async fn seed_unowned_entities(pool: &PgPool, count: usize) {
        sqlx::query(
            "INSERT INTO agents (agent_id, tenant_id, external_agent_id) \
             VALUES ($1, $2::uuid, $3)",
        )
        .bind("agent-a")
        .bind(TENANT)
        .bind("ext-agent-a")
        .execute(pool)
        .await
        .expect("insert agent");

        sqlx::query("ALTER TABLE public.entities DISABLE TRIGGER USER")
            .execute(pool)
            .await
            .expect("disable bridge");
        for _ in 0..count {
            sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, $2, 'x')")
                .bind("agent-a")
                .bind(uuid::Uuid::new_v4().to_string())
                .execute(pool)
                .await
                .expect("seed entity");
        }
        sqlx::query("ALTER TABLE public.entities ENABLE TRIGGER USER")
            .execute(pool)
            .await
            .expect("re-enable bridge");
    }

    async fn unowned_entities(pool: &PgPool) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*)::bigint FROM entities WHERE tenant_id IS NULL OR agent_uuid IS NULL",
        )
        .fetch_one(pool)
        .await
        .expect("count")
    }

    #[sqlx::test]
    async fn the_cli_pauses_resumes_and_then_reports_a_completed_rerun(pool: PgPool) {
        seed_unowned_entities(&pool, 9).await;

        // Nothing started yet.
        let before = run(&pool, status_cmd()).await.expect("status runs");
        assert!(
            before.contains("checkpoint:       none"),
            "expected no checkpoint before the first run:\n{before}"
        );

        // A bounded run stops with work left and prints a cursor.
        let paused = run(&pool, backfill_cmd(2, Some(2)))
            .await
            .expect("bounded run");
        assert!(paused.contains("outcome:          PAUSED"), "{paused}");
        assert!(paused.contains("status:           IN_PROGRESS"), "{paused}");
        assert!(
            paused.contains("resume cursor:    entities|"),
            "a paused run must print the cursor it stopped on:\n{paused}"
        );
        assert!(paused.contains("settled this run: 4"), "{paused}");
        assert_eq!(unowned_entities(&pool).await, 5);

        // `backfill-status` reports the same open checkpoint.
        let mid = run(&pool, status_cmd()).await.expect("status runs");
        assert!(mid.contains("IN_PROGRESS"), "{mid}");
        assert!(mid.contains("resume cursor:    entities|"), "{mid}");

        // Resuming finishes the remaining five and only the remaining five,
        // which is what proves the second invocation started from the persisted
        // cursor rather than from the beginning.
        let done = run(&pool, backfill_cmd(2, None))
            .await
            .expect("resumed run");
        assert!(done.contains("outcome:          COMPLETED"), "{done}");
        assert!(done.contains("settled this run: 5"), "{done}");
        assert!(done.contains("blocking count:   0"), "{done}");
        assert!(done.contains("finalizable:      true"), "{done}");
        assert!(done.contains("resume cursor:    -"), "{done}");
        assert_eq!(unowned_entities(&pool).await, 0);

        // A third invocation is a no-op rather than a second completion.
        let rerun = run(&pool, backfill_cmd(2, None)).await.expect("rerun");
        assert!(
            rerun.contains("outcome:          ALREADY_COMPLETED"),
            "{rerun}"
        );
        assert!(rerun.contains("batches executed: 0"), "{rerun}");
        assert!(rerun.contains("settled this run: 0"), "{rerun}");

        let count: i64 = sqlx::query_scalar(
            "SELECT count(*)::bigint FROM tenancy_backfill_checkpoints WHERE status = 'COMPLETED'",
        )
        .fetch_one(&pool)
        .await
        .expect("count completions");
        assert_eq!(count, 1, "a rerun must not raise a second completion");
    }

    #[sqlx::test]
    async fn the_cli_abandons_an_open_checkpoint_and_reports_when_there_is_none(pool: PgPool) {
        seed_unowned_entities(&pool, 6).await;

        let none = run(&pool, abandon_cmd()).await.expect("abandon runs");
        assert!(
            none.contains("abandoned:        nothing"),
            "with no open checkpoint the command must say so:\n{none}"
        );

        run(&pool, backfill_cmd(2, Some(1)))
            .await
            .expect("bounded run");

        let abandoned = run(&pool, abandon_cmd()).await.expect("abandon runs");
        assert!(abandoned.contains("ABANDONED"), "{abandoned}");
        assert!(
            abandoned.contains("resume cursor:    entities|"),
            "an abandoned checkpoint keeps how far it got:\n{abandoned}"
        );

        // And the tranche is startable again from a fresh checkpoint.
        let fresh = run(&pool, backfill_cmd(10, None)).await.expect("fresh run");
        assert!(fresh.contains("outcome:          COMPLETED"), "{fresh}");
        assert_eq!(unowned_entities(&pool).await, 0);
    }

    #[sqlx::test]
    async fn a_tranche_other_than_one_is_refused_before_any_database_work(pool: PgPool) {
        seed_unowned_entities(&pool, 3).await;

        let err = run(
            &pool,
            Command::Tenancy {
                command: TenancyCommand::Backfill {
                    tranche: TrancheArg::Tranche3,
                    batch_size: 10,
                    max_batches: None,
                },
            },
        )
        .await
        .expect_err("only tranche 1 has shipped");
        assert!(err.to_string().contains("TRANCHE_3_MEMORIES"), "{err}");

        assert_eq!(
            unowned_entities(&pool).await,
            3,
            "the refusal must precede any write"
        );
        let checkpoints: i64 =
            sqlx::query_scalar("SELECT count(*)::bigint FROM tenancy_backfill_checkpoints")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(checkpoints, 0, "a refused tranche must open no checkpoint");
    }

    #[sqlx::test]
    async fn a_blocked_backfill_reports_its_reasons_rather_than_only_a_count(pool: PgPool) {
        sqlx::query("INSERT INTO agents (agent_id, external_agent_id) VALUES ($1, $2)")
            .bind("agent-unmapped")
            .bind("ext-unmapped")
            .execute(&pool)
            .await
            .expect("insert unmapped agent");
        sqlx::query("ALTER TABLE public.entities DISABLE TRIGGER USER")
            .execute(&pool)
            .await
            .expect("disable bridge");
        sqlx::query(
            "INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, 'e', 'person')",
        )
        .bind("agent-unmapped")
        .execute(&pool)
        .await
        .expect("seed entity");
        sqlx::query("ALTER TABLE public.entities ENABLE TRIGGER USER")
            .execute(&pool)
            .await
            .expect("re-enable bridge");

        let blocked = run(&pool, backfill_cmd(10, None))
            .await
            .expect("backfill runs");
        assert!(blocked.contains("outcome:          BLOCKED"), "{blocked}");
        assert!(blocked.contains("finalizable:      false"), "{blocked}");
        assert!(
            blocked.contains("blocking reasons:") && blocked.contains("LEGACY_UNMAPPED"),
            "a blocked run must name what blocked it:\n{blocked}"
        );
    }

    #[sqlx::test]
    async fn a_database_without_prepare_is_told_which_stage_is_missing(pool: PgPool) {
        // Simulates a database the PREPARE migration has not reached.
        sqlx::query("DROP TABLE public.tenancy_backfill_checkpoints")
            .execute(&pool)
            .await
            .expect("drop the checkpoint table");

        for command in [backfill_cmd(10, None), status_cmd(), abandon_cmd()] {
            let err = run(&pool, command)
                .await
                .expect_err("without PREPARE there is nothing to back fill");
            let message = err.to_string();
            assert!(
                message.contains("PREPARE") && message.contains("0032"),
                "the refusal must name the missing migration: {message}"
            );
        }
    }
}
