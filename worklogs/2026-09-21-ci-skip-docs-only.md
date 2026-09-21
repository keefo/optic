# Dated Worklog: 2026-09-21 - Skip CI Build Jobs for Docs-Only Changes

Status: **implemented and locally tested**; GitHub verification pending (see Validation).

## Objective

User request: when a change only touches documentation (no Rust or other
build inputs), don't run the CI build. The `main` ruleset (id `23753137`)
requires two status checks, `Biome (web assets)` and
`Rust (Debian 13 arm64)`, and the Rust job takes minutes on an arm64 runner
even for a docs-only PR (seen on PR #3).

## Constraint That Shapes the Design

GitHub rulesets cannot make a required status check conditional on changed
paths. Workflow-level `paths-ignore` would stop the workflow from running
at all, so the two required checks would never report and docs-only PRs
would be blocked forever ("Expected"). GitHub does count a required check
whose job was **skipped** by a job-level `if:` as passing. So the design
keeps the ruleset unchanged and moves the decision into the workflow.

## Design

- New first job `changes` ("Detect changed scope") computes the changed
  file list and runs `scripts/ci-changed-scope.sh`, which prints
  `docs_only=true|false`.
- Diff range: `pull_request` uses PR base SHA...head SHA; `push` uses
  `before..sha`. A new branch (all-zero `before`), `workflow_dispatch`, or
  any git failure means "not docs-only" (run everything).
- Docs-only means **every** changed file matches: `*.md` anywhere
  (README, setup.md, CLAUDE.md, AGENTS.md, docs, worklogs), `docs/**`,
  `worklogs/**`, `case/**` (3D-print STL models, not code). Anything else,
  including `.github/**`, `scripts/**`, `src/web/**`, `Cargo.*`,
  `verify.sh`, and `systemd/**`, runs the full CI. An empty file list is not docs-only.
- `web-lint` and `rust-arm64` get
  `needs: changes` and
  `if: ${{ !cancelled() && needs.changes.outputs.docs_only != 'true' }}`.
  This fails open: if `changes` itself fails, its output is empty, so both
  checks still run rather than being skipped into a false pass.

## Acceptance Criteria

1. A docs-only PR shows both required checks as skipped, and the PR is
   mergeable without running the Rust build.
2. A PR touching any non-docs path runs both jobs exactly as before.
3. A failure in the `changes` job leads to the build jobs running, not
   skipping.
4. No ruleset change is required.

## Test Plan (before implementation)

1. Local: run `scripts/ci-changed-scope.sh` against file lists covering
   docs-only, mixed, code-only, `.github`-only, `case/`-only, `AGENTS.md`,
   and empty input. Check the output for each.
2. Local: `bash -n` on the script; parse `ci.yml` as YAML (Ruby's
   bundled `yaml`, since actionlint and yq are not installed).
3. GitHub: this PR changes `.github/` and `scripts/`, so it must run
   both jobs in full (criterion 2).
4. GitHub: after this merges, rebase docs-only PR #3 onto `main` and
   confirm both required checks are skipped and the PR is mergeable
   (criterion 1).
5. Criterion 3 is checked by review of the `if:` expression (a
   forced detector failure on GitHub is not planned).

## Implementation Summary

- `scripts/ci-changed-scope.sh` (new): the classifier (stdin file list to
  `docs_only=true|false`).
- `.github/workflows/ci.yml`: new `changes` job; `web-lint` and
  `rust-arm64` get `needs: changes` plus the fail-open `if:`.
- `docs/optic-daemon-ci-cd.md` §5.1: documents the `changes` job and why
  `paths-ignore` is not used.
- The ruleset is unchanged.

## Validation

Environment: macOS dev machine (bash 3.2 via `/usr/bin/env bash`).
actionlint, shellcheck, and yq are not installed, so they were not run.

1. `bash -n scripts/ci-changed-scope.sh`: clean.
2. Classifier cases, 11/11 **passed**: docs-only (docs, worklogs,
   README), AGENTS.md + CLAUDE.md, `case/` STL are all `true`; mixed docs+src,
   `src/web.rs`, `src/web/app.js`, `.github/workflows/ci.yml`, `Cargo.lock`,
   `verify.sh`, empty input, and blank lines only are all `false`.
3. `ruby -ryaml` parse of `ci.yml`: parses; `changes` has no `needs`;
   both build jobs have `needs: "changes"` and the expected `if:`.
4. The `changes` step's script, extracted from the YAML and run with
   `bash --noprofile --norc -eo pipefail` (GitHub's `bash` shell flags)
   against real commits, **passed** 6/6:
   - PR #3 range (docs-only): `docs_only=true`
   - PR #2 range (code): `docs_only=false`
   - push of docs-only commit `a6e1509`: `docs_only=true`
   - push with all-zero `before` (new branch): full CI
   - push with unknown `before` (force push): git error is caught, full CI
   - `workflow_dispatch`: full CI
5. GitHub, pending: this PR changes `.github/` and `scripts/`, so both
   build jobs must run in full. After merge, PR #3 (docs-only) rebased onto
   `main` should show both required checks skipped and be mergeable.

## Remaining Limitations / Follow-up

- A PR that edits both docs and code runs the full CI (by design).
- `*.md` matches anywhere, including under `src/`. There are no `.md` build
  inputs today; revisit if that changes.
- On a branch with an open PR, the `push` run and the `pull_request` run
  both still happen (pre-existing trigger setup). Each skips its build jobs
  for docs-only changes.
