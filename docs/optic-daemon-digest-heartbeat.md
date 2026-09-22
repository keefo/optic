# Design Note: Daily Digest and External Heartbeat

Status: **implemented, deployed to the Pi (2026-09-22, branch build) and
validated there: import, masking, test notification, on-demand digest,
heartbeat arm, replacement and cancel on the real ntfy.sh. Not yet
observed: a scheduled 08:00 digest on the Pi, a delivered silent alert,
the "checking in again" notice. Not user-accepted.** Record:
`worklogs/2026-09-21-digest-heartbeat.md`. Extends the health alerts
(`docs/optic-daemon-alerts.md`); code: `src/optic_digest.rs`,
`src/optic_heartbeat.rs`, wired into the alerts actor in
`src/optic_alerts.rs`.

## 1. Purpose

The health alerts (`optic_alerts`) only speak when something is wrong, and
only while the daemon is alive to notice. Two additions close the gaps:

1. **Daily digest** — one low-priority ntfy message per day summarising the
   last 24 h, so a quiet phone means "fine", not "maybe dead", and slow
   trends (temperature, backlog, missed frames) are visible.
2. **External heartbeat (dead-man's switch)** — an alert raised *outside*
   the Pi when the Pi stops checking in: power loss, a crashed or
   crash-looping daemon, a hung health loop, or a lost network. Until now a
   dead station sent nothing (`docs/optic-daemon-alerts.md` §9).

Both are passive, like the alerts: they never change camera, scheduler or
sync state, and any failure inside them (bad config, delivery failure, an
unwritable state file) is logged and never affects capture or transfer.

## 2. Decisions (confirmed by the user, 2026-09-21)

| Decision | Choice |
| --- | --- |
| Heartbeat mechanism | **ntfy dead-man's switch** on the existing alerts topic: the Pi re-publishes a *scheduled* ntfy message with a fixed sequence ID; each publish replaces the pending one, so it is only delivered if the Pi stops re-arming. No new account or service. |
| Heartbeat timing | Re-arm every **10 min**; the alert is delivered **30 min** after the last check-in. ~144 publishes/day, leaving room in an ntfy.sh free-tier daily quota for alerts and the digest. |
| Heartbeat gating | Withheld while a **capture-path** condition is active (see §4.3) or the camera was not detected at startup. A paused scheduler still checks in. |
| Where settings live | **One config file**: a new `notifications` section of the global `config.json` (next to the Station settings; §5) holds the channel, digest, heartbeat and alert thresholds, edited from a **Notifications** card on the Config page. `~/.config/optic-daemon/alerts.json` is imported once, automatically, and then no longer read (the user does not want separate config files). |
| Secrets in the API and UI | Topic and token are **write-only**: `/api/status` masks them (§6.1), the notifications API returns only masked hints (first 4 + `****` + last 4, fully masked under 16 characters), and the UI replaces them only when a new value is typed. |
| Applying changes | Immediately, without a restart; switching topic or turning the heartbeat or ntfy off first **cancels** the armed heartbeat message on the old topic (§4.6). |
| "Expected frames" | Counted **only while the scheduler was Running**. Run-state changes are recorded by the daemon and persisted, so a restart does not lose them. Paused time is reported separately. |
| Digest send time | **08:00 in the Station timezone** (`ScheduleConfig.station.timezone`, UTC if unset), DST-aware. Covers the previous 08:00 → this 08:00. |
| Digest content | The standard set plus alerts fired and system events (§3.2). |
| Dashboard | Heartbeat and digest status added to `GET /api/alerts` and to the existing **Alerts pill tooltip** (no new pill). |
| UI fields | ntfy on/off, server, topic, token, station name; digest on/off and send time; heartbeat on/off, interval and alert delay; a **Send test notification** button. Alert thresholds live in the same section but are not shown in the UI; a UI save keeps them. |

Rejected: healthchecks.io (new account; better recovery/history, but the
user preferred no new service), Uptime Kuma or healthchecks on the iMac
(shares the iMac failure domain with Beszel and the sync receiver).

## 3. Daily Digest

### 3.1 Schedule

- `due(now)` = the most recent `send_at` (default `08:00`) in the station
  timezone at or before `now`, converted to UTC. On a DST gap day the next
  valid local time is used; on an overlap the earlier instant.
- The digest window is **[previous due, due)** — 24 h, or 23/25 h across a
  DST change.
- A digest is sent on the first tick where `last_digest_due < due`, as long
  as `now - due ≤ 12 h` (catch-up after a restart or outage in the
  morning). Later than that, the digest is skipped and marked sent; the next
  morning's digest covers its own window only.
- First start with no saved state: the current `due` is marked as already
  sent, so the first digest is the next morning's, with a full window of
  samples rather than a mostly empty one.
- Delivery failure: the rendered digest stays pending and is retried every
  tick (30 s) until delivered or 12 h after `due`, then dropped with a
  warning. Only one digest is ever pending.

### 3.2 Content

| Line | Source |
| --- | --- |
| Frames captured vs expected, missed, success % | Successful `source = "scheduler"` rows in `history.db` in the window, vs. the committed schedule replayed (`optic_scheduler::forecast`) over the window's **Running** segments |
| Paused time | Run-state change log (§3.3) |
| Longest gap | Largest *running* time between consecutive successful scheduled frames (paused time inside a gap is not counted) |
| Failures + last error | Failed scheduled rows in the window |
| Manual captures | `source = "web_ui"` rows |
| Bytes captured | Sum of `bytes_total` over all rows in the window |
| Bytes / files transferred | Hourly deltas of `SyncStatus.transferred_bytes/_files` (an in-memory counter that restarts with the daemon; a decrease is treated as a restart) |
| Sync backlog | `SyncStatus` at send time: queued files/bytes, connectivity, last error |
| CPU temperature min/max | Sampled every tick into hourly buckets |
| Free space | Capture tmpfs and state disk at send time, plus the capture tmpfs minimum over the window |
| Alerts fired | `FIRING` transitions recorded by the alerts actor in the window, plus conditions still active |
| System events | `events.db` rows in the window: boots (and how many were `unexpected`), daemon starts |
| Heartbeat | Check-in state, last check-in, times withheld in the window |
| Uptime and version | Host uptime, daemon uptime, `CARGO_PKG_VERSION` |

Delivered as ntfy priority 2 (`low`), tag `bar_chart`. Title:
`Optic <station>: daily digest 412/432 frames` (or `… scheduler paused`).
Times in the message are in the station timezone.

Example:

```
Mon 21 Sep 08:00 → Tue 22 Sep 08:00 PDT
Frames: 412 of 432 expected (95%), 20 missed
Paused: 0m · Longest gap: 25m (03:10 → 03:35)
Failures: 3 (last: camera timeout)
Manual captures: 2 · Captured 1.2 GiB
Transferred: 414 files, 1.1 GiB · Queue: 0 files (online)
CPU: 41.2–63.5 °C
Free: capture 230.0 MiB / 256.0 MiB (min 180.0 MiB) · state 20.1 GiB
Alerts fired: capture stalled ×1 · active: none
System: 1 boot (1 unexpected), 2 daemon starts
Heartbeat: OK, last check-in 07:58, withheld 0×
Uptime: Pi 3d 4h · daemon 1d 2h · v0.1.31
```

### 3.3 Persisted state

`~/.local/state/optic-daemon/digest_state.json` (inside the unit's
`ReadWritePaths`; written atomically via temp file + rename):

- `last_digest_due` — the last window end sent (or skipped).
- `run_changes` — `(at, running)` transitions, pruned to the latest one
  before `now - 48 h`. The scheduler's run state is itself durable
  (`schedule_run_state.json`), so the state between two recorded changes is
  the earlier one, including while the daemon was down: a station that was
  Running before and after a power cut is correctly charged with the shots
  it missed during the outage. A change seen at startup (state differs from
  the last saved one) is recorded at the startup time.
- `buckets` — hourly `{temp min/max, capture free min, transferred
  bytes/files}`, the last 26 hours.
- `alerts_fired` — `(condition, at)`, bounded to 100, pruned to 48 h.
- `heartbeat` — last check-in time and withheld timestamps (§4.4).

Written when a run-state change, an alert firing, a check-in, a digest send,
or an hourly bucket rollover occurs — at most about 170 small writes a day
with the heartbeat enabled. An unreadable or corrupt file is logged and
replaced with fresh state (the next digest then says the window is only
partly covered).

## 4. External Heartbeat

### 4.1 Mechanism

Every `interval_secs` (600) while healthy, the alerts actor publishes:

```
POST <server>/<topic>/<sequence_id>          # sequence_id default "optic-heartbeat"
X-Delay: 30m                                 # alert_after_secs, whole minutes
X-Title: Optic <station>: no check-in for 30m
X-Priority: 5
X-Tags: rotating_light
Authorization: Bearer <token>                # only if configured

Optic <station> last checked in at 14:05 PDT. The Pi, the daemon or its
network is down, or the daemon withheld its check-in because scheduled
captures are failing (see earlier alerts).
```

ntfy deletes the pending scheduled message with the same sequence ID and
stores the new one (docs.ntfy.sh "Scheduled delivery", "Dead man's
switch"). While the Pi keeps re-arming, nothing is delivered. When it stops,
the last message is delivered 30 min after the last check-in — by the ntfy
server, not the Pi.

Transport is the same `curl` stdin-config path as alerts (token never in
argv, logs or the API). Header values are restricted to printable ASCII
(the station name is sanitised).

A failed publish (network down, ntfy unreachable) is retried on every tick
until one succeeds. The previously armed message keeps counting down, so a
long outage produces the alert as intended.

### 4.2 Recovery notice

The last successful check-in time is persisted. When a check-in succeeds
more than `alert_after_secs` after the previous one, the silent alert has
almost certainly been delivered, so the daemon also sends
`Optic <station>: checking in again` (priority 3, tag `white_check_mark`)
with the silent period ("no check-in from 14:05 to 15:12, 1h 7m"). This
also covers a Pi that was off overnight.

### 4.3 Gating: when the heartbeat is withheld

The external check must mean "the station is doing its job", not just "a
process is running". The daemon **does not re-arm** while:

1. `capture_stalled`, `capture_overdue` or `capture_failing` is **firing or
   recovering** (notified and not yet resolved — the same set the Alerts
   pill counts as active). These say the station is not producing frames it
   should. They only evaluate while the scheduler is Running, so a paused
   station keeps checking in (pausing is operator intent, as for alerts).
2. The camera was **not detected** at daemon startup (`OpticCamera::probe`),
   so no capture could succeed.

And, implicitly, nothing is re-armed when the health loop is not ticking
(daemon dead, hung, or the actor stalled) or the network is down.

Not gating: `sync_backlog`, `capture_tmpfs_low`, `temperature_high`,
`throttled`. They are real but not "the station is dead", and they already
alert over ntfy. Withholding for them would duplicate those alerts 30 min
later.

Consequence: a capture-path failure produces its ntfy alert first, then the
heartbeat alert 30 min after the last check-in — an intended escalation,
since it also fires if the first notification was missed. When the
condition resolves, check-ins resume at once and the recovery notice (§4.2)
is sent if the silent alert went out.

### 4.4 Planned shutdowns

A Power → Shut down, or stopping the daemon for longer than 30 min, delivers
the heartbeat alert: the station has stopped. This is intended — it confirms
the shutdown reached the phone. A reboot or daemon restart (≈1 min) does
not. The daemon does not cancel the pending message on shutdown.

### 4.5 Dry run and disabled

- No `heartbeat` section, or `"enabled": false` → no heartbeat (default, so
  a deploy never starts publishing without an explicit opt-in).
- Dry run (notifications disabled, or no valid ntfy settings) → the
  heartbeat logs "would check in" once per interval and publishes nothing.

### 4.6 Changing or disabling the channel

The armed message lives on the ntfy server, so stopping re-arming is not
enough: it would still be delivered 30 min later as a false alarm. When the
effective settings change so that the armed message would be orphaned —
ntfy turned off, the heartbeat turned off, or a different server, topic,
token or sequence ID — the daemon first sends
`DELETE <old server>/<old topic>/<old sequence_id>` with the old token. A
failed cancel is retried every tick until it succeeds or the armed message
would have been delivered anyway. After a successful cancel, the saved last
check-in is cleared, so the new channel does not send a false "checking in
again" notice.

## 5. Configuration

### 5.1 Where

All notification settings live in the committed global `config.json`
(`~/.local/state/optic-daemon/`, the same file as the Station and
schedule), under `notifications`. There is no other notification config
file.

**One-time import of `alerts.json`.** At startup, if the committed config
has no `notifications` section and `~/.config/optic-daemon/alerts.json`
(or `OPTIC_ALERTS_CONFIG`) exists and parses, its contents (channel,
thresholds, and any `digest`/`heartbeat`) are written into `config.json`
through the same save path the UI uses (§5.3), and the daemon logs that
`alerts.json` is no longer used and can be deleted. From then on only
`config.json` is read. If the import cannot be written, the daemon uses the
parsed `alerts.json` for this run only and retries at the next start. An
invalid `alerts.json` is not imported; the reason is shown in
`/api/alerts`, as today.

`config.json`, its tmpfs mirror, and the staging copy
(`/mnt/capture/preview_config.json`) are written with the unit's
`UMask=0027` (mode 0640, owner `liam`) inside `liam`-owned directories.
None of them is transferred by `optic_sync` (its allowlist only matches
capture files).

### 5.2 Shape

```json
"notifications": {
  "enabled": true,
  "station_name": "optic",
  "ntfy": {
    "server": "https://ntfy.sh",
    "topic": "optic-<long random string>",
    "token": "tk_<optional access token>"
  },
  "digest": { "enabled": true, "send_at": "08:00" },
  "heartbeat": {
    "enabled": true,
    "interval_secs": 600,
    "alert_after_secs": 1800,
    "sequence_id": "optic-heartbeat"
  },
  "thresholds": { "capture_stalled_after_secs": 900, "...": "see docs/optic-daemon-alerts.md §7" }
}
```

`thresholds` has the same fields and defaults as before
(`docs/optic-daemon-alerts.md` §7). `alerts.json` (import only) accepts the
same `digest` and `heartbeat` objects.

| Field | Default | Validation |
| --- | --- | --- |
| `enabled` | `true` | `false` = dry run: nothing is published, alerts are logged only |
| `digest.enabled` | `true` (also when `digest` is absent) | — |
| `digest.send_at` | `"08:00"` | `HH:MM`, 24 h |
| `heartbeat.enabled` | `false` when `heartbeat` is absent, else `true` | opt-in, so a deploy never starts publishing on its own |
| `heartbeat.interval_secs` | 600 | 60–3600 |
| `heartbeat.alert_after_secs` | 1800 | 120 s–3 days (ntfy's maximum delay), sent as whole minutes (rounded up); must be at least `interval_secs` + 300 |
| `heartbeat.sequence_id` | `"optic-heartbeat"` | 1–64 of `A-Za-z0-9-_` |

The UI save rejects out-of-range values (HTTP 422). A hand-edited file with
invalid values falls back to dry run, with the reason shown in
`/api/alerts`.

### 5.3 How the section survives the config workflow

Every existing staging handler reads the current config on the server and
changes one field, so `notifications` passes through staging, commit and
discard untouched. The notifications save writes the committed
`config.json` (durable first, then the tmpfs mirror). If a staging copy
exists, the same change is applied to it, so a later commit of unrelated
staged edits cannot bring back the old settings. When nothing was staged
before the save, nothing is staged after it.

Rollback caveat: a daemon older than this change ignores `notifications`
when reading, but drops it when it re-writes a staged config, and it reads
its channel from `alerts.json` only. Keep `alerts.json` until the new build
is accepted; after a rollback, a commit made on the old build removes the
section, and the next start of the new build imports `alerts.json` again.

## 6. API and Dashboard

### 6.1 Masking in `/api/status`

`GET /api/status` returns the in-effect config to every page. Before
serializing, `notifications.ntfy.topic` and `token` are masked: the first
4 and last 4 characters with `****` between (`opti****a7f3`), or just
`****` when the secret is shorter than 16 characters, so at least 8
characters always stay hidden (a generated topic keeps 22 random characters
hidden). Topic and token validation reject `*`, so a masked value can never
be saved back as a real secret. No other endpoint returns
`AppConfig`.

### 6.2 Notification settings API

- `GET /api/notifications` → `{source: "config" | "alerts_file" | "none",
  enabled, station_name, server, topic_set, topic_hint, token_set,
  token_hint, digest, heartbeat}`, where the hints are masked as in §6.1;
  never the topic or token. `alerts_file` appears only when the one-time
  import could not be written.
- `PUT /api/notifications` with `{enabled, station_name, server, topic?,
  clear_topic?, token?, clear_token?, digest, heartbeat}`. An absent or
  empty `topic` or `token` keeps the current one; `clear_topic` /
  `clear_token` remove it (the page sends them when a field that showed a
  saved value is cleared). Removing the topic removes the channel, token
  included, and is rejected while notifications are on.
  Validates (422 with the reason), saves (§5.3), wakes the alerts actor to
  apply at once, and returns the `GET` view.
- `POST /api/notifications/test` sends
  `Optic <station>: test notification` through the saved channel and
  returns the delivery result (409 when notifications are off or not set
  up, 429 within 10 s of the previous manual send, 502 when delivery fails).
- `POST /api/notifications/digest-now` sends a digest of the last 24 h at
  once, with the same rules. It does not change the daily schedule.

The dashboard has no login (as with Reboot and every other control): anyone
on the LAN can change these settings, but cannot read the secrets back.

### 6.3 Status

`GET /api/alerts` gains `settings_source` and two objects (no topic, token
or sequence ID):

- `digest`: `enabled`, `send_at`, `timezone`, `next_due_at`,
  `last_sent_at`, `pending` (a digest waiting for delivery),
  `last_error`.
- `heartbeat`: `state` (`disabled` / `dry_run` / `ok` / `withheld` /
  `failing`), `interval_secs`, `alert_after_secs`, `last_checkin_at`,
  `next_checkin_at`, `withheld_reason`, `last_error`.

### 6.4 Dashboard

- Config page: a **Notifications** card (after Time & NTP) with the fields
  in §2; Notifications, Digest and Heartbeat are sliding On/Off switches
  on their heading lines, so the text fields line up in two columns
  (`role="switch"` buttons named by their heading, styled by
  `.toggle-switch`; the page's CSP
  `style-src 'self'` rules out inline styles, so all styling is in
  `styles.css`). A **Generate topic** button beside the Topic
  field fills a random `optic-` + 24 characters, shown once so it can be
  entered in the ntfy app. Saved secrets appear masked as the fields'
  own text (`opti****bd45`, `tk_a****wxyz`): left as is = keep, cleared =
  remove, a new value = replace; a partly edited mask is refused on the
  page. Save is enabled only while the form differs from what was last
  loaded or saved. The test buttons use the saved settings and ask for a
  save first when the form has unsaved changes. The card's heading shows a
  hint only when something needs attention ("Not set up yet").
  Code: `src/web/notifications.js`.
- `src/web/footer.js` appends a `Heartbeat: …` and a `Digest: …` line to the
  Alerts pill's hover text. The pill's colour and text are unchanged.

## 7. Limitations

- ntfy.sh is a single point: if it is down, neither alerts nor the
  heartbeat reach the phone. An ntfy.sh outage while the Pi is healthy does
  not alarm either (the pending message is simply not delivered or
  replaced).
- ntfy.sh free-tier quotas are not documented precisely; whether replaced
  scheduled messages count toward a daily message limit is unknown. The
  10-minute interval was chosen to leave headroom; watch for HTTP 429 in
  `heartbeat.last_error`.
- Replacing a scheduled message by sequence ID must be supported by the
  ntfy server. Verified on ntfy.sh on 2026-09-22 (worklog); a self-hosted
  server needs a recent ntfy version.
- "Expected" uses the **current** committed schedule for the whole window;
  a rule edit during the day makes the earlier part of the count approximate.
- Byte/temperature samples cover only time the daemon was running; the
  digest says when its samples cover less than the window.
- Uses the wall clock; a large NTP step can distort one window or one
  heartbeat interval. The daemon starts after `time-sync.target`, so this is
  rare.
- `history.db` holds the capture rows. The digest's window query reads only
  a few small columns, so a high-frequency schedule (thousands of rows a
  day) stays cheap.
