# Agent Instructions

## Landing the Plane (Session Completion)

**When ending a work session**, complete the steps below.

**WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - `make all`
3. **Commit changes** - Conventional commits on feature branch
4. **Ask user about push** - Confirm before pushing to remote
5. **Hand off** - Provide context for next session

---

# typemux-cc - Python Type-Checker LSP Multiplexer for Claude Code

See @README.md for project overview.
See @docs/ARCHITECTURE.md for architecture details.

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
4. **Commit**: Use conventional commits (feat:, fix:, docs:, etc.)
5. **Push and create PR**: Never merge directly to main
   ```bash
   git push -u origin <branch-name>
   gh pr create
   ```

### Pre-Commit Checklist

Before committing, verify:
- [ ] On a feature branch (not main)?
- [ ] `make all` passes?
- [ ] README.md updated if needed?
- [ ] PR will be created?

## Instructions for AI Agents

- **All code comments, commit messages, PR titles, PR descriptions, and review comments MUST be written in English.** No exceptions.
- Before committing, ALWAYS re-read this Workflow section
- When user says "commit", first check current branch and create feature branch if on main
- When user-facing behavior changes, proactively update README.md before committing
- **No implicit fallbacks** — Never add silent fallback logic that masks errors. Let it fail loudly so unintended behavior is caught early. An explicit error is always better than a silent wrong result.
- **No backward compatibility** — Do not preserve backward compatibility unless the user explicitly requests it. Breaking changes are the default; do not add compatibility shims, re-exports, or deprecation wrappers.

### Rust Skills

- When investigating or fixing Rust code, prefer rust-skills skills (m01–m15, domain-*, etc.)
- For ownership/borrow/lifetime errors, load the corresponding m0x skill
- For clippy errors or code review, load the relevant skill (e.g., coding-guidelines, m15-anti-pattern)

### Plan-First Rule

For changes touching **3 or more files** or introducing **new architectural patterns**:

1. **Enter plan mode first** — use `EnterPlanMode` to explore the codebase and design the approach before writing any code.
2. **Get the plan approved** — the user must approve before execution begins. The plan is the contract.
3. **Include a verification strategy** — every plan must answer: "How will we verify this works?" (tests, manual checks, CI gates, etc.)
4. **Stop if scope drifts** — if the implementation diverges from the approved plan, stop and re-plan rather than improvising.

For small, well-scoped changes (single-file fixes, typo corrections, simple bug fixes), skip planning and execute directly.

## Code Style

- Rust 2021 edition
- Use `cargo fmt` for formatting
- All clippy warnings treated as errors (`-D warnings`)
- **Prefer early returns over deep nesting** — Use guard clauses (`let x = match ... { Err => return }`) to keep the happy path flat. Avoid nesting `match`/`if let` more than 2 levels deep.

## Testing

- Run single test: `cargo test test_name`
- Run all tests: `cargo test` or `make test`
- Tests located alongside source in same module or in tests/ directory

## Project Structure

See @docs/ARCHITECTURE.md for source code structure.

---

## Known Mistakes & Lessons Learned

<!-- Reverse-chronological. Format: ### YYYY-MM-DD: Description / What happened / Root cause / Rule -->

