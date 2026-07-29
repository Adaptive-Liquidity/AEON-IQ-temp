set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Mirrors the CI "check" job in .github/workflows/ci.yml.
check:
    cargo fmt --check
    cargo check
    cargo clippy -- -D warnings
    cargo test -- --skip memory::store::tests --skip rmk_worker::tests --skip archival::tests::persist_archival --skip tenancy::db_tests --skip credentials::db_tests --skip credentials::grants::db_tests --skip tenancy::audit_db_tests

# Mirrors the database-backed contributor verification commands.
test-db:
    docker compose -f docker-compose.test.yml up --build -d
    python3 test_memory.py
    DATABASE_URL=postgresql://memoryos:memoryos_secret@localhost:5432/memoryos cargo test

# Runs the repository's existing full benchmark-proof verification sequence.
verify:
    bash benchmarks/scripts/ci_benchmark_proof.sh
