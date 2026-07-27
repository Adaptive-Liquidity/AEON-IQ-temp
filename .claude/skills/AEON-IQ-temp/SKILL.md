```markdown
# AEON-IQ-temp Development Patterns

> Auto-generated skill from repository analysis

## Overview
This skill guide covers the core development patterns and workflows used in the AEON-IQ-temp Rust codebase. It documents coding conventions, schema contract hardening workflow, and testing patterns to help contributors maintain consistency and security, especially around credential storage and verification.

## Coding Conventions

### File Naming
- **Style:** camelCase
- **Example:**  
  ```
  src/credentials/store.rs
  src/credentials/dbTests.rs
  ```

### Import Style
- **Style:** Relative imports
- **Example:**
  ```rust
  mod store;
  use super::store::CredentialStore;
  ```

### Export Style
- **Style:** Named exports
- **Example:**
  ```rust
  pub struct CredentialStore { /* ... */ }
  pub fn verify_credentials(...) { /* ... */ }
  ```

### Commit Messages
- **Type:** Conventional commits
- **Prefix:** `fix`
- **Example:**  
  ```
  fix: enforce unique constraint on user_id in credentials table
  ```

## Workflows

### Harden Schema Contract and Verification
**Trigger:** When you need to strengthen schema validation and contract enforcement for a database-backed authentication or credentials subsystem.  
**Command:** `/harden-schema-contract`

1. **Identify weaknesses or bypasses** in current schema validation logic.
   - Review `src/credentials/store.rs` and related modules for missing or weak constraints.
2. **Update contract verification logic** to check for stricter constraints.
   - Add or enhance checks such as `CHECK` expressions, column defaults, primary keys, and unique constraints.
   - Example:
     ```rust
     // Example: Adding a NOT NULL and UNIQUE constraint in a migration
     conn.execute(
         "CREATE TABLE credentials (
             user_id TEXT PRIMARY KEY,
             password_hash TEXT NOT NULL,
             created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
         )",
         [],
     )?;
     ```
3. **Update or add tests** to demonstrate both the broken and fixed behaviors.
   - Modify or create tests in `src/credentials/db_tests.rs`.
   - Example:
     ```rust
     #[test]
     fn test_duplicate_user_id_fails() {
         // Attempt to insert duplicate user_id and assert failure
     }
     ```
4. **Modify the main credential/authentication module** to enforce the stricter contract at startup.
   - Update `src/credentials/mod.rs` and `src/main.rs` to validate schema on launch.
   - Example:
     ```rust
     pub fn verify_schema(conn: &Connection) -> Result<()> {
         // Check for required constraints, fail if missing
     }
     ```
5. **Verify changes against a real or test database,** ensuring startup fails closed on contract violations.
   - Run the application and ensure it refuses to start if the schema is not compliant.

## Testing Patterns

- **Framework:** Unknown (Rust built-in test framework likely)
- **File Pattern:** `*.test.*` (e.g., `db_tests.rs`)
- **Example:**
  ```rust
  #[cfg(test)]
  mod tests {
      #[test]
      fn test_schema_constraints() {
          // Test logic here
      }
  }
  ```

## Commands

| Command                  | Purpose                                                        |
|--------------------------|----------------------------------------------------------------|
| /harden-schema-contract  | Strengthen and verify database schema contracts for credentials |
```
