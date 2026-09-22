# Design Note: System Events Log (`optic_events`)

Status: **implemented, deployed in 0.1.30, and validated on the Pi on
2026-09-21, including a real dashboard reboot (`reboot.target` detected, and
the next boot reported `ended: reboot`). A real shutdown has not been
observed yet.** Implementation record:
`worklogs/2026-09-21-system-events-log.md`. Code: `src/optic_events.rs`.

## 1. Purpose

Records when the Pi boots, reboots, and shuts down, and when the daemon
starts and stops, so the operator can see after the fact whether the station
went down cleanly or lost power. Shown as **System events** on the Capture
History page.

Like `optic_capture_log`, it is a passive observer. Every write is
best-effort: a failure is logged and never fails a request or holds up a
shutdown.

## 2. Events

| `kind` | When | `detail` |
|--------|------|----------|
| `boot` | Daemon start in a boot ID not seen before; timestamped at the kernel boot time (`/proc/stat` `btime`) | `previous_boot`: `null` for the first boot recorded, else `{boot_id, ended, last_event, last_event_at_unix_ms}` |
| `daemon_start` | Every daemon start | `{version}` |
| `reboot` | SIGTERM while the system manager has `reboot.target` or `kexec.target` queued | `{target}` |
| `shutdown` | SIGTERM while `poweroff.target` or `halt.target` is queued | `{target}` |
| `daemon_stop` | SIGTERM with no shutdown job queued (daemon restart or stop) | `{}` |
| `reboot_requested`, `shutdown_requested`, `daemon_restart_requested` | Power menu → Reboot / Shut down, or Restart daemon; recorded before the command runs | `{source: "dashboard"}` |
| `request_failed` | The reboot or shutdown command failed | `{action, error}` |

`previous_boot.ended` comes from the previous boot's last recorded event, by
insertion order:

- `reboot` / `shutdown`: an orderly reboot or shutdown was seen (or at least
  the dashboard request was).
- `daemon_stopped`: the daemon was stopped first, so how the host went down
  was not recorded.
- `unexpected`: nothing marked an orderly end, i.e. power loss, a kernel
  crash, or a watchdog reset.

Reboots and shutdowns started outside the dashboard (SSH, the power button)
are still recorded as `reboot`/`shutdown`, because detection uses the system
manager's job queue (`systemctl list-jobs`, read-only over D-Bus, no
PolicyKit action, 2 s timeout), not the dashboard request.

The stop event is written first in the SIGTERM path, before the camera, sync
and scheduler are shut down. Those steps can hang until systemd SIGKILLs the
process.

## 3. Storage and API

- `~/.local/state/optic-daemon/events.db`, SQLite in WAL mode, one table
  `system_events(id, occurred_at_ms, kind, boot_id, detail_json)`. It is a
  separate file from `history.db` so its writes never contend with capture
  inserts (that connection has no busy timeout).
- Pruned to the newest 5000 rows at daemon start.
- `GET /api/events?limit=N`: newest first by timestamp; default 50, clamped
  to 1–200. Returns `503` if the log could not be opened.
- On non-Linux development machines there is no boot information, so no
  `boot` events are recorded. The other events work.

## 4. Limitations

- Timestamps come from the system clock. If the clock is wrong early in a
  boot (before NTP syncs, without an RTC battery), a `boot` or
  `daemon_start` event can carry the wrong time, and the "Down for about …"
  estimate is off. Choosing the previous boot's last event does not depend
  on the clock.
- A SIGKILL, power loss, or crash writes no stop event. That absence is what
  `unexpected` reports on the next boot.
- Under a read-only root (Phase 9), `events.db` must live on the data volume
  with `history.db` (`docs/phase9-readonly-root.md` §2).
