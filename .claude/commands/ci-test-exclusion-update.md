---
name: ci-test-exclusion-update
description: Workflow command scaffold for ci-test-exclusion-update in AEON-IQ-temp.
allowed_tools: ["Bash", "Read", "Write", "Grep", "Glob"]
---

# /ci-test-exclusion-update

Use this workflow when working on **ci-test-exclusion-update** in `AEON-IQ-temp`.

## Goal

Updates CI workflow configuration to exclude certain tests from specific jobs (e.g., skipping DB tests in no-database jobs).

## Common Files

- `.github/workflows/ci.yml`

## Suggested Sequence

1. Understand the current state and failure mode before editing.
2. Make the smallest coherent change that satisfies the workflow goal.
3. Run the most relevant verification for touched files.
4. Summarize what changed and what still needs review.

## Typical Commit Signals

- Edit .github/workflows/ci.yml to add or modify test exclusion rules.
- Document the rationale in the commit message.

## Notes

- Treat this as a scaffold, not a hard-coded script.
- Update the command if the workflow evolves materially.