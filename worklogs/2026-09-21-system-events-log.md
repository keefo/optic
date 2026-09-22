# Dated Worklog: 2026-09-21 - System Events Log (boot, reboot, shutdown)

Status: **deployed (0.1.30, full) and hardware-validated on the Pi, including a real reboot; awaiting user acceptance**

## Objective

User request: "in our logging system, can we also log system bootup, shutdown
and reboot events?" The user chose (2026-09-21) a new events log, shown on
the Capture History page, over ntfy push notices. Follows
`worklogs/2026-09-21-shutdown-button.md`.

## Findings Before Design

- There is no general event log. Capture history (`src/optic_capture_log.rs`,
  `history.db`) is capture-only. Alerts (`src/optic_alerts.rs`) push to ntfy
  and keep no history. Nothing reads the boot time or boot ID.
- `main.rs::shutdown_signal` has a graceful SIGTERM path. The next step after
  `shutdown requested` (camera shutdown) can hang: at the 0.1.29 → 0.1.30
  deploy, systemd SIGKILLed the old process after the stop timeout. So the
  stop event must be written **before** those steps.
- On the Pi (checked 2026-09-21, as `liam`): `/proc/sys/kernel/random/boot_id`
  and `/proc/stat` `btime` are readable. `systemctl list-jobs --no-legend
  --plain` works over D-Bus without privileges (empty when idle, exit 0).
  Systemd 257. The unit is `ProtectSystem=strict`, `PrivateUsers=no`, and
  `RestrictAddressFamilies` includes `AF_UNIX` (D-Bus).

## Acceptance Criteria

1. **Boot:** when the daemon starts in a new boot (a boot ID not seen
   before), one `boot` event is stored, timestamped at the kernel boot time.
   It says how the previous boot ended, based on that boot's last recorded
   event: `reboot`, `shutdown`, `daemon_stopped` (the daemon stopped, but a
   host shutdown was not seen), or `unexpected` (power loss, crash or
   watchdog reset). The first boot ever recorded has no previous boot.
   Restarting the daemon within one boot adds no second `boot` event.
2. **Reboot / shutdown:** on SIGTERM the daemon checks the system manager's
   job queue for a queued `reboot.target`/`kexec.target` (→ `reboot`) or
   `poweroff.target`/`halt.target` (→ `shutdown`), otherwise it records
   `daemon_stop`. This covers any orderly shutdown or reboot, whether it came
   from the dashboard, SSH, or the power button. The event is written before
   the camera, sync and scheduler shutdown steps.
3. **Dashboard requests:** Power menu → Reboot / Shut down and Restart daemon
   record `reboot_requested` / `shutdown_requested` /
   `daemon_restart_requested` (source `dashboard`) before running the
   command. A failed command records `request_failed` with the error.
4. `daemon_start` (with version) on every daemon start.
5. `GET /api/events?limit=N` (default 50, max 200) returns events newest
   first. The Capture History page shows a **System events** table
   (When / Event / Details).
6. Best-effort and bounded: a failed event write is logged and never fails a
   request or the shutdown. The store is a separate `events.db` in the state
   dir, so it never contends with `history.db`'s capture writes. It is
   pruned to the newest 5000 rows at startup. On non-Linux dev machines the
   boot info is absent: no `boot` event, everything else works.

## Test Plan

- U1 (unit, temp dirs): first start with boot A → `boot` (no previous) +
  `daemon_start`. Second start, same boot A → only `daemon_start`. Start with
  boot B after A's last event was `reboot` → `boot` with previous
  `ended: reboot`. After `shutdown` → `shutdown`; after `daemon_stop` →
  `daemon_stopped`; after `daemon_start` only → `unexpected`.
- U2 `parse_host_action` on sample `list-jobs --plain` output: reboot,
  kexec, poweroff, halt, empty, unrelated jobs.
- U3 list ordering (newest first, `boot` sorted by kernel boot time),
  limit clamp, prune keeps the newest N.
- U4 web: `GET /api/events` returns recorded events; the footer test still
  passes; the Capture History page has the events table and loads
  `/api/events`.
- L1 `cargo fmt --check`, `clippy -D warnings`, `cargo test --locked`,
  `biome ci`.
- B1 browser (static `src/web`, stubbed `fetch` for `/api/events`): each
  kind renders its label and details, and an empty or failed load shows a
  message.
- P1 Pi (full deploy, user approval): after the deploy restart,
  `/api/events` shows `boot` (first ever recorded, no previous) +
  `daemon_start`. The old binary's stop is not logged, because it predates
  this feature.
  Restart daemon from the dashboard → `daemon_restart_requested`,
  `daemon_stop`, `daemon_start`.
- P2 Pi real reboot (**only with explicit user approval**): Power → Reboot
  → after it returns: `reboot_requested`, `reboot`, `boot` with previous
  `ended: reboot`, `daemon_start`. If not approved, U1/U2 cover the logic and
  P2 stays in the user-verification steps.

## Implementation

| File | Change |
|------|--------|
| `src/optic_events.rs` (new) | `SystemEventLog` (own `events.db`, WAL, prune to 5000 at open), `BootInfo::read_host()` (`/proc` boot_id + btime), `record_startup`, `record`, `list`, `host_shutdown_action()` (`systemctl list-jobs --no-legend --plain`, 2 s timeout) + `parse_host_action`; 7 unit tests |
| `src/main.rs` | Opens `state_dir/events.db` and records startup; passes the log to `AppState::with_events` and `events_router`; `shutdown_signal` records `reboot`/`shutdown`/`daemon_stop` right after `shutdown requested`, before camera/sync/scheduler shutdown |
| `src/web.rs` | `AppState.events` + `with_events`; reboot/shutdown/restart handlers record the request first and `request_failed` on error; `events_router` / `GET /api/events` (limit default 50, clamp 1–200, 503 without a log); 3 tests |
| `src/web/capture-history.{html,js}` | **System events** section: When / Event (pill) / Details, Refresh button, empty and error states, all API text HTML-escaped |
| `docs/optic-daemon-system-events.md` (new), `docs/optic-daemon.md`, `docs/phase9-readonly-root.md` | Design, API entry, write-inventory row for `events.db` |

Design change from the plan: "the previous boot's last event" is chosen by
insertion order (`id`), not by timestamp, so a wrong early-boot clock cannot
pick the wrong event.

## Validation (Mac, 2026-09-21)

| Test | Result |
|------|--------|
| U1–U3 `optic_events::tests` | 7 pass: first boot has no previous; a restart in the same boot adds no boot event; previous boot `ended` is `reboot`/`shutdown` (also from the `*_requested` kinds), `daemon_stopped`, `unexpected`; no boot info → daemon events only, never taken for a previous boot; newest first + limit; prune keeps the newest 5000 |
| U2 `host_action_comes_from_queued_shutdown_targets` | pass (reboot, kexec, poweroff, halt, empty, unrelated job) |
| U4 `web::tests` | `events_endpoint_lists_newest_first_and_clamps_the_limit`, `events_endpoint_reports_a_missing_log`, `capture_history_page_shows_system_events` pass |
| L1 | `cargo fmt --check`, `clippy --all-targets -D warnings`, `cargo test --locked` (**169 passed**), `biome ci` (10 files, clean) |
| B1 Chrome, static `src/web`, `/api/events` stubbed | all 9 kinds render the right label, pill and details ("Previous boot ended with a reboot. Down for about 1m."; "unexpectedly … 1d 22h" in a red pill; "First boot recorded."); an error string containing `<b>` renders as text; the empty state reads "No system events recorded yet."; a 503 shows its error message |
| P1 full deploy (user approved) | exit 0; journal: `system event log ready path=/home/liam/.local/state/optic-daemon/events.db`; old daemon stopped cleanly (no stop timeout this time). `/api/events`: `1 boot` at 1790042149000 (= `/proc/stat` btime), `previous_boot: null`; `2 daemon_start 0.1.30` |
| P1 Restart daemon (`POST /api/system/restart-daemon`, the button's endpoint) | `3 daemon_restart_requested {source: dashboard}`, `4 daemon_stop {}`, `5 daemon_start`; no second `boot` |
| P2 real reboot (user approved; `POST /api/system/reboot`, the Power → Reboot endpoint) at 20:30:59 | Down 20:31:05, back 20:31:26. Events: `6 reboot_requested` (20:30:59), `7 reboot {target: reboot.target}` (20:31:01), `8 boot` (20:31:13, new boot 46d2f216) with `previous_boot.ended: reboot`, `last_event: reboot`, `9 daemon_start`. Host: `uptime -s` 20:31:13, new `boot_id`, `journalctl --list-boots` shows boot -1 ending 20:31:02. The previous boot's journal: `shutdown requested` → `host is going down target="reboot.target"` → `Stopped optic-daemon.service` in 89 ms; the service is active again |
| Served pages after deploy | `config.html` order station → time → celestial; `capture-history.html` has `events-body`; `capture-history.js` calls `/api/events`; `/` has `footer-power` with `aria-label="Power"` |

## Limitations

- `list-jobs` detection during a real host **reboot** was observed (P2). A
  real **shutdown** (`poweroff.target`) is covered by the unit test but has
  not been observed; it needs someone at the Pi to power it back on. If the system bus were already gone when the daemon gets SIGTERM, the
  event would fall back to `daemon_stop`, and the next boot would report
  `daemon_stopped` instead of `reboot`. A dashboard `reboot_requested` would
  still appear in the list just before it, but it does not change how the
  boot reads, because `daemon_stop` is the last event. In P2 the bus was still up.
- Clock caveat and SIGKILL/power-loss behaviour: see the design note §4.

## User Verification (steps 1–3 done by Claude on 2026-09-21; please recheck in the UI)

1. Open Capture History → **System events**: expect `Pi booted` ("First
   boot recorded.") and `Daemon started` with the deployed version.
2. Footer → Restart daemon: expect `Daemon restart requested`, `Daemon
   stopped`, `Daemon started`.
3. When convenient, Power → Reboot: expect `Reboot requested`, `Pi
   rebooting` (`reboot.target`), then `Pi booted` "Previous boot ended with
   a reboot. Down for about …", then `Daemon started`.

## Changed Files (this feature)

`src/optic_events.rs` (new), `src/main.rs`, `src/web.rs`,
`src/web/capture-history.html`, `src/web/capture-history.js`,
`docs/optic-daemon-system-events.md` (new), `docs/optic-daemon.md`,
`docs/phase9-readonly-root.md`, this worklog.

## Next Steps

- User acceptance on the Capture History page.
- Observe one real Power → Shut down (expect `shutdown_requested`,
  `shutdown` with `poweroff.target`, then on power-on `boot` with
  `ended: shutdown`).
- Separate follow-ups, not caused by this work: the 0.1.29 stop timeout seen
  once at deploy; unknown paths return 500 instead of 404.
