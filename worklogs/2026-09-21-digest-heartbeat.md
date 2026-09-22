# Dated Worklog: 2026-09-21 - Daily Digest, External Heartbeat, Notification Settings in config.json

Status: **deployed to the Pi (branch build, 2026-09-22, user-approved) and
hardware-validated for: import, masking, test notification, on-demand
digest, heartbeat arm / replace / cancel on the real ntfy.sh. Later UI
polish (switches, topic row, spacing) deployed with `--assets`.
Not observed yet: a scheduled 08:00 digest on the Pi, a delivered silent
alert, the "checking in again" notice. Not user-accepted.**

Branch: `digest-heartbeat` (Orca worktree). Design:
`docs/optic-daemon-digest-heartbeat.md`. Extends
`docs/optic-daemon-alerts.md` (`worklogs/2026-09-20-health-alerts.md`).

## Objective

1. A daily ntfy digest summarising the last 24 h.
2. An external dead-man's switch that alerts when the Pi stops checking in.
3. (Added by the user mid-task) Notification settings editable from the
   Config page, stored in the single global `config.json` (the user does
   not want separate config files), with secrets masked in `/api/status`.

## Facts Established Before Planning

- Read: `README.md`, `docs/optic-daemon-alerts.md`,
  `docs/optic-daemon-capture-log.md`, `docs/optic-daemon-system-service.md`,
  `docs/optic-daemon-system-events.md`, worklogs for health alerts, the
  system-service migration and the system events log; `src/optic_alerts.rs`,
  `src/main.rs`, and the relevant parts of `optic_capture_log.rs`,
  `optic_sync.rs`, `system_status.rs`, `optic_events.rs`,
  `optic_scheduler.rs`, `web.rs`, `footer.js`.
- Pi (read-only SSH, 2026-09-21): `~/.config/optic-daemon/alerts.json`
  exists (0600, 83 bytes, not printed); `/api/alerts` reports channel
  `ntfy`; station and system timezone `America/Vancouver`; scheduler
  `Paused`; service active.
- The scheduler run state has no history (only the current value), and
  `SyncStatus.transferred_bytes` is an in-memory counter since daemon
  start, so the digest must sample them itself.
- The unit's `ProtectHome=read-only` makes `~/.config` unwritable by the
  daemon; `~/.local/state/optic-daemon` (where `config.json` lives) is in
  `ReadWritePaths`.
- `GET /api/status` serializes the whole in-effect `AppConfig`
  (`src/web.rs` `StatusResponse.config`) to every page. No other endpoint
  returns it. `optic_sync`'s allowlist excludes `config.json` and
  `preview_config.json`. Every staging handler is a server-side
  read-modify-write of one field.
- ntfy docs (docs.ntfy.sh/publish, fetched 2026-09-21): scheduled messages
  (10 s–3 days) published with the same sequence ID replace the pending
  one; `DELETE /<topic>/<sequence_id>` cancels; a "Dead man's switch"
  example is documented. The ntfy.sh free-tier daily message limit is not
  documented on the pages checked.
- Baseline: `cargo test --locked --all-targets` → **170 passed**.

## Decisions (confirmed by the user, 2026-09-21)

1. Heartbeat: ntfy dead-man's switch (after I corrected my first framing
   that ntfy could not do it).
2. Re-arm every 10 min; alert 30 min after the last check-in.
3. Withheld while `capture_stalled`/`capture_overdue`/`capture_failing` is
   firing or recovering, or the camera was not detected at startup.
4. Expected frames counted only while Running, run-state changes persisted.
5. Digest at 08:00 station time.
6. Digest content: standard set plus alerts fired and system events.
7. Heartbeat/digest status in the Alerts pill tooltip.
8. Settings in `config.json` → `notifications` (user chose this over a
   sibling file after the leak risk was explained), with `/api/status`
   masking; `alerts.json` imported once, then unused (one config file).
9. Secrets write-only in the UI; UI edits channel, digest, heartbeat and has
   a test button (thresholds not in the UI); changes apply immediately.
   (The user answered "above" to these; recorded as the recommended options,
   as in the earlier round, and stated to the user.)
10. Mid-implementation user requests: no "Saved in config.json" label (the
    card shows a hint only when not set up); a test button plus, my
    addition, "Send digest now"; secrets masked as first 4 + `****` + last 4
    (fully masked under 16 characters; `*` rejected in tokens so a mask can
    never be saved back).

## Files Outside This Track's Owned List (merge-conflict risk)

Required by decision 8/9; kept additive:

- `src/camera.rs`: one new `AppConfig.notifications` field.
- `src/web.rs`: mask in `status()`; `/api/notifications*` routes and
  handlers on the existing `alerts_router` (`AppState` unchanged).
- `src/web/config.html`: one Notifications card and one `<script>` tag.
- `src/web/notifications.js`: new file (keeps `config.js` untouched).
- `src/optic_capture_log.rs`: one additive read-only window query.
- `src/web/styles.css`: `url` and `password` added to the shared input
  selector (line 239), so the Server and Token fields match the others.
  The exposure-ramping track appends to the end of this file; no overlap
  expected.
- `biome.json`: `src/web/notifications.js` added to the CI lint list.

`src/camera.rs` is owned by the exposure-ramping track: my change is one
field at the end of `AppConfig` and its `Default`. Expect a trivial merge
conflict if that track also edits `AppConfig`.

## Acceptance Criteria

Digest
1. `due`/window: 08:00 station time, DST-correct (23/25 h windows), catch-up
   within 12 h, skip later, first start does not send a partial digest.
2. Expected frames = schedule replayed over Running segments only; paused
   time reported; downtime between identical run states counts as Running.
3. Content lines per design §3.2, correct for fake data; "n/a" when a signal
   is missing; covers-less-than-window note when samples are short.
4. Delivery retried until 12 h after due; exactly one digest per window.

Heartbeat
5. Re-arms at most once per interval while healthy; failed publish retried
   next tick; withheld exactly per decision 3; no publish in dry run.
6. Publish request: `POST <server>/<topic>/<seq>`, `X-Delay: <m>m`, ASCII
   headers, token only on curl stdin.
7. Recovery notice when a check-in succeeds more than `alert_after` after
   the previous one (persisted across restarts).
8. Channel change / heartbeat off / ntfy off cancels the armed message with
   the *old* credentials, retried until success or expiry.

Settings
9. `notifications` in `config.json` survives schedule/save-DNG/stream
   staging, commit and discard; a save updates committed config and any
   staging copy, and keeps "not staged" when nothing was staged.
10. `/api/status` never contains the topic or token; masked values are
    rejected by validation.
11. `GET /api/notifications` never returns the topic or token; `PUT`
    validates (422), keeps secrets when fields are empty, can clear the
    token, keeps thresholds, applies without restart; test endpoint
    reports delivery result, 409 without a channel, rate-limited.
12. One-time import of `alerts.json` when `notifications` is absent.
13. No new crate, `Cargo.lock` and `Cargo.toml` version unchanged, no
    systemd unit change.

## Test Plan (written before implementation)

Environment: macOS dev host; `cargo fmt --all -- --check`,
`cargo clippy --locked --all-targets -- -D warnings`,
`cargo test --locked --all-targets`, `node --check` on changed JS,
`npx @biomejs/biome ci` if available as in CI.

Unit tests (fake clock and fake data, no network):

- `optic_digest`:
  - `due_at`: before/after 08:00; America/Vancouver; DST spring-forward and
    fall-back windows are 23 h / 25 h; invalid tz → UTC.
  - send decision: sends once per due; catch-up within 12 h; skip after;
    fresh state marks current due as sent.
  - run segments: changes inside/before window; paused whole day → 0
    expected; downtime with the same state counts as running.
  - expected count with a 5-minute interval rule over segments.
  - longest running gap excludes paused time; < 2 frames → none.
  - hourly buckets: temp min/max, tmpfs min, transfer deltas incl. counter
    reset; pruning to 26 h; alerts-fired pruning.
  - render: example-like title/body for fake inputs; paused title; missing
    signals → n/a.
  - state JSON round trip; corrupt file → fresh state.
- `optic_heartbeat`:
  - due/interval, withheld reasons, failure retry, dry run.
  - recovery notice threshold.
  - curl config for publish and delete: URL with seq id, `X-Delay` minutes
    rounded up, ASCII-sanitised title, bearer only when set.
  - cancel decision on channel change / disable / seq change; none when
    unchanged or nothing armed.
- `optic_alerts` settings:
  - `NotificationSettings` defaults, validation ranges, masked values
    rejected, redaction output.
  - `alerts.json` with digest/heartbeat parses; import conversion keeps
    thresholds.
  - fake-curl process test for a scheduled publish (token on stdin only).
- `web`:
  - `/api/status` JSON has masked topic/token (never the real values).
  - notifications save: committed + staged updated; unstaged stays
    unstaged; `PUT` keeps secrets on empty fields, clears the token, keeps
    thresholds, rejects invalid values.
  - staging handlers keep `notifications`.
- Existing 170 tests still pass.

Local end-to-end (real debug binary, real `curl`, local Python HTTP
listener standing in for ntfy on 127.0.0.1; nothing sent off the machine;
temp dirs in the session scratchpad):

- One-time import from a temp `alerts.json`; `config.json` gains
  `notifications`; `/api/status` masked.
- `PUT` a heartbeat with a short interval; listener sees
  `POST /<topic>/<seq>` with `X-Delay`; change the topic → listener sees
  `DELETE /<old-topic>/<seq>` then a publish on the new topic.
- Test endpoint → listener receives the test message.
- Digest with `send_at` set to just after startup → listener receives it.
- Config page card in a browser (Chrome tool) if available.

On-Pi (requires user approval; shared Pi; not done without it): deploy,
confirm import, a real ntfy test notification, a real digest, and one real
heartbeat cycle (arm, stop re-arming by switching topic → cancel observed,
or let an alert fire on a throwaway topic).

## Implementation Summary

- `src/optic_digest.rs` (new): pure digest scheduling (`due_at`,
  `previous_due`/`next_due`, `decide` with 12 h catch-up), persisted
  `DigestState` (run-state changes, hourly buckets, fired alerts, heartbeat
  check-ins), window arithmetic (running segments, expected shots over
  `[start, end)`, longest running gap), and `build()` rendering.
- `src/optic_heartbeat.rs` (new): gate, due/armed/silent-period logic,
  cancel decision, curl configs for arming (`POST /<topic>/<seq>`,
  `X-Delay`) and cancelling (`DELETE`), message texts.
- `src/optic_alerts.rs`: `NotificationSettings` (+ `DigestSettings`,
  `HeartbeatSettings`) replaces the alerts.json struct; lenient `resolve`
  for disk, strict `validate_for_save` for the UI; settings read from
  `config.json` every tick (hot reload) with a one-time import of
  alerts.json; masking (`redact_notifications`, `mask_secret`); view,
  update merge, save (committed + staging copy); generic `Notifier::publish`;
  actor rewritten as a struct with heartbeat, digest, persistence, test and
  digest-now commands; `/api/alerts` gains `settings_source`, `digest`,
  `heartbeat`.
- `src/optic_capture_log.rs`: `window_rows()` read-only query.
- `src/camera.rs`: `AppConfig.notifications: Option<serde_json::Value>`.
- `src/web.rs`: `/api/status` masking; `GET/PUT /api/notifications`,
  `POST /api/notifications/test`, `POST /api/notifications/digest-now` on the
  alerts router.
- `src/main.rs`: new modules; extra `AlertSources` fields.
- UI: Notifications card (`config.html`, new `notifications.js`), Alerts pill
  tooltip lines (`footer.js`), input style (`styles.css`), `biome.json`.
- Docs: new `docs/optic-daemon-digest-heartbeat.md`; update notes in
  `docs/optic-daemon-alerts.md`.
- Not changed: `Cargo.toml` (no version bump), `Cargo.lock`, systemd unit,
  `src/web/app.js`, `index.html`, `native_camera.rs`, `optic_scheduler.rs`,
  `scheduler.*`, `config.js`.

## Validation (2026-09-21/22 UTC, macOS dev host, rustc 1.98.1)

| Check | Command | Result |
| --- | --- | --- |
| Baseline | `cargo test --locked --all-targets` | 170 passed |
| Format | `cargo fmt --all -- --check` | clean |
| Lint | `cargo clippy --locked --all-targets -- -D warnings` | clean (fixed first: `precedence` in a test, two `type_complexity`) |
| Tests | `cargo test --locked --all-targets` | **206 passed**, 0 failed (36 new) |
| Web lint | `npx @biomejs/biome@2.5.14 ci` (CI's pinned version) | clean after `biome format --write` on the two JS files |
| JS syntax | `node --check` on `notifications.js`, `footer.js` | OK |
| Dependencies | `git diff --stat Cargo.toml Cargo.lock` | no changes |

New tests cover every unit item of the test plan: DST 23/25 h windows and a
send time inside the spring-forward gap, catch-up/skip/initialize, running
segments with unknown time, expected shots at window boundaries, longest
running gap, bucket counter resets and bounds, pruning, state round trip
and corrupt file, full digest rendering (running and paused days), gate,
due/armed/silent period, cancel decision, arm/cancel curl configs, sequence
ID validation, settings parsing/clamping, strict save validation (incl.
masked values), masking at any depth and the 4+4 rule, `AppConfig` staging
round trip and malformed section, merge rules, save into committed +
staged/unstaged/missing preview and refusal on non-JSON config, one-time
import, effective-settings precedence, view without secrets, capture-path
condition split, fake-curl heartbeat publish (token on stdin only),
`window_rows`.

Failures in my own first test run (all test mistakes except one): three
tests passed `(day, hour, minute)` as if `(hour, minute)`; a hand-computed
expected size; and one real bug — `expected_shots` missed a shot exactly at
the window start because `forecast` covers `(from, from + horizon]`. Fixed
in code by starting 1 s early.

Local end-to-end (real debug binary, real `curl`, a Python listener on
127.0.0.1:18099 standing in for ntfy; temp dirs in the session scratchpad;
nothing sent off the machine):

1. Start with a temp alerts.json (http listener, token, heartbeat 60/360 s,
   threshold override): log `imported the legacy alerts config into
   config.json`; `config.json` gained `notifications` next to the existing
   `save_dng`/`schedule`; `notification settings applied source="config"
   channel="ntfy"`.
2. `/api/status` → `"ntfy": {"topic": "opti****a7f3", "token": "****"}`;
   topic/token matches in `/api/status`, `/api/notifications`,
   `/api/alerts`: **0**; in the daemon logs: **0**.
3. Heartbeat: `state: withheld`, `withheld_reason: camera not detected at
   startup` (no camera on the Mac) — gating observed; arming not exercised.
4. `POST /api/notifications/test` → 200; listener got `POST /` with
   `Authorization: Bearer …`, title `Optic localtest: test notification`,
   priority 3. Immediate repeat → 429.
5. `POST /api/notifications/digest-now` → 200; listener got the full
   13-line digest (priority 2, `bar_chart`), incl. the partial-coverage note.
6. `PUT` validation: alert delay < interval + 5 min → 422; masked topic sent
   back → 422.
7. Staged an unrelated edit (`save_dng` via `/api/config/save-dng`), then
   `PUT` with empty topic/token: committed config kept topic, token and
   thresholds, `save_dng` unchanged; staging copy kept `save_dng: true` and
   got identical `notifications`; `config_staged: true`; after
   `/api/config/commit` both edits present.
8. Scheduled digest with `send_at` 06:42 UTC: delivered at 06:42:23 (first
   tick after due); `next_due_at` moved to the next day.
9. Restart: `config.json already has notification settings; the legacy
   alerts config is not used`; no second digest; `digest_state.json` held
   `last_digest_due`, one run change, one withheld record.
10. `PUT` with notifications off: `channel: dry_run`, heartbeat `disabled`;
    test → 409 `notifications are turned off`; topic kept in config.
11. Config page in Chrome: card renders with saved values and hints, status
    lines, no console errors. Found and fixed: Server/Token inputs unstyled
    (styles.css selector); removed the "Saved in config.json" label (user).

## On-Pi Validation (2026-09-22 UTC, approved by the user)

| Step | Observed |
| --- | --- |
| Pre-check | No build running (the only `pgrep` hit was my own SSH shell); Pi on 0.1.31. Backup `~/.local/state/optic-daemon/config.json.bak-before-digest-heartbeat` (993 bytes, 0640). |
| Deploy | `./scripts/build-deploy-optic-daemon.sh`, exit 0: fmt/clippy/**209 tests** on the Pi (3 Linux-only), release build, `SUCCESS: optic-daemon 0.1.31 is active`. |
| Import | 06:55:35 `camera detected`; `imported the legacy alerts config into config.json`; `notification settings applied source="config" channel="ntfy" digest=true heartbeat=false`. `config.json` 1703 bytes, 0640; `digest_state.json` created, 0640. |
| Masking | `/api/status` → `"topic": "opti****bd45"`; real topic in `/api/status`, `/api/notifications`, `/api/alerts`: **0** matches. |
| Digest status | `timezone: America/Vancouver`, `next_due_at: 2026-09-22T15:00:00Z` (08:00 PDT). |
| Test notification | `POST /api/notifications/test` → 200; journal `notification delivered via ntfy title=Optic optic: test notification`. |
| Digest now | → 200; `delivered … Optic optic: daily digest 0/0 frames` (scheduler paused). |
| Heartbeat on (10/30 min) | `PUT` → 200; `state: ok`, check-in 06:56:30. ntfy.sh `GET /<topic>/json?poll=1&sched=1` (topic not printed): one message `sequence_id: optic-heartbeat`, priority 5, title `Optic optic: no check-in for 30m`, due in 1796 s. |
| Replacement | Next check-in 07:06:35; ntfy.sh then held **exactly one** heartbeat message, due in 1547 s at 07:10:48 (= 07:36:35, check-in + 30 min). |
| Cancel | `PUT` heartbeat off at 07:10:58 → `cancelling the armed heartbeat on the previous channel`, then `cancelled` 0.27 s later; ntfy.sh scheduled heartbeat messages: **0**. Heartbeat left **off** (the pre-test state); digest on; station name `optic`. |

Failure during this: my background wait loop used `[ "$a" \> "$b" ]`, which
zsh rejects (`condition expected: >`), so it spun instead of waiting; stopped
it and ran the check directly. Logged in `AGENTS.md` per the repo rules.

## UI Revisions After Deploy (user requests, 2026-09-22)

- **Generate topic** moved into the Topic cell, beside a narrower input
  (`.input-with-button`). First attempt used inline styles, which the
  dashboard's CSP (`style-src 'self'`, `src/web.rs:41`) silently blocks;
  found via computed styles in Chrome and replaced with CSS classes. The
  pre-existing inline style at `scheduler.html:85` is ignored for the same
  reason (not changed; outside this track).
- Notifications / Digest / Heartbeat On/Off: selects → sliding switches
  (`.toggle-switch`, `role="switch"`, `aria-checked`), per the user's
  reference image.
- Help text and `h3` spacing inside the card (`.controls-help` has a -4px
  top margin meant for under-heading use); scoped to `.notifications-card`.
- Verified locally in Chrome: same row (input 431 px + button 120 px),
  switch toggles and marks the form unsaved, help margin 10 px, no console
  errors; `biome ci` clean.
- These are asset-only changes (`config.html`, `notifications.js`,
  `styles.css`). Deployed with `./scripts/build-deploy-optic-daemon.sh
  --assets` (user-approved): `SUCCESS: static web assets deployed and
  verified`, no daemon restart.

### Revision: switches on heading lines (user request, 2026-09-22)

- Notifications / Daily digest / Heartbeat switches moved onto their
  heading lines (`.toggle-heading`, `aria-labelledby` = heading); Saved
  token selector moved beside the token input. Grids now hold only text
  fields: Server | Station name; Topic + Generate | Token + Keep/Remove;
  Send at; Check in every | Alert after silence.
- Verified locally in Chrome: left-column fields all at x=704, right
  column at x=1280, paired rows share a top; switches centred on headings;
  a switch click toggles and marks the form unsaved; `biome ci` clean.
- Committed to PR #15 and deployed to the Pi with `--assets` (user-approved).

### Revision: masked secrets as field values; no Keep/Remove control (user request, 2026-09-22)

- First tried a Remove token button (shown only with a saved token); the
  user then asked for a simpler rule, now implemented: topic and token
  fields show the saved mask as their value (`opti****a7f3`,
  `tk_a****z123`); unchanged = keep, cleared = remove, new value = replace;
  a partly edited mask (still contains `*`) is refused on the page.
  Token field changed from `password` to `text` so the mask is readable.
- API: `clear_topic` added to `PUT /api/notifications` (next to
  `clear_token`); clearing the topic removes the channel and is rejected
  while notifications are on. Unit test extended. **Daemon and page must
  deploy together**: the running daemon rejects unknown fields, so this
  page against an older daemon cannot save.
- Save-button bug (user report): Save stayed enabled after toggling a
  switch back. Replaced the "dirty" flag with a comparison against the form
  state last loaded or saved.
- Checks: `cargo fmt --check`, clippy `-D warnings`, **206 tests** pass,
  `biome ci` clean. Local Chrome run (after fixing my own stale local daemon
  on port 18080, PID 89941, which caused a spurious 422): toggle on → Save
  enabled, toggle back → disabled; edit + undo → disabled; keep (masks
  untouched, station changed) → saved, both secrets kept; partial token edit
  → "type the full new token, or clear the field to remove it", nothing
  saved; token cleared → removed, topic kept; topic cleared with
  notifications on → "an ntfy topic is required to turn notifications on";
  token replaced → new mask `tk_z****a987`; topic cleared with notifications
  off → channel removed.
- Deployed (user-approved), with the heartbeat paused so the build could
  not trigger a false alarm: read-then-save turned only the heartbeat off
  at 07:33:32 (pending message cancelled 07:33:32.28; the station name was
  already `optic` again — changed by the user since 07:16 — and was
  preserved). Full deploy: 209 tests on the Pi, `SUCCESS`. Heartbeat back
  on (read-then-save, only that field): check-in 07:36:39, ntfy.sh holds
  one heartbeat message due in 1794 s. Pi serves the new page (no
  Keep/Remove control, `secretField` present). Committed to PR #15.

## Incident: my cancel test overwrote the user's own settings

The `--assets` deploy printed the journal, which showed the user had used
the Config page on the Pi at 06:58–07:01 UTC (test, digest, two saves with
station name `optic1` and heartbeat on). My 07:10:58 cancel-test `PUT`
(built from my own values, without re-reading the settings) saved station
`optic` and heartbeat off over it. The topic was unaffected (empty topic =
keep). Told the user; at their choice, restored at 07:16 UTC: station
`optic1`, heartbeat on (600 / 1800 s), digest 08:00. Verified: check-in
07:16:21, ntfy.sh holds one `Optic optic1: no check-in for 30m` message due
in 1795 s. Lesson: on the shared Pi, read the current settings immediately
before any test save and change only the field under test.

Current Pi state: heartbeat **on**; a Shut down or a daemon stop longer
than 30 min delivers the heartbeat alert.

## Remaining Limitations / Risks / Follow-up

- **Not yet observed on the Pi**: a scheduled 08:00 digest, a delivered
  silent alert, the "checking in again" notice, and a check-in withheld by a
  real capture-path alert.
- ntfy.sh free-tier quota for ~144 re-arms/day is undocumented; watch
  `heartbeat.last_error` for 429.
- Rollback to an older build drops `notifications` on its next commit and
  goes back to alerts.json; keep alerts.json until this is accepted.
- The dashboard has no login: anyone on the LAN can change notification
  settings (cannot read the secrets back).
- Merge-time doc follow-ups outside this track (not edited): add
  `/api/notifications*` to `docs/optic-daemon.md` §4 and point `setup.md` at
  the Config page instead of hand-editing alerts.json.

## User Verification Steps (after approving a deploy)

1. Deploy (`./scripts/build-deploy-optic-daemon.sh`, needs approval; the Pi
   is shared). Log should show the import of alerts.json.
2. Config page → Notifications: hints show your topic as `xxxx****xxxx`;
   press **Send test notification** → phone gets it.
3. **Send digest now** → phone gets a digest.
4. Turn the heartbeat on (10 / 30 min), save; the status line shows
   "checking in". To see a real silent alert, temporarily switch to a
   throwaway topic subscribed on the phone, then stop the daemon for 30 min
   (or let me run that test with your approval).
5. Next morning at 08:00 (Vancouver): the daily digest arrives.
