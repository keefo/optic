# Dated Worklog: 2026-10-07 - Remove Beszel Monitoring Entirely

Status: **removed from both hosts and the repo, verified on the Pi**.

## Objective

User decision (2026-10-07): "this beszel is not good enough, remove it
completely." Remove the Beszel hub (Mac) and agent (Pi), and every
reference in the repo.

## Why

Beszel never alerted on a single real outage, and it caused repeated
breakage of its own:

- It depends on a literal IP, because the agent's static Go resolver never
  issues mDNS (`setup.md`). Every DHCP lease change on the Mac broke it.
  Four occurrences: September (hub unreachable), 2026-09-29 (hub bound to a
  stale IP, dead for days), and again today, when the Mac moved
  `192.168.0.231` → `192.168.0.202`.
- While broken it logs a failure every 10 s. In the 16 MiB persistent
  journal that evicted the crash evidence the journal exists to keep: after
  today's power-cut outage the journal held **only two boots**, nothing
  before 2026-10-05.
- It did not detect today's outage (the Pi was off ~7 h) or the ~60 h
  outage on 2026-09-25, because its own agent was already broken.

Removing it also removes 15 MB of RSS on a 990 MB Pi.

## What This Leaves

**No host monitoring at all.** The daily digest and ntfy alerts run inside
the daemon, so a dead Pi still reports nothing. The external heartbeat in
`docs/optic-daemon-alerts.md` (designed, never built) remains the intended
answer and is now the only planned way an outage gets noticed.

## Scope

Pi (`liam@optic.local`):
- stop and disable `beszel-agent.service` (user unit), remove the unit, its
  `.bak` copies and its drop-in directory;
- remove `~/.config/beszel` (token), `~/.local/share/beszel-agent`,
  `~/.local/bin/beszel-agent`.
- `Linger=yes` stays: harmless, and `optic-daemon` is a system service now.

Mac:
- `launchctl bootout` and remove `~/Library/LaunchAgents/dev.beszel.hub.plist`;
- remove `~/.local/lib/beszel` (binary + `run-hub`),
  `~/.local/share/beszel` (data, 924 KB, includes `admin-credentials`),
  `~/Library/Logs/Beszel`.
- One tarball of the hub data is kept in this session's scratchpad as a
  short-lived safety net, not in the repo.

Repo (reference counts before the change): `verify.sh` 32, `setup.md` 11,
`scripts/setup-phase-06-pi-ram-transfer.sh` 6,
`docs/optic-daemon-alerts.md` 6, `docs/pi-services-audit.md` 3,
`docs/optic-daemon-system-service.md` 2,
`scripts/setup-phase-08-daemon-host-access.sh` 1,
`docs/phase9-readonly-root.md` 1, `docs/optic-daemon-digest-heartbeat.md` 1.

## Acceptance Criteria

1. No Beszel process or unit on either host, and nothing listening on 8090.
2. No `beszel` references outside `worklogs/` except deliberate history
   notes explaining the removal.
3. `verify.sh` runs clean with no Beszel checks: the monitoring phase is
   gone, and Phase 6 no longer requires the capture stage to be exported to
   a hub.
4. `optic-daemon` is unaffected: still active, captures and sync continue.
5. The journal stops accumulating connection errors.

## Test Plan (written before implementation)

1. `bash -n verify.sh scripts/setup-phase-06-pi-ram-transfer.sh
   scripts/setup-phase-08-daemon-host-access.sh`.
2. `rg -i beszel` outside `worklogs/` returns only intentional notes.
3. `cargo test --locked` (no Rust change expected; guards against stray edits).
4. On the Pi: `systemctl --user list-units | grep beszel` empty; the files
   above gone; `/api/status` still healthy; journal gains no new
   `WebSocket connection failed` lines.
5. On the Mac: `launchctl list | grep beszel` empty; nothing on port 8090.
6. `ssh liam@optic.local 'bash -s -- --phase 6' < verify.sh` → no FAIL.

## Implementation Summary

Hosts (done, with the commands recorded here):

- Pi: `systemctl --user disable --now beszel-agent.service`; removed the unit,
  its two `.bak` copies, the drop-in directory, `~/.config/beszel`,
  `~/.local/share/beszel-agent` and `~/.local/bin/beszel-agent`;
  `daemon-reload`.
- Mac: `launchctl bootout gui/501/dev.beszel.hub`; removed the plist,
  `~/.local/lib/beszel`, `~/.local/share/beszel` and `~/Library/Logs/Beszel`.
  A 108 KB tarball of the hub data sits in this session's scratchpad only.

Repo:

- `verify.sh`: the whole `Monitoring baseline: Beszel` section removed (agent
  service, linger, `HUB_URL`, listener, sensor, health, version, hub
  reachability, protected-file permissions); `EXPECTED_BESZEL_VERSION` and
  `EXPECTED_HUB_URL` gone; `monitoring` removed from `--phase` and its
  validation; Phase 6's "Beszel exports the capture RAM stage" check removed;
  the linger check is now INFO rather than FAIL, since nothing needs
  lingering now that `optic-daemon` is a system service.
- `scripts/setup-phase-06-pi-ram-transfer.sh`: no longer installs the
  `EXTRA_FILESYSTEMS` drop-in, creates its directory, or restarts the agent.
- `scripts/setup-phase-08-daemon-host-access.sh`: comment corrected.
- `setup.md`: the 28-line monitoring section removed, a note at the top
  recording the removal and that nothing watches the host now, and three
  stale sentences corrected.
- `docs/optic-daemon-alerts.md` §2 rewritten: why in-daemon, that Beszel is
  gone, and the structural limit that in-daemon alerts cannot report a dead
  host.
- `docs/pi-services-audit.md`, `docs/phase9-readonly-root.md`,
  `docs/optic-daemon-system-service.md`,
  `docs/optic-daemon-digest-heartbeat.md`: rows and sentences marked removed
  rather than silently deleted.

## Validation

| # | Check | Result |
|---|---|---|
| 1 | `bash -n verify.sh` and both setup scripts | clean |
| 2 | Pi: beszel units, files and processes | **0** of each |
| 3 | Pi: `optic-daemon` after removal | `active`, `/healthz` → `ok` |
| 4 | Mac: `launchctl list`, port 8090, files | **0** of each |
| 5 | `cargo test --locked` | **280 passed** |
| 6 | Pi: `verify.sh --phase 6` | **13 PASS, 0 FAIL** (the Beszel export check is gone) |
| 7 | Pi: `verify.sh` full run | **124 PASS, 2 WARN, 2 FAIL** — both FAILs are the pre-existing watchdog mismatch (installed 90 s vs scripted 15 s), unrelated to this change |
| 8 | Pi: `verify.sh --phase monitoring` | `Invalid phase: monitoring`, exit 64 |
| 9 | `rg -i beszel` outside `worklogs/` | only the deliberate history notes above |

## Failures Encountered

The first edit to `verify.sh`'s phase-validation list silently did nothing:
it was written with the wrong indentation and, unlike the other edits, had no
assertion. `--phase monitoring` therefore still validated and produced an
empty report. Caught by running it on the Pi, then fixed with an asserted
replacement. Every other edit in this change asserted its match.

## Remaining Limitations / Follow-up

- **Nothing monitors the hosts now.** The in-daemon digest and ntfy alerts
  cannot report a dead daemon or a dead Pi, which is how both the 2026-09-25
  (~60 h) and 2026-10-07 (~7 h) power-cut outages went unnoticed. The
  external heartbeat (`docs/optic-daemon-alerts.md` §8) is designed and
  unbuilt; it is now the only planned way an outage is noticed.
- `Linger=yes` is left enabled for `liam`. Nothing requires it; removing it
  is optional and needs `loginctl disable-linger`.
- The watchdog mismatch (90 s vs 15 s) is still open, from the services audit.
- The Mac's DHCP address moved again today (`192.168.0.231` →
  `192.168.0.202`). That no longer affects Beszel, but `optic_sync` still
  depends on `imac.local` resolving, which it currently does.
