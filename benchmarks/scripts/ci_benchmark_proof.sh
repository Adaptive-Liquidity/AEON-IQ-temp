#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ARTIFACT_DIR="$ROOT/ci-artifacts/benchmark-proof"
LOG_DIR="$ARTIFACT_DIR/logs"
RESULTS_DIR="${BENCHMARK_RESULTS_DIR:-$ROOT/benchmarks/results/ci-local}"

mkdir -p "$LOG_DIR" "$RESULTS_DIR"
cd "$ROOT"

export BENCHMARK_RESULTS_DIR="$RESULTS_DIR"
export MANAGEMENT_API_KEY="${MANAGEMENT_API_KEY:-test-management-key}"
export ALLOW_UNAUTH_MANAGEMENT="${ALLOW_UNAUTH_MANAGEMENT:-false}"
export OPENAI_API_KEY="${OPENAI_API_KEY:-sk-mock-test-key-not-real}"
export UPSTREAM_PROVIDER="${UPSTREAM_PROVIDER:-openai}"
export UPSTREAM_BASE_URL="${UPSTREAM_BASE_URL:-http://localhost:11435}"
export EMBEDDING_BASE_URL="${EMBEDDING_BASE_URL:-http://localhost:11435}"
export EXTRACTOR_BASE_URL="${EXTRACTOR_BASE_URL:-http://localhost:11435}"
export RETRIEVAL_THRESHOLD="${RETRIEVAL_THRESHOLD:-0.95}"
export AMP_ENABLED="${AMP_ENABLED:-true}"
export RMK_ENABLED="${RMK_ENABLED:-true}"
export MAX_BODY_BYTES="${MAX_BODY_BYTES:-10485760}"
export DATABASE_URL="${DATABASE_URL:-postgresql://memoryos:memoryos_secret@localhost:5432/memoryos}"
export MOCK_EMBEDDING_MODE="${MOCK_EMBEDDING_MODE:-hash}"
export MOCK_ARCHIVAL_COMPACTION="${MOCK_ARCHIVAL_COMPACTION:-true}"

run_logged() {
  local name="$1"
  shift
  echo "==> $name"
  "$@" 2>&1 | tee "$LOG_DIR/$name.log"
}

copy_artifacts() {
  if [[ -d "$RESULTS_DIR" ]]; then
    mkdir -p "$ARTIFACT_DIR/benchmark-results"
    cp -a "$RESULTS_DIR"/. "$ARTIFACT_DIR/benchmark-results/"
  fi

  local summary="$RESULTS_DIR/summary.json"
  if [[ -f "$summary" ]]; then
    cp "$summary" "$ARTIFACT_DIR/summary.json"
  else
    find "$ROOT/benchmarks/results" -path "*/summary.json" -type f -print0 2>/dev/null \
      | xargs -0r ls -t \
      | head -n 1 \
      | xargs -r -I{} cp "{}" "$ARTIFACT_DIR/summary.json"
  fi
}

trap copy_artifacts EXIT

run_logged docker_compose_down docker compose -f docker-compose.test.yml down -v
run_logged docker_compose_up docker compose -f docker-compose.test.yml up --build -d
run_logged docker_compose_ps docker compose -f docker-compose.test.yml ps

run_logged cargo_fmt cargo fmt --check
run_logged cargo_clippy cargo clippy -- -D warnings
run_logged cargo_unit_skip_store cargo test -- --skip memory::store::tests --skip rmk_worker::tests
run_logged cargo_full_db env DATABASE_URL="$DATABASE_URL" cargo test
run_logged python_compileall python3 -m compileall -q benchmarks mock_openai_server.py run_tests.py test_memory.py
run_logged test_memory python3 test_memory.py
run_logged benchmark_run_all bash benchmarks/scripts/run_all.sh

copy_artifacts

python3 - <<'PY'
import json
import pathlib
import sys

summary_path = pathlib.Path("ci-artifacts/benchmark-proof/summary.json")
if not summary_path.exists():
    print(f"missing benchmark summary: {summary_path}", file=sys.stderr)
    sys.exit(1)

summary = json.loads(summary_path.read_text(encoding="utf-8"))
proof_status = summary.get("proof_status")
print(f"benchmark proof_status={proof_status}")
if proof_status != "pass":
    sys.exit(1)
PY
