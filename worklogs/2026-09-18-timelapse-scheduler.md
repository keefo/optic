# Dated Worklog: 2026-09-18 - Timelapse Scheduler (`optic_scheduler`)

Status: planned, not implemented. Pre-implementation plan only, per the
required workflow (scope, current-state findings, design proposal, test
plan, before writing code). Unlike the Data Sync Manager worklog earlier
today, this one has more unresolved scope forks than defaults are safe to
assume silently — see "Decisions Needed" before implementation starts.

## Objective

Implement `optic_scheduler`, the last unimplemented subsystem from
`docs/optic-daemon.md`: an autonomous loop that takes pictures on a
schedule and queues them for `optic_sync` to transfer, with no operator
action required.

## Current State (found while scoping this)

As with the Data Sync Manager, `docs/optic-daemon.md` section 5 describes a
*target* design that doesn't match what exists today:

- **No config schema exists for scheduling.** The doc's target
  `config.toml` has `[station]` (lat/long/elevation), `[schedule]`
  (`mode = "solar_adaptive"`, `default_interval_sec`,
  `golden_hour_interval_sec`, `night_mode`, `night_interval_sec`), and
  `[exposure]`. None of this exists. The real config
  (`src/camera.rs::AppConfig`, persisted as `config.json`/
  `preview_config.json` in `/mnt/capture`, edited only via the web UI's
  commit/discard endpoints) is just `{ profile, settings }` — no schedule,
  no station coordinates, no interval, nothing autonomous-capture-related
  at all. There's also still no `toml` crate dependency and no `config.toml`
  anywhere.
- **No solar-ephemeris dependency exists.** The design calls for a `spa`
  crate (sun-position-algorithm) computing elevation from
  latitude/longitude/time to pick between golden-hour/daytime/night
  intervals. Nothing like this is in `Cargo.toml` today.
- **The design's "Acquire Camera Hardware Mutex" / "Execute `rpicam-still`"
  steps don't match the real architecture, and this is good news, not a
  gap to fill.** Every capture already goes through
  `OpticCamera::capture_to_stage` (`src/optic_camera.rs` →
  `src/native_camera.rs`), a bounded-FIFO actor that already serializes
  every camera operation. I traced what happens when a `Capture` command
  arrives while a live preview stream is active
  (`native_camera.rs` around the `NativeCommand::Capture` handler): it
  unconditionally calls `stop_pipeline(...)` first, then captures, then
  publishes the result — i.e. **a still capture request already stops any
  active preview automatically**, exactly like the "Capture & transfer"
  button already does today. On the frontend, `refreshStatus()` (already
  polling every 3s) already detects "camera stopped streaming but we
  thought it was live" and calls `ensurePreview()` to restart it — so an
  operator watching the live dashboard already self-heals if *anything*
  (not just their own button clicks) stops the stream. **This means
  `optic_scheduler` needs no new arbitration/mutex logic at all** — it can
  just call `camera.capture_to_stage(&capture_dir, request)` on a timer,
  exactly the same call `POST /api/capture` already makes, and the
  existing actor and existing frontend polling handle the rest.
- **A concrete resource risk worth flagging up front**: `/mnt/capture` is a
  bounded 256 MiB `tmpfs` (confirmed live on the Pi: `size=262144k`).
  Measured real file sizes this session: Master Archive JPEG+DNG pair
  ≈ 30 MB (5.3 MB JPEG + 24.6 MB DNG), 2K Binning JPEG (no DNG)
  ≈ 90–95 KB. At Master Archive+DNG, the tmpfs can hold roughly **8
  captures** before filling completely; at 2K Binning JPEG-only, it can
  hold **thousands**. If `optic_scheduler` fires captures faster than
  `optic_sync` (or the still-present Phase 6 timer) can drain them — e.g.
  during a network outage, which `optic_sync`'s own backoff can stretch to
  15-minute retry gaps — a short interval with a heavy profile could fill
  the tmpfs and start failing captures (or worse, failing in a way that
  hasn't been designed for yet: what should happen when
  `/mnt/capture` is full and a scheduled capture is due?). This interaction
  between two features I've now built/scoped in the same session is the
  single biggest reason this worklog stops at planning instead of also
  implementing: the answer changes the design.

## Decisions Needed (blocking implementation)

Bigger and more numerous than the Data Sync Manager's three, because this
subsystem has essentially no existing scaffolding to anchor it to. Answering
these changes the actual implementation, not just a config default, so
they need a decision rather than "best judgement":

1. **Solar-adaptive cadence, or fixed interval only for v1?** The full
   design (golden hour / daytime / night bands driven by station
   lat/long + a solar-position calculation) is a meaningfully bigger build
   than a single configurable interval with an on/off switch. Recommend
   **starting with a fixed interval** (one `interval_secs` knob, no solar
   math, no station coordinates) and treating solar-adaptive cadence as a
   clean follow-up — it only changes "what interval to use right now," not
   the capture-triggering mechanism itself, so it doesn't need to be built
   first to avoid rework.
2. **Where does schedule config live?** Given there's no `config.toml` or
   `toml` dependency anywhere in the real codebase (unlike what the design
   doc assumes), recommend extending the existing `AppConfig`/`config.json`
   with a new `schedule` section (`{ enabled: bool, interval_secs: u64 }`
   for the fixed-interval v1) rather than introducing a second config file
   and a new dependency. This also means schedule settings would go through
   the *existing* commit/discard flow for free.
3. **What capture profile/settings does an autonomous shot use?** Simplest:
   whatever `AppConfig`'s current committed `profile`/`settings` already
   are (the same ones the dashboard's capture-profile radio buttons and
   controls already manage) — no new "scheduler-specific" settings. Confirm
   this is desired, versus wanting an independent profile just for
   autonomous captures (e.g. always 2K Binning for timelapse regardless of
   what's selected for manual test shots).
4. **What should happen when `/mnt/capture` is at or near capacity when a
   scheduled capture is due?** Options: skip the tick and log/report it
   (simplest, matches "never miss the *next* opportunity, but don't jam
   more into a full disk"); block/retry shortly after; or something else.
   This needs an answer before writing the capture-triggering loop, not
   after.
5. **DNG alongside JPEG for scheduled captures?** Given DNG roughly
   quintuples the per-capture size (and therefore mostly determines how
   fast risk #4 above can occur), confirm whether autonomous captures
   should ever save DNG, or whether that stays a manual/test-shot-only
   option.
6. **UI scope**: following the pattern just built for Data Sync (a status
   panel + pause/resume/retry-now), does the user want an equivalent
   "Schedule" panel (enabled/paused toggle, interval, time until next
   capture, last capture result) with matching `/api/schedule/*` control
   endpoints? Assumed yes for consistency, but confirm scope (e.g. is a
   "capture now" manual trigger wanted in addition to pause/resume?).
   **Placement decided** (2026-09-18, after this worklog was first drafted):
   the dashboard was restructured so `.profile-card` and the old
   `.controls-card` are now two nested `<section>`s (`.profile-card` and
   `.controls-panel`) inside one outer `<section class="controls-card">`
   card, with the config Save/Discard buttons moved to the bottom of that
   same merged card (see `src/web/index.html`/`styles.css`, not yet
   committed). The Schedule panel should be a **third nested section**
   inside that same `.controls-card`, below `.controls-panel` and above the
   Save/Discard button row — not a separate standalone card like Data
   Sync's. Follow the existing nested-section pattern exactly: a plain
   `<section aria-labelledby="...">` with its own `.section-heading`/`h2`,
   reset to no border/background/padding of its own via the
   `.controls-card .profile-card, .controls-card .controls-panel` CSS rule
   (extend that selector list to include the new schedule section's class),
   and a `border-top` divider matching `.controls-panel`'s.

## Proposed Design (v1, pending the decisions above)

Assuming the recommended answers above (fixed interval, config.json
extension, current committed profile/settings, skip-and-report when full,
no DNG for autonomous captures):

- New `src/optic_scheduler.rs`, following the exact same bounded-actor
  shape as `optic_camera` and `optic_sync`: a cheap `Clone` handle
  (`mpsc::Sender` + `watch::Receiver`), one owning `tokio::task`.
- `AppConfig` gains a `schedule: ScheduleConfig` field
  (`{ enabled: bool, interval_secs: u64 }`, defaulting to disabled) —
  read fresh from `config.json` on every tick (or on a change
  notification; needs the same "hot reload" property the design doc
  calls for, which is cheap since `AppState` already knows how to read
  `config.json`/`preview_config.json`).
- On each tick (interval-driven, `tokio::time::interval` again — consistent
  with `optic_sync`'s polling approach rather than introducing a different
  concurrency primitive for a very similar problem): if enabled and not
  paused, check `/mnt/capture` free space against a threshold before
  calling `camera.capture_to_stage(...)`; if under threshold, skip this
  tick and record why (answers decision #4).
- Status snapshot (`enabled`, `paused`, last capture result/time, next
  capture in N seconds, skip reason if any) exposed the same way
  `optic_sync`'s `SyncStatus` is, added to `GET /api/status`.
- `POST /api/schedule/{pause,resume}` (and possibly `capture-now`, per
  decision #6), mirroring `/api/sync/*`.
- Dashboard: a new nested `<section>` inside the merged `.controls-card`
  (alongside `.profile-card` and `.controls-panel`, below the latter, above
  the Save/Discard button row) — not a new standalone card. Content
  mirrors the "Data Sync" panel's approach (status fields + control
  buttons) but laid out as a nested section per the placement decision
  above, not a full bordered card.

## Test Plan (written before implementation)

- Unit tests (macOS dev target, no camera hardware):
  - Interval/tick-due logic (given "now" and "last capture at", is a
    capture due?) as a pure function, independent of the actor loop —
    same style as `optic_sync`'s `ActorState` being unit-testable without
    a real transport.
  - Free-space-threshold skip logic: given a simulated free-space number
    under/over the threshold, confirm capture is attempted or skipped and
    the reason is recorded.
  - Config hot-reload: change `interval_secs`/`enabled` in a fresh
    `config.json` on disk between two status reads and confirm the actor
    picks up the change without a restart.
  - Pause/resume mirroring the `optic_sync` command tests.
- Integration test on the Pi (native target, required environment):
  - Enable scheduling with a short interval, confirm real captures appear
    in `/mnt/capture` on schedule and get picked up by the already-verified
    `optic_sync` automatically (this is the real end-to-end proof: capture
    → sync → arrive on the Mac, with no manual API calls at all, the way
    an actual deployed station would run for real).
  - Confirm a manual capture/test-shot from the dashboard while the
    scheduler is enabled doesn't double-fire or conflict (the existing
    actor's FIFO queue should already guarantee this, but it's worth
    confirming live rather than assuming).
  - Simulate the full-tmpfs case (fill `/mnt/capture` toward its 256 MiB
    cap, e.g. by pausing `optic_sync` first) and confirm the scheduler
    reports skips rather than erroring or crashing.
  - Dashboard: confirm the new panel renders and the next-capture countdown
    and pause/resume controls work from a real browser (also still owed
    from the Data Sync Manager worklog — worth doing both together).

## Explicitly Out of Scope for This Worklog

- Solar-adaptive cadence (see decision #1) — v1 is fixed-interval only.
- Any remote-authoritative config concept / config mirroring to the Mac —
  `optic_sync`'s worklog already deferred this pending `optic_scheduler`
  existing; this worklog doesn't pick it back up either, since even v1
  scheduling doesn't need a *remote* config source, just a local one.
- Changing anything about `optic_sync` or the Phase 6 shell timer.
