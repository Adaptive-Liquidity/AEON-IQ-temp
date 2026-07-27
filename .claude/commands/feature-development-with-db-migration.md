---
name: feature-development-with-db-migration
description: Workflow command scaffold for feature-development-with-db-migration in AEON-IQ-temp.
allowed_tools: ["Bash", "Read", "Write", "Grep", "Glob"]
---

# /feature-development-with-db-migration

Use this workflow when working on **feature-development-with-db-migration** in `AEON-IQ-temp`.

## Goal

Implements a new backend feature that requires both database schema changes and Rust code changes, including new modules, logic, and tests.

## Common Files

- `migrations/*.sql`
- `src/credentials/*.rs`
- `src/main.rs`
- `Cargo.toml`
- `Cargo.lock`
- `src/test_support.rs`

## Suggested Sequence

1. Understand the current state and failure mode before editing.
2. Make the smallest coherent change that satisfies the workflow goal.
3. Run the most relevant verification for touched files.
4. Summarize what changed and what still needs review.

## Typical Commit Signals

- Create or modify a SQL migration file in migrations/ to update the database schema.
- Add or update Rust module files under src/ (e.g., src/credentials/...) to implement the feature logic.
- Update Cargo.toml and Cargo.lock if new dependencies are needed.
- Write or update tests in src/credentials/db_tests.rs or similar test files.
- Integrate feature into main application logic (e.g., src/main.rs).

## Notes

- Treat this as a scaffold, not a hard-coded script.
- Update the command if the workflow evolves materially.