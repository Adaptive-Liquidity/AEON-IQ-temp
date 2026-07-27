```markdown
# AEON-IQ-temp Development Patterns

> Auto-generated skill from repository analysis

## Overview
This skill teaches the core development patterns and workflows used in the AEON-IQ-temp Rust codebase. It covers coding conventions, file organization, commit practices, and step-by-step guides for common workflows such as database migrations and feature implementation. This guide is designed to help contributors quickly understand and follow the established practices in the repository.

## Coding Conventions

- **File Naming:**  
  Use `snake_case` for all Rust source files.
  ```
  src/tenancy.rs
  src/main.rs
  ```

- **Import Style:**  
  Prefer relative imports within the crate.
  ```rust
  mod tenancy;
  use crate::tenancy::Tenant;
  ```

- **Export Style:**  
  Use named exports for modules and functions.
  ```rust
  pub mod tenancy;
  pub fn run() { /* ... */ }
  ```

- **Commit Messages:**  
  Follow [Conventional Commits](https://www.conventionalcommits.org/) with these prefixes:
  - `feat`: New features
  - `fix`: Bug fixes
  - `docs`: Documentation changes

  Example:
  ```
  feat: add tenant isolation to database layer
  ```

## Workflows

### Database Migration Workflow
**Trigger:** When introducing a new database feature or enforcing new constraints  
**Command:** `/new-migration`

1. **Create or update forward migration SQL file**  
   Add a new SQL file in `migrations/` describing the schema change.
   ```
   migrations/20240601_add_tenant_table.sql
   ```
2. **Create or update rollback migration SQL file**  
   Add a corresponding SQL file in `rollback/` to revert the change.
   ```
   rollback/20240601_remove_tenant_table.sql
   ```
3. **Update application logic**  
   Modify relevant Rust files to use the new schema.
   ```rust
   // src/tenancy.rs
   pub struct Tenant { /* ... */ }
   ```
4. **Document migration steps and environment variables**  
   Update documentation in `docs/` and add any new environment variables to `.env.example`.
   ```
   # .env.example
   DATABASE_URL=postgres://user:pass@localhost/aeon_iq
   ```
5. **Update CI workflow if needed**  
   Edit `.github/workflows/ci.yml` to handle schema changes or new test requirements.

### Feature Implementation and Documentation Workflow
**Trigger:** When adding a new application-level feature or configuration  
**Command:** `/new-feature`

1. **Implement feature logic**  
   Add or update code in `src/` (e.g., `src/tenancy.rs`, `src/main.rs`).
   ```rust
   // src/tenancy.rs
   pub fn enable_multi_tenancy() { /* ... */ }
   ```
2. **Document feature and configuration**  
   Update `docs/` with usage and add any new environment variables to `.env.example`.
   ```
   # docs/multi_tenancy.md
   ## Enabling Multi-Tenancy
   Set `ENABLE_MULTI_TENANCY=true` in your environment.
   ```
3. **Update CI workflow if needed**  
   Modify `.github/workflows/ci.yml` to add new tests or handle new environment variables.

## Testing Patterns

- **Test File Naming:**  
  Test files follow the pattern `*.test.*` (e.g., `tenancy.test.rs`).
- **Framework:**  
  No specific testing framework detected; use Rust's built-in test framework.
- **Example Test:**
  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn test_tenant_creation() {
          // test logic here
      }
  }
  ```

## Commands

| Command         | Purpose                                                    |
|-----------------|------------------------------------------------------------|
| /new-migration  | Start a new database migration workflow                    |
| /new-feature    | Start a new feature implementation and documentation workflow |
```
