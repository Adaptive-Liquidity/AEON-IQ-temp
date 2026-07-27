---
name: harden-schema-contract-and-verification
description: Workflow command scaffold for harden-schema-contract-and-verification in AEON-IQ-temp.
allowed_tools: ["Bash", "Read", "Write", "Grep", "Glob"]
---

# /harden-schema-contract-and-verification

Use this workflow when working on **harden-schema-contract-and-verification** in `AEON-IQ-temp`.

## Goal

Strengthen and verify database schema contracts for a security-sensitive table, ensuring all constraints, checks, and defaults are enforced and validated at startup.

## Common Files

- `src/credentials/store.rs`
- `src/credentials/db_tests.rs`
- `src/credentials/mod.rs`
- `src/main.rs`

## Suggested Sequence

1. Understand the current state and failure mode before editing.
2. Make the smallest coherent change that satisfies the workflow goal.
3. Run the most relevant verification for touched files.
4. Summarize what changed and what still needs review.

## Typical Commit Signals

- Identify weaknesses or bypasses in current schema validation logic.
- Update contract verification logic to check for stricter constraints (e.g., CHECK expressions, column defaults, primary keys, unique constraints).
- Update or add tests that demonstrate both the broken and fixed behaviors.
- Modify the main credential/authentication module to enforce the stricter contract at startup.
- Verify changes against a real or test database, ensuring startup fails closed on contract violations.

## Notes

- Treat this as a scaffold, not a hard-coded script.
- Update the command if the workflow evolves materially.