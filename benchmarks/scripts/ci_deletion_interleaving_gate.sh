#!/usr/bin/env bash
#
# The serialized lane for issue #27.
#
# `a_policy_inserted_during_cleanup_cannot_break_agent_deletion` is `#[ignore]`d
# out of the ordinary parallel suite because at high `--test-threads` the
# interleaving its proof depends on is not reliably *exercised* -- `delete_agent`
# acquires a pooled connection and then sits idle without sending its `BEGIN`, so
# it is genuinely unblocked and the strict readiness gate correctly refuses.
#
# The gate itself is UNCHANGED and stays strict. This script is the reason
# ignoring it is not the same as skipping it.
#
# WHY THIS IS A SCRIPT AND NOT A ONE-LINE `run:` STEP
#
# `cargo test` exits 0 when its filter matches NOTHING. Measured, not assumed:
#
#   $ cargo test a_policy_inserted_during_cleanup_cannot_break_agent_deletion \
#         -- --ignored --exact
#   running 0 tests
#   test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 519 filtered out
#   $ echo $?
#   0
#
# `--exact` matches the FULL module path, so the bare function name selects
# nothing and the job goes green having run no test at all. The same silent pass
# is produced by renaming the test, moving it to another module, or REMOVING the
# `#[ignore]` attribute -- `--ignored` runs only ignored tests, so an un-ignored
# test is filtered out and this lane would quietly stop covering it while the
# parallel suite picked it up again along with its flake.
#
# Every one of those is a silent skip, so this script does not trust the exit
# code. It requires positive evidence that this exact test ran and passed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

# The full module path, because `--exact` matches nothing less.
TEST="tenancy::tranche1_db_tests::a_policy_inserted_during_cleanup_cannot_break_agent_deletion"

ARTIFACT_DIR="$ROOT/ci-artifacts/deletion-interleaving-gate"
mkdir -p "$ARTIFACT_DIR"
LOG="$ARTIFACT_DIR/serialized-run.log"

echo "==> $TEST (serialized: --ignored --exact --test-threads=1)"

# The exit code is captured rather than allowed to abort, so a FAILING test still
# reaches the evidence checks below and is reported as a test failure rather than
# as a bare non-zero exit.
status=0
cargo test "$TEST" -- --ignored --exact --test-threads=1 2>&1 | tee "$LOG" || status=$?

# EVIDENCE 1: this exact test ran, and passed.
if ! grep -qxF "test ${TEST} ... ok" "$LOG"; then
    echo "::error::the serialized lane did not observe '${TEST}' run and pass." >&2
    echo "Its exit status was ${status}. cargo test exits 0 on an empty filter, so" >&2
    echo "a green exit here is not evidence. Check that the test still exists under" >&2
    echo "this exact module path and is still marked #[ignore] -- both are required" >&2
    echo "for '--ignored --exact' to select it." >&2
    exit 1
fi

# EVIDENCE 2: exactly one test ran, in exactly one test binary. Guards the other
# direction -- a filter that widened to match siblings would mean this lane is no
# longer serializing only what it claims to, and '1 passed; 0 failed' is the only
# shape that is.
#
# COUNTED, not merely matched. CodeRabbit, on `6463ca0`: a presence test is
# satisfied by the FIRST matching summary, so if `cargo test` ever produced more
# than one test binary -- a second `[[bin]]`, a `[lib]`, an integration test
# target -- each could report its own `1 passed; 0 failed` and this lane would
# pass while running the test more than once, single-threaded within each binary
# but not once overall. Not reachable in this crate today (one `[[bin]]`, no
# `[lib]`, so one binary and one summary line), which is exactly why it would go
# unnoticed the day a target is added.
result_lines="$(grep -cE "^test result: ok\. 1 passed; 0 failed;" "$LOG" || true)"
if [[ "$result_lines" -ne 1 ]]; then
    echo "::error::the serialized lane must run exactly one test, in exactly one" >&2
    echo "test binary, and pass it -- found ${result_lines} matching result lines," >&2
    echo "expected 1." >&2
    grep -E "^test result:" "$LOG" >&2 || echo "(no 'test result:' line at all)" >&2
    exit 1
fi

# Belt and braces: having proven the test ran and passed, a non-zero exit would
# mean something else in the harness failed, and that is still a failure.
if [[ "$status" -ne 0 ]]; then
    echo "::error::'${TEST}' passed but cargo exited ${status}." >&2
    exit "$status"
fi

echo "==> serialized lane OK: ${TEST} ran once, single-threaded, and passed"
