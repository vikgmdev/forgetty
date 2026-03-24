# ADR-003: Open-Source Core with Separate SaaS Repository

## Status

Accepted

## Context

Forgetty's business model includes both an open-source terminal emulator and
a paid cloud sync service (settings sync, session sync across devices, team
workspaces). We need to decide where the SaaS-specific code lives relative to
the open-source terminal.

### Options Considered

1. **Monorepo** — Everything in one repository with feature flags or build-time
   configuration to include/exclude SaaS features.
2. **Separate repositories** — The open-source terminal in one repo, the SaaS
   backend and sync client in a private repo.
3. **Open core with proprietary directory** — Open-source repo with a
   `proprietary/` directory that is not covered by the MIT license.

### Prior Art

- **Beekeeper Studio** — Open-source SQL editor (GPLv3) with a separate
  private repo for the "Ultimate" edition features. The open-source version
  is fully functional; premium features are additive.
- **GitLab** — Single repo with CE (MIT) and EE (proprietary) directories.
  Works but creates licensing confusion and merge complexity.
- **Zed** — Fully open-source editor with a separate private repo for the
  collaboration server.

### Key Factors

**Clear licensing.** A separate repository means the open-source terminal is
unambiguously MIT-licensed. No license headers to maintain, no confusion about
which files are proprietary.

**Community trust.** Contributors know that everything in the open-source repo
is MIT. There is no risk of their contributions ending up behind a paywall.

**Development velocity.** The SaaS backend (authentication, payment, sync
protocol, infrastructure) has completely different dependencies, CI pipelines,
and deployment processes. Mixing it with the terminal codebase would slow down
both.

**Integration surface.** The sync client in the SaaS repo depends on the
open-source terminal via the socket API and config schema. This is a narrow,
well-defined interface.

## Decision

Maintain **two separate repositories**:

1. **`totem-labs-forge/forgetty`** (this repo) — The open-source terminal
   emulator. MIT licensed. Contains all crates needed to build and run a
   fully functional terminal.

2. **`totem-labs-forge/forgetty-cloud`** (private) — The SaaS backend and
   sync client. Contains the authentication service, sync protocol, payment
   integration, and a thin client library that connects to the terminal via
   the socket API.

The open-source terminal defines stable interfaces that the SaaS client
consumes:

- **Socket API** — The JSON-RPC API for reading/writing terminal state.
- **Config schema** — The TOML configuration format, including sync-related
  keys that the open-source terminal reads but does not implement.
- **Workspace format** — The on-disk session persistence format.

### Plugin / Extension Point

The open-source terminal includes a `[sync]` configuration section with
`enabled = false` by default. When enabled, it looks for a `forgetty-sync`
binary on `PATH` and launches it as a child process, communicating via the
socket API. The sync binary is distributed separately (e.g., via `brew install
totem-labs-forge/tap/forgetty-sync`).

This means:
- The open-source terminal has zero SaaS code.
- The sync feature is opt-in and works via the same public API available to
  any external tool.
- Third parties could build alternative sync implementations.

## Consequences

**Benefits:**
- Clean MIT license for the entire open-source repo.
- No contributor confusion about proprietary code.
- Independent CI/CD pipelines for terminal and SaaS.
- The open-source terminal is fully functional without the SaaS component.
- Third-party integrations use the same API as the first-party sync client.

**Costs:**
- Changes that span both repos require coordinated releases.
- The socket API becomes a stability contract that must be maintained.
- Two repos to manage, with separate issue trackers and CI.

**Mitigations:**
- The socket API is versioned and follows JSON-RPC conventions.
- Integration tests in the SaaS repo test against the latest terminal release.
- A shared `CHANGELOG.md` section tracks API changes.
