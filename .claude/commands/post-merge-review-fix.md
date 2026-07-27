---
name: post-merge-review-fix
description: Workflow command scaffold for post-merge-review-fix in AEON-IQ-temp.
allowed_tools: ["Bash", "Read", "Write", "Grep", "Glob"]
---

# /post-merge-review-fix

Use this workflow when working on **post-merge-review-fix** in `AEON-IQ-temp`.

## Goal

Addresses code review findings after a feature or refactor is merged, focusing on bug fixes, security, or correctness improvements.

## Common Files

- `src/credentials/*.rs`

## Suggested Sequence

1. Understand the current state and failure mode before editing.
2. Make the smallest coherent change that satisfies the workflow goal.
3. Run the most relevant verification for touched files.
4. Summarize what changed and what still needs review.

## Typical Commit Signals

- Identify and describe each issue in the commit message.
- Update relevant Rust source files to address the findings.
- Add or update regression/unit tests to verify the fixes.
- Commit with detailed notes about each fix.

## Notes

- Treat this as a scaffold, not a hard-coded script.
- Update the command if the workflow evolves materially.