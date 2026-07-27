```markdown
# AEON-IQ-temp Development Patterns

> Auto-generated skill from repository analysis

## Overview
This skill teaches the core development patterns and workflows for the AEON-IQ-temp Rust codebase. It covers coding conventions, file organization, commit practices, and common workflows such as feature development with database migrations, post-merge review fixes, and CI test exclusion updates. The guide is intended to help contributors quickly understand and follow the established practices in this repository.

## Coding Conventions

### File Naming
- Use **camelCase** for file names.
  - Example: `userCredentials.rs`, `dbTests.rs`

### Imports
- Use **relative imports** within the `src/` directory.
  - Example:
    ```rust
    mod dbTests;
    use super::dbTests::setup_test_db;
    ```

### Exports
- Use **named exports** for modules and functions.
  - Example:
    ```rust
    pub mod credentials;
    pub fn validate_user(...) { ... }
    ```

### Commit Messages
- Follow **conventional commit** format.
- Common prefixes: `feat`, `ci`, `fix`
- Example:
  ```
  feat(credentials): add password reset logic
  fix(db): correct migration for user table
  ci: exclude db tests from no-db job
  ```

## Workflows

### Feature Development with DB Migration
**Trigger:** When adding a new backend feature that requires both database schema and Rust code changes.  
**Command:** `/new-feature-with-db`

1. **Create or modify a SQL migration file** in `migrations/` to update the database schema.
   - Example: `migrations/20240601_add_user_status.sql`
2. **Add or update Rust module files** under `src/` to implement the feature logic.
   - Example: `src/credentials/userStatus.rs`
3. **Update dependencies** in `Cargo.toml` and `Cargo.lock` if needed.
4. **Write or update tests** in files like `src/credentials/dbTests.rs`.
   - Example:
     ```rust
     #[test]
     fn test_user_status_migration() {
         // test logic here
     }
     ```
5. **Integrate the feature** into the main application logic (e.g., `src/main.rs`).
6. **Commit** with a descriptive message, e.g.:
   ```
   feat(credentials): add user status with db migration
   ```

### Post-Merge Review Fix
**Trigger:** When fixing issues found in code review or after a merge (bug, security, or correctness).  
**Command:** `/review-fix`

1. **Identify and describe each issue** in the commit message.
2. **Update relevant Rust source files** (e.g., `src/credentials/*.rs`) to address the findings.
3. **Add or update regression/unit tests** to verify the fixes.
   - Example:
     ```rust
     #[test]
     fn test_password_reset_fix() {
         // regression test logic
     }
     ```
4. **Commit** with detailed notes about each fix.
   ```
   fix(credentials): correct password reset logic and add regression test
   ```

### CI Test Exclusion Update
**Trigger:** When adjusting which tests run in different CI environments.  
**Command:** `/ci-update`

1. **Edit `.github/workflows/ci.yml`** to add or modify test exclusion rules.
   - Example:
     ```yaml
     jobs:
       test:
         steps:
           - run: cargo test --exclude db_tests
     ```
2. **Document the rationale** in the commit message.
   ```
   ci: exclude db tests from no-db CI job
   ```

## Testing Patterns

- **Test files** follow the pattern: `*.test.*` (e.g., `dbTests.rs`, `userCredentials.test.rs`)
- Tests are written using Rust's built-in test framework.
- Place tests in the same module or in dedicated test files like `src/credentials/dbTests.rs`.
- Example:
  ```rust
  #[cfg(test)]
  mod tests {
      #[test]
      fn test_feature() {
          assert_eq!(2 + 2, 4);
      }
  }
  ```

## Commands
| Command                | Purpose                                                        |
|------------------------|----------------------------------------------------------------|
| /new-feature-with-db   | Start a new feature requiring both DB migration and code logic |
| /review-fix            | Apply post-merge or code review fixes                         |
| /ci-update             | Update CI workflow to exclude or include specific tests        |
```