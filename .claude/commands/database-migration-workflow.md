---
name: database-migration-workflow
description: Workflow command scaffold for database-migration-workflow in AEON-IQ-temp.
allowed_tools: ["Bash", "Read", "Write", "Grep", "Glob"]
---

# /database-migration-workflow

Use this workflow when working on **database-migration-workflow** in `AEON-IQ-temp`.

## Goal

Adds or modifies database schema with forward and rollback migrations, and coordinates code changes to support new schema.

## Common Files

- `migrations/*.sql`
- `rollback/*.sql`
- `src/*.rs`
- `docs/*.md`
- `.env.example`
- `.github/workflows/ci.yml`

## Suggested Sequence

1. Understand the current state and failure mode before editing.
2. Make the smallest coherent change that satisfies the workflow goal.
3. Run the most relevant verification for touched files.
4. Summarize what changed and what still needs review.

## Typical Commit Signals

- Create or update forward migration SQL file in migrations/
- Create or update rollback migration SQL file in rollback/
- Update application logic to use new schema (e.g., src/tenancy.rs, src/main.rs)
- Document migration steps and environment variables in docs/ and .env.example
- Update CI workflow to accommodate schema changes if necessary

## Notes

- Treat this as a scaffold, not a hard-coded script.
- Update the command if the workflow evolves materially.