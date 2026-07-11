# Agent Instructions

## Landing the Plane (Session Completion)

**When ending a work session**, complete the steps below.

**WORKFLOW:**

1. **Run quality gates** (if code changed) - `make all`
2. **Report remaining work** - Describe follow-up work without creating issues unless the user explicitly authorizes issue creation
3. **Report the working tree** - Summarize changed files and verification results
4. **Hand off** - Provide context for the next session

Creating issues, committing, pushing, and opening pull requests are separate external
mutations. Perform each one only when the user explicitly authorizes that action. An
instruction to commit does not authorize a push or pull request, and an instruction to
push does not authorize opening a pull request.

---

# typemux-cc - Python Type-Checker LSP Multiplexer for Claude Code

See README.md for project overview.
See docs/ARCHITECTURE.md for architecture details.

## Build & Quality

```bash
# REQUIRED: Run before completing any work
make all          # format + lint + test

# Individual commands
make fmt          # cargo fmt
make lint         # cargo clippy -- -D warnings
make test         # cargo test
```

## Git Workflow (MUST FOLLOW)

⚠️ **NEVER commit directly to main. Always use feature branches.**

1. **BEFORE any code changes**: Create a feature branch
   ```bash
   git checkout -b feat/your-feature-name
   ```
2. **After changes**: Run quality checks
   ```bash
   make all  # format + lint + test
   ```
3. **Update documentation**: If user-facing behavior changes, update README.md
4. **Commit, when explicitly authorized**: Use conventional commits (feat:, fix:, docs:, etc.)
5. **Push, when explicitly authorized**: Never merge directly to main
   ```bash
   git push -u origin <branch-name>
   ```
6. **Create a pull request, when explicitly authorized**
   ```bash
   gh pr create
   ```

### Pre-Commit Checklist

Before committing, verify:
- [ ] On a feature branch (not main)?
- [ ] `make all` passes?
- [ ] README.md updated if needed?
- [ ] The user explicitly authorized the commit?

## Instructions for AI Agents

- **All code comments, commit messages, PR titles, PR descriptions, and review comments MUST be written in English.** No exceptions.
- Before committing, ALWAYS re-read this Workflow section
- When user says "commit", first check current branch and create feature branch if on main
- Treat authorization for issue creation, commits, pushes, and pull requests separately; never infer one from another
- When user-facing behavior changes, proactively update README.md before committing
- **No implicit fallbacks** — Never add silent fallback logic that masks errors. Let it fail loudly so unintended behavior is caught early. An explicit error is always better than a silent wrong result.
- **No backward compatibility** — Do not preserve backward compatibility unless the user explicitly requests it. Breaking changes are the default; do not add compatibility shims, re-exports, or deprecation wrappers.

## Code Style

- Rust 2021 edition
- Use `cargo fmt` for formatting
- All clippy warnings treated as errors (`-D warnings`)
- **Prefer early returns over deep nesting** — Use guard clauses (`let x = match ... { Err => return }`) to keep the happy path flat. Avoid nesting `match`/`if let` more than 2 levels deep.

## Testing

- Run single test: `cargo test test_name`
- Run all tests: `cargo test` or `make test`
- Tests located alongside source in same module or in tests/ directory
- `tests/version_consistency_test.rs` requires `jq` on `PATH` (used by
  `scripts/check-versions.sh`); `make ci` runs `check-versions` first and
  fails fast with a clear error if `jq` is missing, before `cargo test` runs

## Project Structure

See docs/ARCHITECTURE.md for source code structure.
