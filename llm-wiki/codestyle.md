# Code Style

<!--
TEMPLATE NOTE: Record style rules that are either tool-backed (formatter/
linter config) or strongly reflected in the current tree. Delete this note
once real content replaces it.
-->

Last cross-checked: <YYYY-MM-DD>

Primary sources:
- `AGENTS.md`
- `<formatter config, e.g. .clang-format / .prettierrc / pyproject.toml>`
- `<linter config, e.g. .flake8 / .eslintrc / clippy.toml>`
- representative source files that reflect current convention

## Scope
This page records the style rules that are either tool-backed or strongly
reflected in the current tree. Local file conventions still win if a
touched subsystem clearly uses a different established pattern.

## Tool-Backed Rules

<!-- Per language: column limit, indent style, brace style, naming
conventions the linter enforces, target language version, etc. -->

### `<language>`
- <rule>

## Common Tree Conventions

<!-- Naming conventions not enforced by tooling but consistently followed:
PascalCase vs snake_case, prefix conventions for globals/constants, header
guard style, etc. -->

- <convention>

## Practical Notes
- Do not run a whole-file automatic formatter on existing source unless
  explicitly requested; a tree with legacy formatting or mixed line endings
  can produce large unrelated diffs from a narrow intended edit.
- Preserve the touched file's existing formatting and line endings. Inspect
  the diff before building.
- Naming and local-pattern guidance is medium confidence and should be
  re-checked against the files you touch.
- If formatter output, local file style, and this page disagree, preserve
  the local subsystem's established pattern unless the user explicitly
  requested a formatting migration.

### Current lint debt and triage

<!-- If the project tracks a lint baseline/ratchet, summarize its current
state here and point to known-debt.md for the reasoning behind any
accepted exceptions. -->

<summary of current lint baseline state, or "no lint ratchet configured
yet">
