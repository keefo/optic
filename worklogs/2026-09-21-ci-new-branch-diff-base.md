# Dated Worklog: 2026-09-21 - CI: Classify a New Branch's First Push Against `main`

Status: **implemented, merged (PR #9), new-branch path confirmed live**; a
docs-only first-push skip not yet observed on GitHub.

## Objective

User request: skip the build jobs (notably `Rust (Debian 13 arm64)`) for
docs-only updates. The skip from `worklogs/2026-09-21-ci-skip-docs-only.md`
already works for `pull_request` runs and later pushes. It does not work
for the **first push of a new branch**.

Observed on PR #8 (`docs/timelapse-accepted`, commit `a5d92c2`, docs only):

- The `pull_request` run gave `docs_only=true`, and all three build jobs
  were skipped.
- The `push` run logged `No usable diff range for push`, because GitHub
  sends an all-zero `before` for a new branch. It fell back to the full CI,
  so Biome, `Rust (Debian 13 arm64)`, and `Timelapse tool (macOS)` all ran.

So every new docs-only branch pays for one full build.

## Decision (user, 2026-09-21)

When `before` is all zeros, diff the push against the default branch:
`origin/<default>...SHA`, the files changed since the branch forked. The
alternative, dropping branch `push` triggers, was not chosen.

## Acceptance Criteria

1. First push of a new docs-only branch gives `docs_only=true`, so the
   build jobs are skipped.
2. First push of a new branch with code changes gives the correct
   classification (`docs_only=false`; `timelapse` true or false by path).
3. Still fails open to the full CI when the default branch ref is missing,
   when `git diff` fails, when the diff is empty, for an unknown `before`,
   and for `workflow_dispatch`.
4. `pull_request` and normal-push ranges are unchanged.
5. The default branch comes from
   `github.event.repository.default_branch`, not a hard-coded `main`.

## Test Plan (before implementation)

1. Local: extract the `scope` step from `ci.yml` (Ruby YAML) and run it
   with `bash --noprofile --norc -eo pipefail` in a clone whose
   `origin/main` matches GitHub.
   - New branch, docs-only: PR #8's head `a5d92c2` with a zero `before`.
     Expect `docs_only=true`.
   - New branch, tool code: PR #5's head `f7d85f3`, against the `main` it
     was based on. Expect `timelapse=true`.
   - New branch, daemon code: PR #6's head. Expect `docs_only=false`,
     `timelapse=false`.
   - New branch identical to `main` (empty diff): expect full CI.
   - Missing default branch ref (`DEFAULT_BRANCH=nope`): expect full CI.
   - Regressions: `pull_request` ranges for PRs #3/#5/#6, a normal push,
     an unknown `before`, and `workflow_dispatch` should match the previous
     results in `worklogs/2026-09-21-ci-timelapse-macos.md`.
2. Local: parse `ci.yml` as YAML, and run `bash -n` on the extracted step.
3. GitHub: the first push of this branch touches `ci.yml`, so it must run
   in full. Its `changes` log must show a real file list from the new
   `origin/main...SHA` range, not `No usable diff range`. That shows the
   new-branch path live. The docs-only skip on a first push can only be
   shown by a later docs-only branch, which is recorded as follow-up if not
   observed here.

## Implementation Summary

- `.github/workflows/ci.yml`, `changes` → `scope` step:
  - new env `DEFAULT_BRANCH: ${{ github.event.repository.default_branch }}`;
  - for a `push` with an all-zero `before`, if
    `refs/remotes/origin/$DEFAULT_BRANCH` exists (the checkout already uses
    `fetch-depth: 0`), the range is `origin/$DEFAULT_BRANCH...$SHA`.
  - Everything else is unchanged. An empty diff still reaches the classifier
    as empty input, which gives full CI.
- `docs/optic-daemon-ci-cd.md` §5.1: the fail-open list is updated, and the
  new-branch behavior is described.
- No change to `scripts/ci-changed-scope.sh` or to any job's `if:`.

## Validation (local)

The `scope` step was extracted from `ci.yml` with Ruby YAML (parses, and
`bash -n` is clean). It was run with `bash --noprofile --norc -eo pipefail`
in a scratch clone, with `refs/remotes/origin/main` set to the `main` each
branch forked from. **12/12 as expected**:

| Case | `before` | `origin/main` | Result |
|---|---|---|---|
| New branch, docs-only (PR #8 head `a5d92c2`) | zero | `b8ecbed` | `docs_only=true timelapse=false` (previously full CI) |
| New branch, tool (PR #5 head `f7d85f3`) | zero | `a56e01a` | `docs_only=false timelapse=true` |
| New branch, daemon (PR #6 head) | zero | its base | `docs_only=false timelapse=false` |
| New branch identical to `main` | zero | same | empty diff, so full CI |
| `DEFAULT_BRANCH=nope` | zero | | "No usable diff range", so full CI |
| `DEFAULT_BRANCH` empty | zero | | "No usable diff range", so full CI |
| PR #3 / #5 / #6 `pull_request` | | | unchanged: `true/false`, `false/true`, `false/false` |
| Normal push `a6e1509^..a6e1509` (docs) | | | `docs_only=true` (unchanged) |
| Unknown `before` | | | full CI (unchanged) |
| `workflow_dispatch` | | | full CI (unchanged) |

## GitHub Validation (recorded 2026-09-21, after merge)

- Run `35575800715` (`push`, the branch's own first push): the `changes`
  log lists `.github/workflows/ci.yml`, `docs/optic-daemon-ci-cd.md` and
  this worklog, taken from the new `origin/main...SHA` range rather than
  "No usable diff range". All jobs then ran in full, which is correct for a CI change.
- Independent review re-ran the docs-only case locally: PR #8's head
  `a5d92c2` against its fork base `b8ecbed` gives `docs_only=true
  timelapse=false`.
- Still to observe: a new docs-only branch whose first push skips the
  build jobs on GitHub.
