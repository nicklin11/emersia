# Contributing to Emersia

## Workflow (trunk-based, CI-gated)

- `main` is protected: **no direct commits**, every change lands via pull
  request, and the `ci` check must be green before merge.
- Branch naming: `feat/…`, `fix/…`, `docs/…`, `chore/…`, `ci/…`,
  `refactor/…`, `test/…` — scoped where useful, e.g. `feat/host-capture`.
- Commits follow [Conventional Commits](https://www.conventionalcommits.org/):
  `feat(host): …`, `fix(client): …`, `docs(adr): …`, `ci: …`.
- Merge strategy: **squash merge**, PR title becomes the commit subject.
- CI runs `scripts/pre-check.sh` (repo hygiene), `cargo fmt --check`,
  `cargo clippy -D warnings` and `cargo test` on the `host` workspace.

## Pull requests

1. Open as draft while work is in progress; mark ready for review when CI is
   green and the change is complete.
2. Link related issues (`Fixes #123`) so issues close on merge.
3. Keep PRs small and reviewable; large efforts get a tracking issue first.

## Architecture Decision Records

Significant technical decisions (stack, protocol, security model) are recorded
in `docs/adr/`:

- File name: `NNNN-kebab-case-title.md`, number allocated once, never reused.
- Structure: `# ADR NNNN: Title` with **Status / Context / Decision /
  Consequences**.
- Superseding a decision: write a new ADR and mark the old one
  `Status: superseded by ADR NNNN` — never edit history in place.

## Labels & project board

The [project board](https://github.com/users/nicklin11/projects/4) tracks work:

- New issues/PRs are added automatically (Status = **Todo**).
- Add the `status: in-progress` label (or assign the issue) → **In Progress**.
- PR opened → **In Review**; PR merged or issue closed → **Done**.
- `priority:` and `area:` labels plus the **Milestone** field drive the
  roadmap view.

## Security

Do not file security issues publicly — contact the maintainer directly.
The host daemon can inject input into your session (`uinput`) and capture the
screen: treat pairing, transport encryption and consent UI as security surface.
