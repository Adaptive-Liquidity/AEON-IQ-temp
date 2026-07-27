```markdown
# AEON-IQ-temp Development Patterns

> Auto-generated skill from repository analysis

## Overview
This skill teaches you the core development patterns, coding conventions, and workflows used in the AEON-IQ-temp TypeScript codebase. You'll learn how to structure files, write and organize code, follow commit message conventions, and run or write tests according to the repository's standards.

## Coding Conventions

### File Naming
- Use **camelCase** for all file names.
  - Example: `userProfile.ts`, `dataService.test.ts`

### Import Style
- Use **relative imports** for referencing modules within the project.
  - Example:
    ```typescript
    import { fetchData } from './dataService';
    ```

### Export Style
- Use **named exports** for all modules.
  - Example:
    ```typescript
    // In dataService.ts
    export function fetchData() { ... }
    ```

### Commit Messages
- Follow **conventional commit** style.
- Prefixes used: `fix`, `ci`
- Example:
  ```
  fix: correct data fetching logic in userProfile
  ci: update workflow to use latest Node.js version
  ```

## Workflows

### Fixing Bugs
**Trigger:** When you need to correct a bug or issue in the codebase  
**Command:** `/fix-bug`

1. Identify the bug and its location.
2. Create a new branch for your fix.
3. Apply the fix, following the coding conventions.
4. Write or update tests as needed.
5. Commit your changes using the `fix:` prefix.
   - Example: `fix: resolve null pointer in dataService`
6. Push your branch and open a pull request.

### Continuous Integration Updates
**Trigger:** When updating CI configuration files or scripts  
**Command:** `/update-ci`

1. Make necessary changes to CI-related files (e.g., workflow scripts).
2. Commit your changes using the `ci:` prefix.
   - Example: `ci: update node version in CI workflow`
3. Push your branch and open a pull request.

## Testing Patterns

- Test files use the pattern `*.test.*` (e.g., `userProfile.test.ts`).
- The specific testing framework is not detected—refer to existing test files for structure.
- Example test file:
  ```typescript
  // userProfile.test.ts
  import { getUserProfile } from './userProfile';

  describe('getUserProfile', () => {
    it('returns user data for valid ID', () => {
      // test implementation
    });
  });
  ```

## Commands
| Command      | Purpose                                      |
|--------------|----------------------------------------------|
| /fix-bug     | Start the bug fixing workflow                |
| /update-ci   | Start the CI configuration update workflow   |
```
