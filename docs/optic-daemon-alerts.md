# Design Note: Health Alerts (`optic_alerts`)

Status: **implemented, merged (PR #6), and validated on the Pi**: live
ntfy delivery from the Pi was confirmed on the user's phone, and the user
accepted the work on 2026-09-21. Not soak-tested. Implementation record:
`worklogs/2026-09-20-health-alerts.md`. Code: `src/optic_alerts.rs`.

## 1. Purpose

Nothing notified the operator when the station stopped doing its job during
unattended operation. `optic_alerts` is an in-daemon health monitor that
watches signals the daemon already has, detects a small set of real failure
conditions, and sends one notification when a condition starts, a reminder
while it persists, and a recovery notice when it clears.

It is a passive observer, like `optic_capture_log`: it never touches the
camera, never changes scheduler/sync state, and a failure inside it (bad
config, notification delivery failure) never affects capture or transfer.

## 2. Why In-Daemon, Not Beszel

Beszel (`setup.md` § Monitoring Baseline) records host metrics — CPU and RP1
temperature, root disk, the `/mnt/capture` tmpfs (via `EXTRA_FILESYSTEMS`) —
but has no view of capture or sync health, and no Beszel alert/notification
configuration is recorded in this repository. More importantly, the Beszel
Hub runs on the **same iMac that receives `optic_sync` transfers**. An iMac
outage (the most likely real failure — see
`worklogs/2026-09-20-stale-iMac-ip-beszel-and-sync.md`) silences Beszel at
exactly the moment the sync backlog starts growing. Alerts therefore go
directly from the Pi to a notification service that does not depend on the
iMac.

Overlap with Beszel is limited to temperature and tmpfs usage; both are kept
here because they are only useful if they are delivered when the iMac is down.

## 3. Decisions (confirmed 2026-09-20)

| Decision | Choice |
| --- | --- |
| Channel | **ntfy** (default server `https://ntfy.sh`), private unguessable topic, optional access token. JSON publish via the system `curl` binary — no new crate (same shell-out pattern as `optic_sync` → `ssh`). |
| Credential location | `~/.config/optic-daemon/alerts.json`, mode `0600`, never in git. Path overridable with `OPTIC_ALERTS_CONFIG`. No change to the tracked systemd unit (readable under `ProtectHome=read-only`). |
| Thresholds | "Balanced" preset (§5) as built-in defaults; every value overridable in `alerts.json`. |
| Dashboard | Read-only `GET /api/alerts` **and** a footer badge on every page. |

## 4. Signals Used

All inputs are read from existing in-process handles; no existing module is
restructured.

| Signal | Source |
| --- | --- |
| Scheduler run state, next due capture | `SchedulerHandle::status()` (`run_state`, `next_capture_at`) |
| Shots that *should* have fired | `optic_scheduler::occurrences()` over the committed config (tmpfs cache, same file the scheduler reads) |
| Scheduled capture outcomes | `CaptureLog::query(source = "scheduler")`, newest first — sees every outcome even between polls. Fallback when the history DB is unavailable: the monitor's own record of `SchedulerStatus.last_capture` changes. |
| Sync queue and progress | `DataSyncManager::status()` (`enabled`, `paused`, `queued_files`, `transferred_files`, `connectivity`, `last_error`). There is no "last successful transfer" timestamp, so the monitor records when `transferred_files` last increased. |
| Capture tmpfs free space | `SystemStatusReader::snapshot()` → disk labelled `capture` |
| CPU temperature | `SystemStatusReader::snapshot()` → `cpu_temp_celsius` |
| Throttling / under-voltage | `vcgencmd get_throttled`, read by `optic_alerts` itself (Linux only; `None` elsewhere) |

A signal that is unavailable in a tick (e.g. no temperature on macOS) yields
"unknown" for that condition: its state is left unchanged — it neither fires
nor resolves.

## 5. Conditions and Default Thresholds ("Balanced")

| Key | Active when | Clears when |
| --- | --- | --- |
| `capture_stalled` | Scheduler `Running`, and at least one scheduled shot was due at least **15 min** ago, after both the later of (monitor start, last pause→resume) and the last successful scheduled capture, with no successful scheduled capture since. | A scheduled capture succeeds, or the scheduler is paused. |
| `capture_overdue` | Scheduler `Running` and `next_capture_at` is more than **5 min** in the past (actor or camera hung mid-capture). | `next_capture_at` advances or scheduler paused. |
| `capture_failing` | Scheduler `Running` and the most recent **3** scheduled captures all failed — counting only captures completed since the scheduler was last seen switching to `Running` (or since daemon start), so a resume or restart does not re-fire on stale failures. | The newest scheduled capture succeeded, or scheduler paused. |
| `sync_backlog` | Sync enabled and not paused, `queued_files > 0`, and no transfer progress for **30 min** (backlog clock starts when the queue becomes non-empty and restarts whenever `transferred_files` increases). | Queue drains to zero, a transfer succeeds, or sync is disabled/paused. |
| `capture_tmpfs_low` | Capture tmpfs free space **< 25 %** (64 MiB of 256 MiB). | Free space **> 35 %** (hysteresis). |
| `temperature_high` | CPU temperature **≥ 80 °C**. | **< 75 °C** (hysteresis). |
| `throttled` | `get_throttled` reports any *current* flag: under-voltage (bit 0), ARM frequency capped (bit 1), throttled (bit 2), soft temperature limit (bit 3). | All four current bits clear. |

Paused scheduler/sync are operator intent and do not alert (a paused sync is
still caught indirectly by `capture_tmpfs_low`). Capture failures from manual
dashboard captures are not counted — only `source = "scheduler"`.

### Debounce, recovery, repeat

Each condition runs through the same per-condition state machine, driven by
an explicit `now` (so the logic is a pure function testable with a fake
clock):

```
          raw=true                 held ≥ fire_after
 Clear ────────────▶ Pending ─────────────────────────▶ Firing ──▶ notify "FIRING"
   ▲                   │ raw=false                        │  ▲       (every `repeat_every`
   │                   ▼                                  │  │        while Firing/Recovering:
   └──────────────── Clear                      raw=false │  │ raw=true  notify "STILL FIRING")
   ▲                                                      ▼  │
   └──── notify "RESOLVED" ◀── clear ≥ recover_after ── Recovering
```

| Parameter | Default |
| --- | --- |
| `fire_after` | 5 min for `temperature_high` and `throttled`; 0 for the others (their thresholds already contain a time window, or use hysteresis) |
| `recover_after` | 5 min continuously clear |
| `repeat_every` | 6 h while still firing |
| Poll interval | 30 s |

A condition that flaps back to active while `Recovering` returns to `Firing`
silently — no new "FIRING" notification. That, plus hysteresis on the
threshold conditions, is the anti-spam mechanism.

## 6. Notification Delivery

- **ntfy JSON publish**: `POST <server>/` with body
  `{"topic", "title", "message", "priority", "tags"}`. Firing and reminder
  notifications use priority 4 (`high`) and tag `warning`; resolved uses
  priority 3 (`default`) and tag `white_check_mark`. Title format:
  `Optic <station>: FIRING captures stalled` / `... RESOLVED ...`.
- **Transport**: `curl -q --silent --show-error --fail --proto =https,http
  --max-time 10 --config -` (`-q` ignores any `~/.curlrc`; the body is sent
  with `data-raw` so a leading `@` is never treated as a file name).
  The URL, `Authorization: Bearer <token>` header and body are written to
  curl's **stdin** as a curl config file, so the token never appears in the
  process argument list, and it is never logged or served over the API
  (`NtfyConfig`'s `Debug` output redacts it).
- **Outbox**: undelivered notifications wait in a bounded in-memory queue
  (32 entries; oldest dropped with a warning when full). Each poll tick tries
  to flush it in order and stops at the first failure, so a network outage
  costs at most one `curl` attempt per 30 s. A reminder is not queued if an
  undelivered notification for the same condition is already waiting. The
  outbox does not survive a daemon restart.
- **Dry run**: with no config file, an invalid config, or `"enabled": false`,
  the monitor still evaluates every condition and serves `/api/alerts`, but
  notifications only go to the daemon log (`tracing::warn!`). Local
  development and unit tests always use a dry-run notifier.

## 7. Configuration File

`~/.config/optic-daemon/alerts.json` (override: `OPTIC_ALERTS_CONFIG`).
Unknown fields are rejected so a typo in a threshold name is reported rather
than silently ignored. A group- or world-readable file is still loaded but
reported as a warning in the log and in `/api/alerts`.

```json
{
  "enabled": true,
  "station_name": "optic",
  "ntfy": {
    "server": "https://ntfy.sh",
    "topic": "optic-<long random string>",
    "token": "tk_<optional access token>"
  },
  "thresholds": {
    "capture_stalled_after_secs": 900,
    "capture_overdue_after_secs": 300,
    "capture_failures_in_a_row": 3,
    "sync_backlog_after_secs": 1800,
    "tmpfs_low_fire_below_percent": 25,
    "tmpfs_low_clear_above_percent": 35,
    "temperature_fire_at_celsius": 80.0,
    "temperature_clear_below_celsius": 75.0,
    "sustained_secs": 300,
    "recover_after_secs": 300,
    "repeat_every_secs": 21600
  }
}
```

Only `ntfy.topic` is required when the file exists (1–64 characters of
`A-Za-z0-9-_`); everything else has the defaults above. Duration thresholds
above 30 days are clamped to 30 days (with a warning) so an extreme value
cannot overflow time arithmetic — release builds use `panic = "abort"`, so an
overflow would otherwise stop the whole daemon. Inverted hysteresis pairs and
`repeat_every_secs: 0` (reminders off) are reported as warnings. The file is read once at startup — edit it, then restart the
daemon. Setup (as `liam` on the Pi):

```bash
install -d -m 0700 ~/.config/optic-daemon
install -m 0600 /dev/null ~/.config/optic-daemon/alerts.json
$EDITOR ~/.config/optic-daemon/alerts.json
systemctl --user restart optic-daemon.service
curl -s http://127.0.0.1:8000/api/alerts
```

## 8. API and Dashboard

`GET /api/alerts` (read-only) returns the channel (`ntfy` or `dry_run`),
config load error/warnings, last evaluation time, every condition's state
(`clear`/`pending`/`firing`/`recovering`, since, detail, last notified), outbox
length, and last delivery error/success time. It never returns the topic,
token, or config path.

Every page's site header shows the same status pills, in this order:
**Daemon `<version>`**, **Camera** (streaming / busy / ready /
unavailable), and **Alerts** — `Alerts OK` (green), `N alerts: …` (red,
condition names; details on hover), or `Alerts: dry-run` when no channel is
configured — followed by any page-specific pills. They are built by the
shared `src/web/footer.js` (loaded on every page), which polls
`/api/alerts` every 15 s and, on pages other than the dashboard,
`/api/status` every 5 s (the dashboard's `app.js` keeps updating its own
Daemon/Camera pills). The Alerts pill stays hidden if the endpoint is
missing (older daemon). The Scheduler page's header "Running/Paused" pill
was removed at the user's request (2026-09-21): its Run control card
already shows the scheduler state.

## 9. Limitations

- **Not a dead-man's switch.** A crashed daemon, a crash-looping service, a
  powered-off Pi, or a lost network sends nothing. Covering that needs an
  external heartbeat check (e.g. a periodic ping to a hosted monitor that
  alerts on silence) — a follow-up, not part of this design.
- Requires outbound HTTPS from the Pi to the ntfy server.
- Uses the wall clock; a large NTP step can shorten or lengthen a time window
  once.
- Alert state is in memory: after a daemon restart, a still-present problem
  fires again (at most one duplicate per restart), and undelivered
  notifications are lost.
- An accidentally paused scheduler does not alert (treated as operator
  intent).
