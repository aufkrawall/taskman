# llm-wiki Index

Last cross-checked: 2026-08-31 (GUI parity and protected core-service pass)

Primary sources:

- `AGENTS.md`
- `Cargo.toml`, `build.py`
- `crates/tm-core`, `crates/tm-platform`, `crates/tm-app`, `crates/tm-service`
- tests beside each subsystem

## Purpose and trust model

`llm-wiki` is compact, derived project memory for maintainers and agents. It
is not the implementation: cross-check security-, startup-, and
correctness-sensitive claims against current source, tests, and build scripts.
When they disagree, code and observed behavior win.

## Recommended read order

1. Read `repo-map.md` before changing an unfamiliar subsystem.
2. Read `current.md` for the current feature and architecture summary.
3. Route to the topic page below; use `log/recent.md` for change history.
4. Read `AGENTS.md` and `build.md` before running gates or producing releases.

## Content catalog

- `current.md` — compact current state and recent major changes.
- `repo-map.md` — crate/module ownership, high-risk files, and test matrix.
- `core-service.md` — privileged broker, IPC trust boundary, filesystem ACLs,
  install/upgrade/uninstall lifecycle, and fallback behavior.
- `build.md` — release driver, packaging, profile and renderer policy.
- `debug-tools.md` — headless diagnostics and interactive-only tools.
- `codestyle.md` — coding and tooling conventions.
- `known-debt.md` — deliberate remaining parity and platform gaps.
- `log/recent.md` — recent chronology; `log.md` routes to older archives.

## Maintenance

- Keep dates and primary sources current when durable behavior changes.
- Record accepted limitations in `known-debt.md`, not scattered TODO comments.
- Never treat logs, dumps, captures, or generated `target/`/`dist/` files as
  documentation fixtures; they may contain user or machine data.
