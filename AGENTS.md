# Agent Instructions

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   git push
   git status  # MUST show "up to date with origin"
   ```
4. **Clean up** - Clear stashes, prune remote branches
5. **Verify** - All changes committed AND pushed
6. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds

---

# pyright-lsp-proxy - Pyright LSP Proxy for Claude Code

See @README.md for project overview.
See @ARCHITECTURE.md for architecture details.

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

- Before committing, ALWAYS re-read this Workflow section
- When user says "commit", first check current branch and create feature branch if on main
- When user-facing behavior changes, proactively update README.md before committing
- **All code comments, commit messages, PR titles, PR descriptions, and review comments MUST be written in English**

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

## Testing

- Run single test: `cargo test test_name`
- Run all tests: `cargo test` or `make test`
- Tests located alongside source in same module or in tests/ directory

## Project Structure

- `src/main.rs` - Entry point, logging setup
- `src/proxy/` - LSP proxy module (split by responsibility)
  - `mod.rs` - LspProxy struct, `new()`, `run()` orchestration loop
  - `client_dispatch.rs` - Client message dispatch (initialize, shutdown, requests, notifications)
  - `backend_dispatch.rs` - Backend message dispatch (response forwarding, ID rewriting)
  - `initialization.rs` - Backend spawn + initialize handshake + document restoration
  - `pool_management.rs` - Pool lifecycle (ensure, evict LRU/TTL, crash, cancel)
  - `diagnostics.rs` - Diagnostics clearing
  - `document.rs` - Document lifecycle (didOpen/didChange/didClose) + URI utilities
- `src/text_edit.rs` - Pure text manipulation (incremental change, position-to-offset) + tests
- `src/backend.rs` - pyright-langserver process management
- `src/backend_pool.rs` - Multi-backend pool management
- `src/venv.rs` - `.venv` search logic
- `src/state.rs` - Proxy state management (open documents, session ID)
- `src/message.rs` - JSON-RPC message type definitions + error response helper
- `src/framing.rs` - JSON-RPC framing (Content-Length header processing)
- `src/error.rs` - Error type definitions

---

## Known Mistakes & Lessons Learned

Record AI-generated mistakes and the rules that prevent them from recurring. Update this section after every code review where the AI got something wrong. This knowledge compounds over time.

<!-- Add entries in reverse-chronological order (newest first) -->
<!-- Format: ### YYYY-MM-DD: Short description -->
<!-- - **What happened**: ... -->
<!-- - **Root cause**: ... -->
<!-- - **Rule**: The constraint to prevent recurrence -->

## Architecture Decisions

Key design choices and their rationale. See @ARCHITECTURE.md for full details.

### Strict Venv Mode
- **Context**: Running LSP with wrong `.venv` gives silently wrong results.
- **Decision**: Disable backend entirely when `.venv` is not found, returning explicit errors.
- **Rationale**: "A silently lying LSP is the worst." Explicit errors are healthier than false information.

### Multi-backend Pool
- **Context**: Monorepo environments have multiple projects with different `.venv` paths.
- **Decision**: Pool of pyright-langserver backends, one per `.venv`, with automatic routing.
- **Trade-off**: Higher memory usage vs. instant switching between projects.
