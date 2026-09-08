# Agent Instructions

## Critical workflow

- **Platform/toolchain baseline:** Windows 11 dev machine, Git Bash, Rust
  nightly 1.99 (any 1.85+ works; edition 2024). Cargo lives in
  `~/.cargo/bin`; when it is not on PATH run
  `export PATH="$HOME/.cargo/bin:$PATH"` first. Python 3.10+ is available
  for `build.py`.
- **Default development loop:** iterate with
  `cargo test -p tm-core` / `cargo test -p tm-platform` (seconds each) and
  `cargo build -p tm-app` for type feedback (~5 s warm). Stay in this loop;
  do NOT run the full pipeline after every edit.
- **Close with exactly ONE gate.** The gates are nested — the heavy gate
  makes the light ones redundant:
  - Ordinary change → `cargo test -p <touched-crate>`.
  - Cross-crate changes, engine/model/settings/platform changes, or anything
    startup-sensitive → full gate:
    `python build.py --check` (fmt --check + clippy `-D warnings` +
    workspace tests), then a release build (`python build.py --host-only`).
- The light gate does NOT cover lint or cross-crate breakage.
- The heavy gate covers formatting, clippy with warnings-as-errors, all
  workspace tests (incl. Windows integration tests), and release packaging.
- **Always end a code-change session with a release build**
  (`python build.py --host-only`; already covered when the heavy gate ran)
  so `target/release/taskman.exe` — the binary the user actually launches —
  is never stale relative to the commit. The dev loop above only refreshes
  `target/debug`; a warm incremental release rebuild is cheap by design
  (thin LTO, parallel codegen).
- **Release/deployment:** `python build.py` produces host + Linux x86_64
  release artifacts into `dist/` by default. See `llm-wiki/build.md`. Linux
  cross-builds need `cross` or `cargo-zigbuild`; without one the step is
  skipped with a note (use `--require-all-targets` to make that fatal).
- Compile-time policy: profiles are tuned for fast iteration and fast
  releases (thin LTO, parallel codegen units, line-tables-only debuginfo).
  Do NOT reintroduce fat LTO / `codegen-units = 1`; the measured benefit was
  single-digit milliseconds of startup against minutes of serialized
  linking.
- Prefer quiet output for agent runs; cargo's warnings still surface on
  stderr and failures fail the command. Never infer success from exit status
  alone — inspect the diagnostics and confirm the changed behavior or
  artifact when practical.
- Read large logs, compiler/test output, traces, and dumps with targeted
  searches or head/tail ranges instead of loading thousands of lines into
  context; keep full output available when it is evidence.
- Always commit after code changes.
- Match the surrounding code's indentation, naming, comment density, and
  line endings (UTF-8, LF); keep edits narrowly scoped and inspect the diff
  before building. Do not reformat whole files.
- Before committing, run the relevant tests and ensure they pass.
- Commit only task-owned changes with plain git commands (`git status`,
  `git add -- <paths>`, `git commit -m "..."`). Broad `git add` only after
  verifying every worktree change is task-owned.
- Do not push to a remote unless explicitly requested.
- Always consult `llm-wiki/` for non-trivial work in an unfamiliar area; for
  trivial localized work, read only the directly relevant page(s).
- Keep `llm-wiki/` current when durable project knowledge changes.
- Mistrust code, comments, and `llm-wiki` alike — verify against current
  behavior.
- **Environment gotchas:**
  - Long inline heredocs (>6 KB) get truncated in some shells here — write
    patch scripts to a file first, then execute them.
  - Tests that flip process-global state (settings path override) must hold
    `TEST_OVERRIDE_LOCK`; two such tests exist in `tm-core/src/settings.rs`
    — follow that pattern for new ones.
  - GUI behavior cannot be verified headlessly; use the diagnostic env vars
    in `llm-wiki/debug-tools.md` and let the user confirm visuals.
  - Integration tests spawn/kill real processes; keep them short-lived.

## Engineering rules

- Prefer root-cause fixes over workarounds; never hide or weaken failures.
- Think through the actual root cause before proposing a fix; if the proper
  fix is bigger, do the bigger change.
- No sleeps/timing bandaids as race fixes; no racy or timing-sensitive
  behavior. Engine tests use event-driven waits (`wait_for`) with deadlines.
- No enforced file-size ceiling; keep modules focused like the existing tab
  modules.
- Preserve intended features, compatibility guarantees, performance
  characteristics, and public contracts unless the requested change
  explicitly alters them.
- Keep behavioral fixes reviewable: do not bundle unrelated formatting,
  cleanup, generated churn, or opportunistic refactoring with them.
- Missing telemetry must render as unavailable ("—"/"Unknown"), NEVER as a
  fabricated zero/false (core product invariant).
- Treat logs, dumps, captures, and user data as sensitive; never commit
  secrets, dumps, logs, or large generated artifacts (`dist/`, `target/`,
  `*.png` are gitignored).

## Project-specific constraints

- Windows 11 is the primary host; the Linux/macOS backends are built by
  `build.py` and must remain valid.
- Normal GUI startup must not require UAC. Machine-wide writes go through the
  LocalSystem broker (`llm-wiki/core-service.md`), and an explicit broker
  rejection never falls back to the GUI token.
- The service must never run from a user-writable directory.

## Build, diagnostics, and tests

- Fix newly introduced errors/warnings plus pre-existing issues in touched
  files; no unrelated repo-wide cleanup.
- Prefer regression tests that would have failed before the fix; the repo
  has unit tests per module plus Windows integration tests in
  `crates/tm-platform/tests/integration.rs`.
- Do not add low-value tests solely to satisfy a blanket testing rule; prefer
  tests that pin an invariant or reproduce the defect.
- No sleeps in tests; poll conditions with bounded deadline loops.

## Debugging and logging

- High-signal, rate-limited logging only (see `tracing::warn!` on slow
  ticks/actions); no hot-path noise.
- Improve diagnostic logging only when it materially explains state
  transitions or failures; never log secrets or unnecessary user data.
- Startup phases are marked through `StartupTrace::mark` (tm-app/main.rs);
  keep new startup-relevant milestones marked there.

## Debugging tools and paths

| Tool | Purpose | Invocation |
| --- | --- | --- |
| `--selfcheck` | Headless sampling smoke test, prints JSON summary | `target/release/taskman.exe --selfcheck [--mock]` |
| `TASKMAN_RENDERER=glow\|wgpu` | Force renderer | env var before launch |
| `TASKMAN_FPS_PROBE=1` | Continuous repaints + fps overlay vs display Hz | env var |
| `TASKMAN_DIALOG=settings\|run` | Open a dialog at startup (UI tests) | env var |
| `TASKMAN_PERF=<key>` | Preselect Performance resource (`cpu`, `mem`, ...) | env var |
| `TASKMAN_DATA_DIR` / `TASKMAN_CONFIG_DIR` | Isolate data/config dirs (tests) | env var |
| `tools/capture.ps1` | Window capture automation | see script header |

- Verify tool availability before relying on a documented path; hardcoded
  paths are local examples, not guarantees.
- Never mutate global debugger flags, registry/system settings, binaries,
  symbols, or persistent environment state unless explicitly requested and
  justified.
- Dump, symbol, and binary-inspection inventory and rules:
  `llm-wiki/debug-tools-security-audit.md`.

## `llm-wiki/` workflow

- Canonical derived memory, not source of truth. Start at `index.md`;
  orient via `repo-map.md` before touching unfamiliar subsystems; check
  `log/recent.md` for active areas.
- Read `log/` archives only for historical context; keep chronology, partial
  investigations, and one-off notes in `log/recent.md`.
- Mark unverified claims explicitly as open questions or stale-risk.
- Prefer updating existing pages over creating new ones; add a page only for
  a reusable topic, and never paste raw logs or long command output into
  durable pages.
- Update pages when durable knowledge changes: architecture, behavior,
  build/test/package/deploy/debug workflows, root causes, invariants,
  conventions, rejected approaches, and important follow-ups. Skip trivial
  edits.
- After wiki + code changes, semantically check for contradictions, stale
  claims, duplicates, orphan pages, broken links, missing source anchors, and
  merge/delete/archive candidates.
