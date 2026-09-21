# Dated Worklog: 2026-09-20 - Health Alerts for Unattended Operation

Status: **implemented, tested locally, deployed to the Pi, and live ntfy
delivery from the Pi observed (ntfy.sh accepted the request, 2026-09-21);
phone receipt confirmed by the user ("my phone see a FIRING capture tmpfs
low"); not soak-tested; not user-accepted.**

## Objective

Nothing notifies the operator when the station stops doing its job. Beszel
watches host metrics only. Add an in-daemon health monitor that detects
real failure conditions and notifies the operator, without spamming.

Design: `docs/optic-daemon-alerts.md`.

## Signal Survey (2026-09-20, from source)

- Scheduler: `SchedulerStatus` has `run_state`, `next_capture_at`,
  `next_capture_rules`, and only the single latest `last_capture` — no
  history or failure counter. `optic_scheduler::occurrences()` is pure and
  public, so "shots that should have fired" can be replayed over a past
  window from the committed config cache.
- Capture outcomes: `CaptureLog::query(CaptureQueryFilter { source:
  "scheduler", .. })` returns entries newest first, with `success` and
  `completed_at_unix_ms`. `capture_log` is `Option` (DB may be unavailable).
- Sync: `SyncStatus` has `enabled`, `paused`, `connectivity`,
  `queued_files/bytes`, `transferred_files`, `last_error`, `backoff_secs`.
  No last-successful-transfer timestamp.
- System: `SystemStatusReader::snapshot()` gives the `capture` disk's
  total/available bytes and `cpu_temp_celsius` (vcgencmd, Linux only).
  Throttling/under-voltage (`vcgencmd get_throttled`) is not read anywhere.
- Beszel: records temps, root disk, and `/mnt/capture`; no alert
  configuration is recorded in the repo. Its Hub runs on the same iMac as
  the sync receiver, so it cannot alert during an iMac outage — the main
  failure this feature must catch.
- No HTTP client crate in `Cargo.toml`; the unit allows AF_INET/AF_INET6
  and `ProtectHome=read-only` (a `~/.config` file is readable).
- `AGENTS.md` (imported by `CLAUDE.md`) does not exist in this worktree and
  is not tracked in git; not created by this track.

## Decisions (confirmed by the user, 2026-09-20)

1. Channel: ntfy (default `https://ntfy.sh`), sent via system `curl`, no
   new crate.
2. Credential: `~/.config/optic-daemon/alerts.json` (0600, outside git;
   `OPTIC_ALERTS_CONFIG` override). No systemd unit change.
3. Thresholds: "Balanced" preset as defaults (see design doc §5), all
   overridable.
4. Dashboard: `GET /api/alerts` **plus a footer badge**. The badge requires
   editing `src/web/footer.js`, which is **outside this track's owned
   files** — accepted by the user explicitly; see "Out-of-Scope File" below.

## Acceptance Criteria

1. Each of the seven conditions (`capture_stalled`, `capture_overdue`,
   `capture_failing`, `sync_backlog`, `capture_tmpfs_low`,
   `temperature_high`, `throttled`) activates and clears exactly per design
   doc §5, including hysteresis and "paused/disabled = no alert".
2. A condition produces exactly one FIRING notification on onset (after
   `fire_after`), a STILL FIRING reminder every `repeat_every`, and exactly
   one RESOLVED after `recover_after` of continuous clear; flapping within
   `recover_after` produces no extra notifications.
3. Unknown signals neither fire nor resolve.
4. Missing/invalid/disabled config → dry-run (log only), monitor still runs,
   `/api/alerts` reports why. Unknown JSON fields are rejected.
5. The ntfy token never appears in argv, logs, `Debug` output, or
   `/api/alerts`.
6. Delivery failure does not lose notifications (bounded outbox, in-order
   retry) and never affects capture/sync.
7. `GET /api/alerts` is read-only and serves the state; the footer badge
   renders OK / N alerts / dry-run and hides if the endpoint is absent.
8. No `Cargo.toml` version bump, no new dependencies, `Cargo.lock`
   unchanged.

## Test Plan (written before implementation)

Environment: macOS dev host (`cargo test --locked --all-targets`,
`cargo clippy --locked --all-targets -- -D warnings`, `cargo fmt --check`).
On-Pi validation needs explicit user approval (shared Pi) and is listed
separately.

Automated unit tests in `src/optic_alerts.rs` (fake clock = explicit `now`):

- Debounce engine:
  - fire_after = 0 fires on first active tick; fire_after = 5 min stays
    Pending until held 5 min; Pending → inactive returns to Clear silently.
  - Firing → inactive → Recovering; RESOLVED only after recover_after of
    continuous clear; re-activation while Recovering returns to Firing with
    no notification (flap suppression).
  - Repeat: exactly one STILL FIRING per repeat_every while Firing (and
    while Recovering); none earlier.
  - Unknown (`None`) input changes nothing in any state.
- Conditions (pure evaluation over an `Observation`):
  - `capture_stalled`: fires when a due shot is ≥ 15 min old with no later
    success; not when last success is recent; not before 15 min; not when
    paused; running_since (monitor start / resume) excludes earlier shots;
    uses a real `ScheduleConfig` interval rule through `occurrences()`.
  - `capture_overdue`: fires at > 5 min past `next_capture_at`, not at 4
    min, not when paused.
  - `capture_failing`: 3 consecutive failures fire; 2 do not; newest success
    clears; paused suppresses.
  - `sync_backlog`: backlog clock starts at first non-empty queue; 30 min
    without progress fires; `transferred_files` increase restarts clock;
    queue empty clears; disabled/paused suppress.
  - `capture_tmpfs_low`: < 25 % fires; 30 % stays active if already active
    (hysteresis) but not if clear; > 35 % clears; missing disk → unknown.
  - `temperature_high`: ≥ 80 fires (after sustained window), 77 stays
    active, < 75 clears, `None` → unknown.
  - `throttled`: parse `throttled=0x50005` etc.; current bits 0–3 → active;
    only historical bits (16–19) → inactive; unparsable → unknown.
- Capture-outcome fallback tracker: records each new `last_capture.at` once,
  counts consecutive failures, bounded length.
- Config: full file parses; minimal `{"ntfy":{"topic":"x"}}` gets defaults;
  unknown field rejected; missing file → dry-run with reason; `enabled:
  false` → dry-run; permissive file mode → warning; `Debug` redacts token.
- ntfy request: generated curl config contains URL, JSON body with topic /
  title / priority / tags, bearer header only when a token is set, and
  escapes quotes/backslashes/newlines; http server + token → warning.
- Notifier process path (Unix): a fake `curl` script that captures stdin
  and exits 0 → delivered, captured config contains token only on stdin
  (not argv); fake script exiting non-zero → error string, outbox retains
  the notification and retries in order on the next flush.
- Outbox: bounded at 32 (oldest dropped); reminder not queued when one for
  the same condition is pending.
- Status snapshot: serializes without topic/token.
- Existing 109 tests still pass.

Manual local check: `cargo run` on macOS (no camera), `curl
localhost:8000/api/alerts` returns dry-run status; footer badge renders.

On-Pi (requires user approval before deploy/restart; not done by this
track without it): create `alerts.json` with a real topic, restart, confirm
`/api/alerts` channel = `ntfy`; trigger one real end-to-end notification
(e.g. temporarily pause sync delivery by stopping the receiver, or a
test-only low threshold) and observe FIRING then RESOLVED on the phone.

## Implementation Summary

- `src/optic_alerts.rs` (new, ~2,400 lines incl. tests):
  - Pure half: `Thresholds` (Balanced defaults, `deny_unknown_fields`,
    durations clamped to 30 days), `Debouncer` state machine
    (Clear/Pending/Firing/Recovering) with an explicit `now`, `Monitor`
    (condition evaluation + run-state/backlog tracking), `OutcomeTracker`
    (fallback when the history DB is unavailable), bounded `Outbox`, config
    parsing/validation, curl-config builder.
  - Actor: `AlertsHandle::spawn` ticks every 30 s, gathers signals from
    `SchedulerHandle`, `DataSyncManager`, `CaptureLog`, `SystemStatusReader`,
    the committed-config tmpfs cache, and `vcgencmd get_throttled` (Linux
    only), then delivers via `Notifier::{DryRun, Ntfy}`. Status is published on
    a `watch` channel.
  - `capture_stalled` replays the committed schedule with the existing public
    `optic_scheduler::forecast()` over the window since the later of monitor
    start / resume / last success (capped at 24 h).
- `src/main.rs`: `mod optic_alerts;`, spawn after the scheduler, merge
  `web::alerts_router(alerts)` into the served router. No other changes.
- `src/web.rs`: `alerts_router()` + `alerts_status` handler for
  `GET /api/alerts`, as a separate router so `AppState` is unchanged.
- `src/web/footer.js`: alerts pill (see Out-of-Scope File below).
- `docs/optic-daemon-alerts.md`: new design doc.
- Not changed: `Cargo.toml` (no version bump, no new dependency),
  `Cargo.lock`, systemd unit, `optic_scheduler.rs`, `optic_sync.rs`,
  `system_status.rs`, `optic_capture_log.rs`.

## Validation (2026-09-20/21 UTC, macOS dev host, rustc 1.98.1)

| Check | Command | Result |
| --- | --- | --- |
| Baseline before changes | `cargo test --locked --all-targets` | 109 passed |
| Format | `cargo fmt --all -- --check` | clean |
| Lint | `cargo clippy --locked --all-targets -- -D warnings` | clean (one `collapsible_if` found and fixed first) |
| Tests | `cargo test --locked --all-targets` | **149 passed** (109 existing + 40 new `optic_alerts` tests), 0 failed |
| Footer JS syntax | `node --check src/web/footer.js` | OK |
| Dependencies | `git diff --stat Cargo.toml Cargo.lock` | no changes |

The 40 new tests cover every item in the test plan above, including the fake
`curl` process tests (token only on stdin, never argv; failure keeps the
outbox; in-order retry) and a regression test that `u64::MAX` thresholds
don't panic.

Local end-to-end run (real debug binary, real `curl`, a local Python HTTP
listener standing in for ntfy on `127.0.0.1:18099`; nothing sent off the
machine; temp capture/state/cache dirs in the session scratchpad):

1. No config file → `/api/alerts` returned `channel: "dry_run"` with
   `config_error: "alerts config file not found; …"`; startup log line
   `health alerts running in dry-run mode`. All conditions `clear`
   (scheduler paused).
2. `alerts.json` (mode 0600, http server + token, tmpfs threshold forced to
   100 % to trigger a condition) with the listener **down** →
   `capture_tmpfs_low` went `firing`, delivery failed with
   `curl: (7) Failed to connect`, `outbox_len: 1`, error visible in
   `/api/alerts`; plain-http-with-token warning reported.
3. Listener started while the daemon kept running → on the next 30 s tick
   the queued notification was delivered: listener received `POST /` with
   `Authorization: Bearer tk_localtest`, `Content-Type: application/json`,
   body `{"topic":"optic-local-e2e","title":"Optic localtest: FIRING capture
   tmpfs low","priority":4,"tags":["warning"],"message":"Capture tmpfs has …
   free … (63%).\nActive since … UTC (0s)."}`; `/api/alerts` then showed
   `outbox_len: 0`, `last_delivery_error: null`.
4. `grep` for the topic and token in `/api/alerts` output and the daemon log:
   0 matches.

Not verified: footer pill rendering in a browser (JS only syntax-checked);
anything on the Pi (`vcgencmd get_throttled` under the service sandbox,
outbound HTTPS to ntfy.sh, `/mnt/capture` disk figures, real scheduler/sync
behavior, a real phone notification).

## Failures Encountered

- Clippy `collapsible_if` in `Outbox::push` → rewritten as a let-chain
  (rustc 1.98.1 on both Mac and Pi, edition 2024).
- My own first temperature test expected 79 °C to clear a Pending
  condition; with hysteresis the raw state stays active until < 75 °C,
  which matches the design. Corrected the test, not the code.
- Self-review before compiling: threshold durations could overflow
  `DateTime - Duration` and panic (release is `panic = "abort"`, which would
  stop the whole daemon) → clamped to 30 days, with a warning and a
  regression test.
- First end-to-end attempt raced the listener startup (first tick fires
  immediately); kept as evidence of the failure/retry path (steps 2–3).

## Remaining Limitations / Risks / Follow-up

- **Not a dead-man's switch**: a dead daemon, Pi, or network sends nothing.
  An external heartbeat monitor is a separate follow-up.
- Requires outbound HTTPS from the Pi to ntfy.sh — **not verified**.
- `vcgencmd get_throttled` under the systemd sandbox is assumed to work like
  the already-working `measure_temp`; not verified.
- Wall-clock based: a large NTP step can distort one window;
  `capture_overdue` could false-fire if the clock jumps forward by > 5 min
  while the scheduler sleeps (the scheduler's own sleep is monotonic).
- State and outbox are in memory: a restart can re-fire one still-present
  condition and loses undelivered notifications.
- A paused scheduler never alerts (by design).
- `system_status.snapshot()` is called every 30 s (spawns `vcgencmd` and
  `timedatectl`), the same work the footer already triggers every 5 s per
  open page.
- Merge-time doc follow-up (outside owned files, deliberately not edited):
  add `GET /api/alerts` to the endpoint list in `docs/optic-daemon.md` §4,
  and a pointer from `setup.md` to `docs/optic-daemon-alerts.md` §7 for
  creating `alerts.json`.

## User Verification Steps (after approving deploy)

1. On the Pi as `liam`: create `~/.config/optic-daemon/alerts.json` (0600)
   with a private topic, per design doc §7; subscribe to the topic in the
   ntfy phone app.
2. Deploy and restart (requires your approval; the Pi is shared).
3. `curl -s http://127.0.0.1:8000/api/alerts` → `channel: "ntfy"`,
   `config_error: null`, no warnings; the footer shows `Alerts OK`.
4. Trigger one real alert, e.g. temporarily set
   `"tmpfs_low_fire_below_percent": 100, "tmpfs_low_clear_above_percent": 100`
   and restart → expect FIRING on the phone within ~30 s; revert the file and
   restart → the condition does not persist across restarts, so no RESOLVED
   is sent. To see RESOLVED, instead stop the iMac receiver with files queued
   and a short `sync_backlog_after_secs`, then restart the receiver.

## Deployment (2026-09-21, user-approved)

The user approved deploying this branch over the `feat/provisioning` 0.1.30
build that was running on the Pi (so provisioning's changes are no longer on
the Pi until merged or redeployed). No version bump: deployed as `0.1.29`.

- Pre-checks (read-only): no build in progress; Pi reaches ntfy.sh
  (`curl https://ntfy.sh/v1/health` → 200); no `alerts.json` yet.
- **First deploy reported SUCCESS but installed a stale binary.**
  `./scripts/build-deploy-optic-daemon.sh` → Pi ran only 114 tests, the
  release "finished" in 0.40 s, and the installed binary contained no
  `health alerts` strings; `/api/alerts` fell through to the static-asset
  route (500). Cause: the script uploads with `tar` (preserves the Mac
  mtimes, here ~22:33 PDT) into a **shared** `CARGO_TARGET_DIR`
  (`~/.cache/optic-daemon-target`), whose cached artifacts from other
  sessions' builds (dep-info files at 23:13 and 23:37 PDT) were newer, so
  cargo's mtime freshness check skipped compilation. The script's
  "binary contains the version string" check passed because the stale
  artifact was also `0.1.29`. **Affects every track's deploy**; not fixed
  here (`scripts/` is outside this track) — recommended fix: have the script
  refresh source mtimes after upload (e.g. `find src -exec touch {} +`) or
  verify a per-build marker.
- Workaround: `find src -type f -exec touch {} + && touch Cargo.toml`
  locally (mtime only, no content/git change), then redeployed → Pi compiled
  for real: **152 tests passed**, strict Clippy passed, release built
  (1m 13s), `SUCCESS: optic-daemon 0.1.29 is active`.
- Post-deploy on the Pi: binary contains the alerts code; service active;
  `GET /api/alerts` → 200, `channel: "dry_run"`, `config_error: "alerts
  config file not found; …"`; startup log `health alerts running in dry-run
  mode`. All seven conditions `clear` with real signals: capture tmpfs
  256.0 of 256.0 MiB free, CPU 45.5 °C, `get_throttled=0x0` (vcgencmd works
  under the service sandbox), sync queue empty, scheduler paused.

### Going live (2026-09-21)

- User supplied the ntfy topic. Wrote `~/.config/optic-daemon/alerts.json`
  on the Pi via SSH stdin (topic not in any argv), `umask 077`, validated as
  JSON, atomic rename → `-rw------- liam`. Contents: `station_name` +
  `ntfy.topic` only (default server, no token, default thresholds). Topic is
  not recorded in this repository.
- `systemctl --user restart optic-daemon.service` → `/api/alerts`:
  `channel: "ntfy"`, `config_error: null`, no warnings; log
  `health alerts will notify via ntfy server=https://ntfy.sh`.
- Real test alert: temporarily set tmpfs thresholds to 101 % (always
  active), restarted → log `state change condition=CaptureTmpfsLow
  transition=Fired` at 07:19:15Z and `health alert delivered via ntfy
  title=Optic optic: FIRING capture tmpfs low` at 07:19:16Z;
  `last_delivery_error: null`, `outbox_len: 0`. Restored the original file
  (`cp -p` backup) and restarted → `channel: ntfy`, `active_count: 0`,
  `tmpfs_low_fire_below_percent: 25.0`. No RESOLVED was sent for the test
  alert (alert state does not survive a restart — documented limitation).
- Cosmetic: with `station_name: "optic"` the title reads `Optic optic: …`.
- User confirmed the notification arrived on their phone:
  "my phone see a FIRING capture tmpfs low".

## UI Change: Status Pills Moved to the Site Header (2026-09-21, user request)

User asked to move the alerts pill from the footer to the site header,
next to the Daemon version and Camera pills, with those pills on **all**
pages (previously Daemon/Camera existed only on the dashboard), and to
remove the Scheduler page's redundant header run-state pill ("Paused"),
since the Run control card already shows it.

- `src/web/footer.js`: builds the header `.status-row` (creating it on
  Capture History and Config, which had none) and prepends Daemon, Camera,
  Alerts before any page-specific pills. Reuses the dashboard's existing
  `#daemon-status`/`#camera-status` (still driven by `app.js`); creates and
  fills them from `/api/status` every 5 s on the other pages, with the same
  wording as `app.js`. Footer alerts pill removed.
- `src/web/scheduler.html` / `src/web/scheduler.js`: removed the
  `#run-state` header pill and its two update lines (the `running` variable
  is still used by the Pause/Resume button).
- Verified locally (debug daemon on 127.0.0.1:18080, Chrome): all four
  pages show `Daemon 0.1.29 · Camera unavailable · Alerts: dry-run` in the
  header (no camera or alerts config on the Mac); Scheduler header has no
  run-state pill; no footer alerts pill; no console errors;
  `node --check` passes for both edited scripts. Biome not run locally
  (not installed); the deploy script lints on the Pi.
- Deployed (user-approved) with `./scripts/build-deploy-optic-daemon.sh
  --assets` → Biome lint passed on the Pi, `SUCCESS: static web assets
  deployed and verified`. Served `footer.js`, `scheduler.html`,
  `scheduler.js` byte-identical to local (`cmp`); served `scheduler.html`
  has no `id="run-state"`; no service restart (ActiveEnterTimestamp still
  00:19:21 PDT). Visual check on the real Pi dashboard pending the user.

## GitHub CI and PR (2026-09-21, user-approved)

- Committed the work, rebased onto `origin/main` (18e2f84: adds
  `.github/workflows/ci.yml`, `rust-toolchain.toml`, provisioning 0.1.30);
  no overlapping files, clean rebase. Branch version is now 0.1.30 from
  `main`; still no bump in this branch.
- Local checks on the rebased tree: fmt OK, clippy `-D warnings` OK,
  **151 tests passed**.
- Pushed `feat/health-alerts` and opened PR #6
  (https://github.com/keefo/optic/pull/6). CI (push run 35573245157, PR run
  35573251239): Detect changed scope ✅, Biome (web assets) ✅ — first Biome
  run on this branch's JS/HTML — and Rust (Debian 13 arm64: fmt, test,
  clippy, release build, ldd/version check, artifact) ✅ on both runs.
- Not merged. Deploy-script stale-binary fix deferred to a separate PR
  (user decision).

## Out-of-Scope File

`src/web/footer.js`, and (UI change above) `src/web/scheduler.html` and
`src/web/scheduler.js`. `footer.js` was first edited only to add the alerts pill (the user chose
"API + footer badge"). The pill element is created from JavaScript so the
four page HTML files stay untouched, and existing `.pill.good/.bad/.neutral`
classes are reused so `styles.css` stays untouched. Merge-conflict risk:
any other track editing `footer.js`.

## Files This Track Owns

New `src/optic_alerts.rs`, `docs/optic-daemon-alerts.md`; small, additive
hooks in `src/main.rs` and one read-only status route in `src/web.rs`.
Read (do not restructure) `optic_scheduler.rs`, `optic_sync.rs`,
`system_status.rs`.

## Parallel-Session Coordination (applies to all four tracks)

This track runs in its own git worktree alongside three others
(`feat/capture-latency`, `feat/health-alerts`, `feat/provisioning`,
`feat/timelapse-builder`), all branched from `main` at `a6e1509`.

- **Do not bump `Cargo.toml` `version` on this branch.** The bump happens
  once, at merge time into `main`, immediately before deploy.
- **The Pi and camera are a single shared resource.** Ask the user before any
  deploy, service restart, reboot, or on-hardware test, and do not assume
  another session is not using it.
- **Stay inside the files this track owns (below).** If a change outside them
  becomes necessary, record why here and tell the user; it is a merge-conflict
  risk with the other tracks.
- Do not commit or push without explicit user approval (`CLAUDE.md`).
- Merge order is decided by the user; after each merge the other branches
  rebase onto `main`.
