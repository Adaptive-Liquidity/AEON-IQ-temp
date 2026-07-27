```markdown
# AEON-IQ-temp Development Patterns

> Auto-generated skill from repository analysis

## Overview
This skill teaches the core development conventions and workflows used in the AEON-IQ-temp Rust codebase. You'll learn how to structure files, write and organize code, follow commit message standards, and implement tests according to the repository's patterns. This guide is ideal for contributors aiming for consistency and maintainability in AEON-IQ-temp.

## Coding Conventions

### File Naming
- Use **camelCase** for file names.
  - Example: `userProfile.rs`, `dataFetcher.rs`

### Import Style
- Use **relative imports** for modules within the project.
  - Example:
    ```rust
    mod utils;
    use crate::helpers::mathUtils;
    ```

### Export Style
- Use **named exports** for exposing functions, structs, or modules.
  - Example:
    ```rust
    pub fn calculate_score() { ... }
    pub struct UserProfile { ... }
    ```

### Commit Messages
- Follow the **Conventional Commits** format.
- Use the `fix` prefix for bug fixes.
  - Example:
    ```
    fix: resolve panic in dataFetcher when input is empty
    ```

## Workflows

### Commit Changes
**Trigger:** When making any code changes that need to be committed.
**Command:** `/commit-changes`

1. Stage your changes:
    ```
    git add .
    ```
2. Write a commit message using the conventional format (e.g., `fix: <description>`):
    ```
    git commit -m "fix: correct calculation in userProfile"
    ```
3. Push your changes:
    ```
    git push
    ```

### Run Tests
**Trigger:** Before pushing changes or to verify code correctness.
**Command:** `/run-tests`

1. Identify test files (pattern: `*.test.*`).
2. Run tests using the Rust test runner:
    ```
    cargo test
    ```
3. Review output and address any failures.

## Testing Patterns

- Test files follow the `*.test.*` naming convention (e.g., `userProfile.test.rs`).
- The specific testing framework is not detected, but Rust's built-in test runner is commonly used.
- Example test function:
    ```rust
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_calculate_score() {
            assert_eq!(calculate_score(2, 3), 5);
        }
    }
    ```

## Commands
| Command         | Purpose                                           |
|-----------------|---------------------------------------------------|
| /commit-changes | Guide for staging, committing, and pushing code   |
| /run-tests      | Steps to run and verify tests in the codebase     |
```
