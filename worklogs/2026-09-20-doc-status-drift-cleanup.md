# Dated Worklog: 2026-09-20 - Doc/Worklog Status Drift Cleanup

Status: **implemented and textually verified** (documentation-only
change; no code, config, or Pi changes). Awaiting user review.

## Objective

A worklog review found several status statements in `docs/`, `setup.md`,
and older worklog headers that contradict later, verified worklogs. Per
`CLAUDE.md` ("If they disagree, do not guess: identify the mismatch and
resolve or document it"), bring those statements in line with the
recorded evidence.

## Mismatches Found

| # | Location | Stale claim | Evidence it is stale |
| --- | --- | --- | --- |
| 1 | `docs/optic-daemon.md` §intro "Implementation status" banner | "implements Phase 1 (`optic_web`) only", "`optic_scheduler` is not yet implemented", Phase 6 transfer timer "still runs in parallel" | Scheduler Phases 1a–1d and 2a–2j worklogs (all implemented/deployed/verified); `worklogs/2026-09-20-stale-iMac-ip-beszel-and-sync.md` found no transfer timer/service installed on the Pi |
| 2 | `docs/optic-daemon.md` Phase 1 validation table, "Reboot persistence" row + follow-on sentence | "an actual reboot test is pending" | `worklogs/2026-09-19-reboot-nonewprivileges-fix.md` (user rebooted via dashboard, daemon came back automatically, `/healthz` ok) and `worklogs/2026-09-19-timelapse-scheduler-phase1b.md` (real reboot) |
| 3 | `setup.md` §8 last paragraph | "A full reboot validation remains pending" | Same as #2 |
| 4 | `docs/optic-daemon-scheduler.md` status banner | "Design proposal, not implemented"; Decision #4 "still open"; Decision #6 "likely superseded" | Phase 1a–2j worklogs; the doc's own §7 and §10/§14 mark both resolved |
| 5 | `worklogs/2026-09-20-stale-iMac-ip-beszel-and-sync.md` status header | known_hosts gap is "the live, active risk" | Same worklog's "Second Addendum" — "Status: fully resolved", 15 queued files transferred and confirmed on the Mac |
| 6 | `worklogs/2026-09-18-timelapse-scheduler.md` status header | "planned, not implemented" | Superseded by the Phase 1a–2j worklogs |
| 7 | `worklogs/2026-09-18-system-control-panel.md` status header | B (reboot/restart-daemon) "functionally untested" | Reboot button user-verified in `2026-09-19-reboot-nonewprivileges-fix.md`. No worklog records a verified restart-daemon button press, so that part stays marked untested |
| 8 | `setup.md` §F intro (found during verification) | "scheduler and in-process sync workers are not yet implemented" | Same as #1 |

## Approach

- Docs (`docs/`, `setup.md`): rewrite the stale statements to the current
  state, citing the worklog that proves it.
- Worklogs (point-in-time records): do not rewrite history. For #6 and #7
  add a dated "Update 2026-09-20" line above the original status. For #5
  (a same-day header contradicting its own final addendum) correct the
  header to the final resolved state.
- No changes to code, scripts, systemd units, or the Pi.

## Test Plan (written before editing)

Documentation-only, so verification is textual:

1. `rg` for each stale phrase afterwards returns no current-state claims:
   `not yet implemented`, `Phase 1 (\`optic_web\`) only`,
   `reboot test is pending`, `reboot validation remains pending`,
   `Design proposal, not implemented`.
2. `git diff --stat` shows only the files listed above plus this worklog.
3. Re-read the diff to confirm every new claim cites a worklog and no
   secrets or unrelated edits were introduced.
4. `cargo test` is not required (no code touched); skipping it is recorded
   below, not implied as passed.

## Implementation Summary

Files changed:

- `docs/optic-daemon.md`: implementation-status banner now says all three
  subsystems are implemented and points to the scheduler design doc and
  worklogs; the Phase 6 transfer timer is described as not installed on
  the Pi; the reboot-persistence row and the sentence after it record the
  later passing reboots.
- `docs/optic-daemon-scheduler.md`: status banner changed from "design
  proposal, not implemented" to implemented/deployed with worklog
  pointers; Decisions #4 and #6 marked resolved/superseded, matching §7,
  §10 and §14.
- `setup.md`: §F intro no longer says the scheduler and sync workers are
  unimplemented; §8's reboot-pending sentence records the confirmed
  reboot.
- `worklogs/2026-09-20-stale-iMac-ip-beszel-and-sync.md`: header now
  leads with the final "fully resolved" state; the earlier status text is
  kept, labelled "Earlier status".
- `worklogs/2026-09-18-timelapse-scheduler.md`,
  `worklogs/2026-09-18-system-control-panel.md`: a dated "Update
  2026-09-20" line added above the original, unchanged status.

## Validation

Environment: macOS dev machine, local repository only.

1. `rg -n "not yet implemented|Phase 1 \(\`optic_web\`\) only|reboot test is pending|reboot validation remains pending|Design proposal, not implemented|still runs in parallel" docs setup.md README.md`.
   The first run found one more stale line (`setup.md` §F, mismatch #8), which
   was then fixed. Second run: no matches (exit code 1). **Passed.**
2. `git status --short` / `git diff --stat`: only the six files above plus
   this new worklog. **Passed.**
3. Re-read the full `git diff`: every new claim cites a worklog; no secrets,
   code, or unrelated edits. **Passed.**
4. `cargo test` / `cargo clippy`: **skipped**, since no Rust or web code changed.

## Remaining Limitations / Follow-up

- Restart-daemon button: no worklog records a verified press, so the
  system-control-panel update keeps it marked untested. One press from the
  dashboard would close that.
- Only the specific stale statements above were fixed. Other historical
  sections that are clearly labelled as the original design (e.g.
  `docs/optic-daemon.md` "As Originally Designed (not implemented)",
  `config.toml` schema) were left alone because they already say so.
- Not addressed here (tracked separately from the review): no browser
  click-through of Phase 2 pages; PolicyKit rules not captured in setup
  scripts; iMac DHCP reservation.

## User Verification

Review `git diff` for the six changed files and confirm the wording
matches your understanding of the current deployment.
