# ADR 0001: Trunk-based development with CI-gated PRs

- Status: accepted
- Date: 2026-09-23
- Supersedes the workflow ADR of libworkspaceVR (0001-trunk-based-ci-gated)

## Context

Emersia couples a low-level Rust host (capture/encode/transport/input) with a
Godot client targeting physical Quest hardware, plus a Tauri GUI later on.
Hardware QA is manual and cannot be automated in CI. Risks: an untested host
change breaks the streaming pipeline; an unreviewed client change is only
discoverable on-device; unstructured hacking loses decisions between sessions.

## Decision

1. Single trunk (`main`), short-lived feature branches, squash merges.
2. **No direct pushes to `main`** (branch protection): every change lands via
   pull request and must pass the required `ci` check (pre-check hygiene,
   `cargo fmt`, `clippy -D warnings`, `cargo test`).
3. Functional changes are linked to a GitHub Issue; headset-side behavior is
   tracked through the Hardware/VR QA issue template, since CI cannot
   exercise it.
4. Architecture decisions are recorded as ADRs (`docs/adr/NNNN-title.md`),
   immutable once accepted; a reversal is a new ADR superseding the old one.
5. Work is tracked on the GitHub Projects (v2) board: automated status moves
   (Todo → In Progress → In Review → Done) plus developer-driven fields
   (Milestone, Priority, Area).

## Consequences

+ Binary "what changed and why" survives across sessions.
+ Human remains the merge gate for everything CI cannot see (visual, latency,
  comfort); required `ci` check keeps `main` always buildable.
+ Roadmap and day-to-day work share one board — no second source of truth.
− Slightly slower feedback loop than free-form hacking; intentional.
− Solo development means review-by-others arrives only with contributors;
  the PR template's self-review checklist covers the gap meanwhile.
