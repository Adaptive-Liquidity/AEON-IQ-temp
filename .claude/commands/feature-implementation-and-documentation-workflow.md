---
name: feature-implementation-and-documentation-workflow
description: Workflow command scaffold for feature-implementation-and-documentation-workflow in AEON-IQ-temp.
allowed_tools: ["Bash", "Read", "Write", "Grep", "Glob"]
---

# /feature-implementation-and-documentation-workflow

Use this workflow when working on **feature-implementation-and-documentation-workflow** in `AEON-IQ-temp`.

## Goal

Implements a new feature in the application logic, updates documentation, and ensures environment variables and CI are in sync.

## Common Files

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

- Implement feature logic in src/ (e.g., src/tenancy.rs, src/main.rs)
- Document feature and configuration in docs/ and .env.example
- Update CI workflow if new tests or environment variables are required

## Notes

- Treat this as a scaffold, not a hard-coded script.
- Update the command if the workflow evolves materially.