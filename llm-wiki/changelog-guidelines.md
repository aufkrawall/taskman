<!--
SPDX-License-Identifier: MIT
Copyright (c) 2026 aufkrawall
-->

# Changelog and Release Notes Guidelines

Use this page as a reusable baseline for project changelog and release-note maintenance. The default integration owns creation of a root `CHANGELOG.md` when the target has no changelog or equivalent release log, unless explicit repository policy or the user opts out. Adapt project-specific commands, categories, versioning, and release automation instead of copying assumptions from another project.

## Purpose

Changelog entries should make completed work easy to understand from the user's or operator's point of view. Keep the changelog continuously current during development so release preparation does not depend on reconstructing intent from old commits.

## Core principles

### 1. Describe the observable issue or capability

- Every entry should identify the user-facing behavior, defect, compatibility issue, operational problem, or capability that changed.
- Do not write purely internal implementation notes without explaining their practical effect.
- Name affected applications, APIs, platforms, protocols, subsystems, or configurations when that context materially helps users understand scope.

Prefer:

```markdown
- **Recording shutdown crash:** fixed a deadlock when stopping capture while a background privacy check was active.
- **DirectX 12 overlay shutdown:** fixed a crash during application exit while graphics work was still draining.
```

Avoid:

```markdown
- **Locking:** improved recursive mutex handling.
- **D3D12 hook:** changed fence cleanup.
```

### 2. Keep entries highly scannable

- Start each bullet with a concise bold anchor: `- **<Anchor>:** <details>` or `- **<Anchor>** <details>`.
- A reader skimming only the bold anchors should understand the release's notable changes quickly.
- Put the symptom or capability first, then the implementation detail only when it adds useful context.
- Keep one logical change per bullet where practical.

When the project uses Keep a Changelog-style categories, prefer the standard headings:

- `### New`
- `### Improved`
- `### Fixed`
- `### Changed`
- `### Deprecated`
- `### Removed`
- `### Security`

Use the target repository's established categories when they differ.

### 3. Create and update the changelog by default

- If the repository has no changelog or equivalent release log, initialize a root `CHANGELOG.md` from the reusable baseline with `# Changelog` and `## Unreleased`, unless explicit repository policy or the user opts out.
- When the repository maintains `CHANGELOG.md`, update the current unreleased section as part of completing changelog-worthy work.
- Do not defer all changelog writing to release preparation; that loses problem context and makes omissions more likely.
- Keep the unreleased section aligned with completed changes since the last published release.
- Follow the repository's existing definition of changelog-worthy work. If none exists, prioritize user-visible fixes, behavior changes, compatibility changes, new capabilities, removals, security fixes, and meaningful operational improvements.

### 4. Keep release notes aligned with the changelog

- When GitHub/GitLab or another release system publishes release notes, prefer deriving them from the same changelog section rather than maintaining two independent summaries.
- If release automation already validates, extracts, promotes, or publishes changelog sections, preserve that workflow and use its validation commands before release.
- Do not invent project-specific release commands in copied templates. Document only commands verified in the target repository.

### 5. Frame native capabilities natively

- Describe the project's own behavior, settings, APIs, and operating modes directly.
- Avoid unnecessary claims of parity with third-party tools or products.
- Mention third-party software when it is actually relevant to interoperability, compatibility, migration, or a verified defect.

## Recommended entry structure

```markdown
# Changelog

## Unreleased

### New

- **New capability:** added a user-visible feature and explain the practical effect.

### Improved

- **Existing workflow:** improved performance, robustness, usability, or diagnostics in a way users can observe.

### Fixed

- **Specific failure:** fixed the concrete bug or compatibility problem and identify the affected scope when useful.
```

If the target repository already has a different changelog schema, preserve it unless the user explicitly requests migration.

## Agent workflow

For projects using this baseline:

1. Preserve an existing changelog/equivalent; if none exists, initialize root `CHANGELOG.md` before the next agent-created commit unless explicitly opted out.
2. For changelog-worthy work, update the unreleased section before committing.
3. Describe the practical issue or capability first.
4. Use the repository's established categories and style; otherwise use the scannable bold-anchor format above.
5. Keep the entry scoped to task-owned changes.
6. Run any repository-provided changelog validator, release-note extractor, formatter, or test that applies.
7. Review the changelog together with the code/documentation diff so the description matches the implemented behavior.

## Invariants and guardrails

- Do not claim a fix, supported platform, compatibility result, performance improvement, or security property that was not established by the task's evidence.
- Do not copy version numbers, release tags, product names, or automation commands from another repository into the target project.
- Do not let release notes drift from the changelog when both describe the same release.
- Keep internal implementation detail subordinate to the user-visible effect.
- Preserve links to issues, pull requests, migrations, or upgrade notes when they materially help users act on the change.
