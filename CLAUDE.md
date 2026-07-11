# typemux-cc - Project Instructions

All project rules are defined in @AGENTS.md. Read it before starting any work.

## Rust Skills

- Provided by the [rust-skills plugin](https://github.com/actionbook/rust-skills); skip this section if the plugin is not enabled
- When investigating or fixing Rust code, prefer rust-skills skills (m01–m15, domain-*, etc.)
- For ownership/borrow/lifetime errors, load the corresponding m0x skill
- For clippy errors or code review, load the relevant skill (e.g., coding-guidelines, m15-anti-pattern)

## Plan-First Rule

For changes touching **3 or more files** or introducing **new architectural patterns**:

1. **Enter plan mode first** — use `EnterPlanMode` to explore the codebase and design the approach before writing any code.
2. **Get the plan approved** — the user must approve before execution begins. The plan is the contract.
3. **Include a verification strategy** — every plan must answer: "How will we verify this works?" (tests, manual checks, CI gates, etc.)
4. **Stop if scope drifts** — if the implementation diverges from the approved plan, stop and re-plan rather than improvising.

For small, well-scoped changes (single-file fixes, typo corrections, simple bug fixes), skip planning and execute directly.
