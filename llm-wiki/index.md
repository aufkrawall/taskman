# llm-wiki Index

<!--
TEMPLATE NOTE: This is a generic skeleton for the wiki's routing page. Fill
in the Primary sources, Content Catalog, and cross-check date for the real
project, then delete this note. The "Last cross-checked" line at the top of
this file is meant to carry a dense, evolving summary of everything an
agent needs to know is currently true before trusting older pages — keep it
short at first and let it grow as real work accumulates.
-->

Last cross-checked: <YYYY-MM-DD> (initial template scaffold, no project
content recorded yet)

Primary sources:
- `AGENTS.md`
- `<build entry point, e.g. build.py / Makefile / package.json>`
- `<key config files>`
- `<top-level source directories>`

## Purpose
`llm-wiki` is the derived documentation layer for agents and maintainers.
It collects repo knowledge that is useful to consult quickly, but it is not
the concrete implementation.

## Trust Model
- Always consult `llm-wiki` before non-trivial work.
- Do not blindly trust it, especially after a period of active change in a
  given area.
- Cross-check important claims against code, tests, build scripts, config
  files, and current behavior.
- Update the relevant page and `log/recent.md` whenever you confirm drift,
  fill a gap, or change project behavior.
- Do not conflate the wiki with the project code. If the wiki and the code
  disagree, the code/tests/build scripts win.

## Recommended Read Order
- Start here to find the right page.
- If you are about to understand or change code in an unfamiliar area,
  first orient with `repo-map.md` (the code map: layout, subsystem
  ownership, build pipeline, test matrix, important paths) before reading
  the topic page.
- Read `current.md` next for a compact current-state summary and routing.
- Read `log/recent.md` after that when you need recent historical context
  for a changing area. For older entries, consult the relevant
  `log/archive-*.md` file.
- For build and tooling questions, read `<build.py.md or equivalent>` and
  `codestyle.md`.

## Content Catalog
<!-- One bullet per topic page, in the same style as this example. -->
- `codestyle.md`
  - Style/tooling rules, formatting conventions, naming, and common tree
    conventions. Last verified <YYYY-MM-DD>.
- `current.md`
  - Compact current-state summary and token-efficient routing into the
    longer wiki pages.
- `repo-map.md`
  - **Code map**: top-level layout, subsystem/module ownership, build
    pipeline structure, test matrix, and important paths.
- `known-debt.md`
  - Deliberately accepted debt with its reasoning, so later audits don't
    re-derive it and later agents don't "fix" it without weighing the same
    trade-off.
- `debug-tools.md`
  - Available debug commands, tool paths, and diagnostic workflows.
- `log.md`
  - Stub pointing to `log/recent.md` (recent activity) and
    `log/archive-*.md` (older archives).

## Page Maintenance
- Each page should keep `Last cross-checked`/`Last verified`, `Primary
  sources`, and an `Open questions / stale-risk` section current.
- Plan docs and old comments may be useful context, but they are secondary
  sources. Prefer live code, tests, config, and build scripts.
- Keep `current.md` compact and current. Move detailed history into the
  topical pages and `log/recent.md` (or the relevant archive) instead of
  expanding the compact entrypoint indefinitely.
