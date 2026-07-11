# Security Policy

## Supported Versions

typemux-cc is pre-1.0 and releases roughly follow `main`. Only the latest
released version receives security fixes; please upgrade before reporting
an issue.

## Reporting a Vulnerability

Please report security vulnerabilities privately via GitHub's
[Private Vulnerability Reporting](https://github.com/K-dash/typemux-cc/security/advisories/new),
not through public issues, discussions, or pull requests.

Include as much of the following as you can:

- Affected version (`typemux-cc --version`) and platform
- Steps to reproduce, or a minimal example
- Impact (what an attacker could do)

This is a solo-maintained open source project, so response times are
best-effort rather than a guaranteed SLA. You'll get an acknowledgment and
a fix, credit, or explanation once triaged.

## Automated Checks

Every change is checked in CI with [`cargo-deny`](deny.toml) (advisories,
license, and dependency-source policy), GitHub's Dependency Review, and
[`zizmor`](https://github.com/zizmorcore/zizmor) (GitHub Actions workflow
security linting).
