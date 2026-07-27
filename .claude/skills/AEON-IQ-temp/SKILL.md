```markdown
# AEON-IQ-temp Development Patterns

> Auto-generated skill from repository analysis

## Overview
This skill introduces the core development patterns and conventions used in the AEON-IQ-temp Rust codebase. You'll learn how to structure files, write imports and exports, follow commit message conventions, and organize tests to align with the repository's standards.

## Coding Conventions

### File Naming
- Use **camelCase** for file names.
  - Example: `dataProcessor.rs`, `userManager.rs`

### Import Style
- Use **relative imports** for referencing modules within the project.
  - Example:
    ```rust
    mod utils;
    use crate::dataProcessor::process_data;
    ```

### Export Style
- Use **named exports** to expose functions, structs, or modules.
  - Example:
    ```rust
    pub fn calculate_score(input: i32) -> i32 {
        // implementation
    }
    ```

### Commit Messages
- Follow **conventional commit** format.
- Prefixes used: `feat`, `ci`
- Example:
  ```
  feat: add user authentication module
  ci: update workflow for Rust toolchain
  ```

## Workflows

### Commit Changes
**Trigger:** When committing code to the repository  
**Command:** `/commit`

1. Stage your changes:
   ```
   git add .
   ```
2. Write a commit message using the conventional format:
   ```
   git commit -m "feat: short description of your change"
   ```
   - Use `feat` for features, `ci` for continuous integration changes.
3. Push your changes:
   ```
   git push
   ```

### Add a New Module
**Trigger:** When creating a new feature or logical unit  
**Command:** `/add-module`

1. Create a new file using camelCase, e.g., `userManager.rs`.
2. Define your module and export functions or structs using `pub`.
   ```rust
   pub struct UserManager { /* fields */ }
   pub fn create_user() { /* ... */ }
   ```
3. Import your module where needed using a relative path.
   ```rust
   use crate::userManager::UserManager;
   ```

### Write and Run Tests
**Trigger:** When adding or updating code that requires testing  
**Command:** `/test`

1. Create a test file matching the pattern `*.test.*`, e.g., `userManager.test.rs`.
2. Write your tests using Rust's built-in test framework.
   ```rust
   #[cfg(test)]
   mod tests {
       use super::*;

       #[test]
       fn test_create_user() {
           // test implementation
       }
   }
   ```
3. Run tests:
   ```
   cargo test
   ```

## Testing Patterns

- Test files follow the `*.test.*` naming pattern (e.g., `moduleName.test.rs`).
- Tests are written using Rust's built-in test framework.
- Example:
  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn test_functionality() {
          assert_eq!(function_under_test(), expected_value);
      }
  }
  ```

## Commands
| Command      | Purpose                                         |
|--------------|-------------------------------------------------|
| /commit      | Commit code following the conventional format   |
| /add-module  | Add a new module using project conventions      |
| /test        | Write and run tests for your code               |
```
