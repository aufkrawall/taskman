<!--
SPDX-License-Identifier: MIT
Copyright (c) 2026 aufkrawall
-->

# Secret Leak Prevention

This procedure is part of the normal agent commit workflow. It applies whenever an agent is authorized to create a Git commit, independently of whether a full security audit is being performed.

The goal is to prevent credentials, private keys, tokens, sensitive configuration, private data, local diagnostic artifacts, or accidentally copied secrets from entering Git history or commit metadata.

## Sensitive material to protect

Treat at least the following as sensitive unless repository policy explicitly establishes otherwise:

- passwords, API keys, bearer tokens, OAuth/client secrets, session cookies, refresh tokens, access tokens, cloud credentials, database credentials, and connection strings containing credentials
- private keys, signing keys, certificate private material, recovery codes, seed phrases, and authentication cookies
- real `.env`/local configuration values, credential stores, auth caches, package-manager tokens, and machine-specific secret files (including local uncommitted `tool-paths.env`)
- dumps, captures, traces, logs, exported requests, screenshots (`shots/`, `graphpngs/`, `*.png`), databases, user data, or generated artifacts (`dist/`, `target/`) that may contain credentials or personal/private information
- credentials or sensitive values embedded in source, tests, fixtures, examples, generated files, documentation, changelog entries, commit messages, trailers, or other Git metadata

Public identifiers, intentionally public test fixtures, and documented placeholders are not automatically secrets, but verify that they are non-sensitive before committing them.

## Mandatory pre-commit check

Before every agent-created commit:

1. Inspect the complete staged-file inventory and relevant untracked files intended for staging.
2. Review the staged patch, not only the working-tree diff. Use Git's staged diff/check facilities (`git diff --staged`) or an equivalent repository workflow.
3. Check for unexpectedly staged local configuration (`tool-paths.env`), credential files, keys, dumps, logs, captures, databases, generated artifacts, or other sensitive files.
4. Run the repository-provided secret scan/check when one exists (`python build.py --audit` invokes `gitleaks detect --no-banner --redact` when installed).
5. When a suitable local secrets scanner such as `gitleaks` or `trufflehog` is already available, use it on the staged/changed content or repository scope appropriate to the project.
6. If no scanner is available, the check is still mandatory: perform a targeted manual review/search for credential markers and suspicious secret-bearing files, and record that automated scanning was unavailable.
7. Review the planned commit message/body/trailers before committing. Do not paste raw secrets, sensitive log excerpts, private URLs containing credentials, tokens, or personal data into commit metadata.
8. If any suspected secret or sensitive artifact is found, stop the commit until it is removed, redacted, replaced with a safe fixture/placeholder, or explicitly established as safe to commit.

A clean scanner result does not replace staged-diff review. Secret scanners can miss custom formats, encoded values, private data, or sensitive artifacts that are not recognizable as credentials.

## Mandatory post-commit check

Immediately after every agent-created commit and before any push:

1. Inspect the exact commit that was created, including its complete patch, file list, commit message/body/trailers, and author/committer metadata. A command equivalent to `git show --format=fuller --stat --patch HEAD` is appropriate.
2. Confirm that the commit contains only intended task-owned files and no secret-bearing local/generated artifacts.
3. Re-run the repository-provided or available local secret scanner against the newly created commit or the relevant commit range when the tool supports Git-history/commit scanning.
4. If automated commit scanning is unavailable, manually re-check the committed patch and metadata for credential material and sensitive values.
5. Do not push, publish, open a release from, or otherwise share a commit that fails this check.

For a multi-commit outgoing branch, prefer an additional secrets scan/review over the complete outgoing range before pushing when practical.

## Remediation when a secret reaches a commit

If a real credential or other sensitive value is found after commit:

- stop before pushing or otherwise sharing the commit;
- remove the sensitive material from the working tree and rewrite/amend the affected local commit(s) as appropriate;
- do not quote the full secret in remediation notes, changelog entries, issue/PR text, or commit messages;
- treat a real credential as potentially exposed and rotate/revoke it according to repository/security policy, especially if the commit, patch, terminal output, logs, or scanner results may have left the local trusted environment;
- if the commit was already pushed/shared, follow the project's incident-response/history-rewrite procedure rather than assuming deletion of the latest branch tip removes the secret from all reachable history or caches.

## Reporting

When reporting secret-check results:

- state which staged/commit scope was checked and which scanner or manual fallback was used;
- report suspected secrets using a redacted fingerprint or location, never the complete value;
- distinguish "scanner unavailable" from "scan passed";
- do not claim that a repository is secret-free merely because one scan found no matches.
