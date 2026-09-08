# Code Style

Last cross-checked: 2026-09-08

Primary sources:
- `AGENTS.md`
- `build.py` (quality gate)
- `Cargo.toml` (edition, MSRV, profiles)
- representative modules (`crates/tm-core/src/format.rs`,
  `crates/tm-app/src/tabs/*`, `crates/tm-platform/src/win/*`)

## Scope

This page records style rules that are tool-backed or strongly reflected in
the current tree. A touched subsystem's established local pattern still wins
when it clearly differs.

## Tool-Backed Rules

### Rust

- Edition 2024, MSRV 1.85 (workspace package metadata).
- `cargo fmt --all -- --check` is enforced by `python build.py --check`.
  There is no `rustfmt.toml`, so rustfmt defaults apply (4-space indent,
  100-column width, standard brace and import layout). Do not add a rustfmt
  config or reformat whole files for style-only reasons.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` is
  enforced by the same gate. There is no lint ratchet or accepted baseline:
  the tree is expected to stay warning-free.
- Targeted `#[allow(...)]` is acceptable only when justified inline, e.g.
  `#[allow(dead_code)] // kept for future committed-limit charts`. It is not
  a way to silence fixable lints.
- Keep `unsafe` minimal. Where it is unavoidable (Win32 FFI), keep the block
  narrow and document non-obvious pointer/lifetime invariants with a
  `SAFETY:` comment (see `crates/tm-platform/src/win/net_etw.rs` and
  `net_info.rs`).

## Common Tree Conventions

- Standard Rust naming: `snake_case` modules/functions/files, `PascalCase`
  types/traits, `SCREAMING_SNAKE_CASE` constants. No project-specific name
  prefixes.
- Module-level `//!` docs state purpose and architecture; public items get
  `///` docs. Comments explain why, not what; comment density follows the
  surrounding module.
- GUI tabs are one module per tab under `crates/tm-app/src/tabs/` (wired by
  `mod.rs`); Windows platform concerns are one module per concern under
  `crates/tm-platform/src/win/`. Keep new modules focused the same way.
- Unit tests live in `#[cfg(test)] mod tests` beside the code they cover;
  Windows integration tests live in `crates/tm-platform/tests/integration.rs`.
- Tests are deterministic and event-driven (`wait_for` with deadlines); no
  sleeps or timing assumptions.
- Source files are UTF-8 with LF line endings.

## Practical Notes

- Do not run a whole-file automatic formatter on existing source unless
  explicitly requested; a tree with legacy formatting or mixed line endings
  can produce large unrelated diffs from a narrow intended edit.
- Preserve the touched file's existing formatting and line endings. Inspect
  the diff before building.
- Naming and local-pattern guidance here is medium confidence; re-check it
  against the files you touch.
- If formatter output, local file style, and this page disagree, preserve the
  local subsystem's established pattern unless the user explicitly requested
  a formatting migration.

### Current lint debt and triage

No lint ratchet or accepted exceptions: `build.py --check` runs clippy with
`-D warnings`, so any new warning is a gate failure, not debt.
