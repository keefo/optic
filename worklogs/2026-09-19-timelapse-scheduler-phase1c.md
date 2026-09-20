# Dated Worklog: 2026-09-19 - Timelapse Scheduler, Phase 1c (Wired In, Live)

Status: **implemented and hardware-verified — a real autonomous capture
fired on schedule on the actual Pi, using the real IMX477 camera, and was
automatically drained by `optic_sync` to the remote Mac.** The scheduler
is now reachable for the first time (`POST /api/schedule/{pause,resume}`,
`GET /api/status`'s `schedule` field), though there's still no rules-
editor UI (1d) — rules were written directly for this test, the way 1d
will do it via HTTP once built.

## Objective

Per the phase-1a worklog's scoping: wire the Phase 1a rule engine and the
Phase 1b durable-config mechanism into the actual running daemon —
`AppState`, a real bounded actor that ticks and fires captures,
`POST /api/schedule/{pause,resume}`, and folding `schedule` into the
existing `AppConfig`/commit/discard flow.

## Acceptance Criteria

- `AppConfig` gains `schedule: ScheduleConfig` and `save_dng: bool`
  (needed for the Dci4k DNG-preference decision, design doc §6 — see
  Findings below for why this field didn't already exist).
- A bounded actor (`SchedulerHandle`, same shape as `optic_camera`/
  `optic_sync`) starts at boot in whatever `ScheduleRunState` was durably
  persisted (§2.1) — `Paused` on a fresh install — and, while `Running`,
  fires real captures via the existing `camera.capture_to_stage` call at
  the times `occurrences()` computes, with no new hardware-arbitration
  logic (confirmed still true, unaffected by this slice).
- `POST /api/schedule/pause`/`resume`: immediate actions (not staged),
  mirroring `/api/sync/pause`/`resume`. Status must already reflect the
  new state by the time the response returns — no race where the
  response says "Running" but a status poll a moment later still shows
  "Paused."
- `commit_config` wakes the actor immediately on every successful commit
  (design doc §2.1's hot-reload requirement) and rejects the commit
  outright if two rules share a slug (§3.1/§14, authoritative check).
- Captures fired by the scheduler are recorded to the capture history log
  with `source: "scheduler"`, distinguishable from `"web_ui"`.
- A rule actually fires a real capture on real hardware, and that capture
  is picked up by the existing, independently-verified `optic_sync` —
  the same "real end-to-end proof" the very first scheduler worklog
  (`2026-09-18-timelapse-scheduler.md`) specified as the ultimate test.

## Test Plan (written before implementation)

- Unit tests (macOS dev target): actor lifecycle (spawn/status/pause/
  resume/shutdown/notify) using `OpticCamera::spawn_native()`'s no-
  hardware fallback, matching `optic_camera`'s own test pattern —
  *specifically* verifying the pause/resume race is closed (status
  reflects the new state before the command's reply arrives, not after).
- Local smoke test (`cargo run`, real HTTP handlers): `/api/status`
  carries the new `schedule` field; `/api/schedule/{pause,resume}` work
  and persist durably; pause/resume never touch `config.json`.
- Pi-native build/test/clippy, matching every other change this session.
- **Real hardware test** (the actual proof): write a real rule via the
  existing preview/commit flow (no UI yet, so directly via the API, the
  way 1d's UI will do it), resume the scheduler, confirm a real capture
  fires on the real camera at the predicted time, is recorded with
  `source: "scheduler"`, and is picked up by `optic_sync`.

## Findings While Implementing (worth recording, not just the plan)

- **`start_stream`/`reconfigure_stream` would have silently wiped
  `schedule.rules` on every live-preview slider tweak.** Both handlers
  constructed a brand-new `AppConfig { profile, settings }` from scratch
  on every call — fine before this slice (those were the only two
  fields), but now discards `save_dng`/`schedule` on every calibration
  action. Fixed by reading the current config first (extracted into a
  shared `current_app_config` helper, also de-duplicating `status()`'s
  identical fallback logic) and overriding only `profile`/`settings`.
  Caught by working through the design, not by a test failing — worth
  flagging as the kind of regression that's easy to miss when adding a
  field to a struct that gets reconstructed elsewhere.
- **The design doc's §6 claim ("Dci4k follows `settings.save_dng`") had
  no real field to follow.** `save_dng` only ever existed as a per-
  request field on manual `/api/capture` calls (a UI checkbox at click
  time) — there was nothing for an autonomous capture, with no click, to
  read. Added `AppConfig.save_dng: bool` (default `false`) as a real,
  committed preference, mirroring `CaptureRequest`'s existing shape.
  `fire_capture` coerces it defensively regardless (`MasterArchive`→
  `true`, `Binning2k`→`false` always, ignoring whatever's stored — only
  `Dci4k` actually reads the field), so a scheduled capture can never
  violate `validate_raw_policy` even if the stored value is stale/wrong.
- **A second race, separate from the one this worklog's test plan
  already targeted**: `handle_command` mutated `run_state` and replied
  before the status `watch` channel was updated — the actual update only
  happened on the *next* loop iteration. Fixed by publishing status
  synchronously inside `handle_command`, before the reply, at all three
  call sites. Verified with a real Pi hardware check too (immediate
  `resume()` response showed `"run_state":"Running"`), not just the unit
  test.
- **`OpticCamera::spawn_native()` cannot be called more than once
  concurrently on real Linux/libcamera hardware** — libcamera enforces a
  hard one-`CameraManager`-per-process rule. First deploy attempt
  segfaulted the Pi-native test binary (`SIGSEGV`) because 5 actor tests,
  run in parallel by the test harness, each spawned their own instance.
  Invisible on macOS (no real camera manager to collide over). Fixed by
  moving the actor tests into their own `#[cfg(all(test, not(target_os =
  "linux")))]` submodule — the *exact* gate `optic_camera.rs`'s own actor
  test already uses, for the identical reason; matched, not invented.
  The deploy script's rollback/`on_exit` safety net (from the earlier RAM-
  headroom worklog) kept the live daemon on the old version throughout —
  confirmed no user-visible impact from the failed attempt.

## Implementation Summary

- `src/camera.rs`: `AppConfig` gains `save_dng: bool` and
  `schedule: crate::optic_scheduler::ScheduleConfig`, with container-level
  `#[serde(default)]` so a `config.json` committed before this slice
  still deserializes (missing fields fall back to `AppConfig::default()`).
- `src/optic_scheduler.rs`: added the bounded actor —
  `SchedulerHandle`/`SchedulerStatus`/`LastCapture`/`SchedulerUnavailable`/
  `SchedulerCommand`, `run_actor` (ticks against `occurrences()`, fires
  via `capture_to_stage`, records to the capture log with
  `source: "scheduler"`), `handle_command` (race-free status
  publication), `fire_capture` (defensive DNG coercion). 9 new tests (4
  pure/composition-adjacent, 5 actor-lifecycle, the latter gated non-Linux
  per the libcamera finding above).
- `src/web.rs`: `AppState` gained a `scheduler: SchedulerHandle` field;
  new `current_app_config` helper (de-duplicates the preview-or-cache
  fallback `status()` already had); `start_stream`/`reconfigure_stream`
  fixed to preserve `save_dng`/`schedule` instead of discarding them;
  `commit_config` validates rule slugs authoritatively and wakes the
  scheduler on success; new `POST /api/schedule/{pause,resume}` handlers
  and routes; `StatusResponse` gained a `schedule: SchedulerStatus` field.
- `src/main.rs`: hydrates the new `schedule_run_state.json` durable+cache
  pair at startup (same pattern as `config.json`, §2.1), reads the
  initial `ScheduleRunState`, spawns the scheduler before constructing
  `AppState`, and stops it during graceful shutdown (`shutdown_signal`
  gained a third parameter — it previously only stopped `camera`/`sync`).
- `Cargo.toml`/`Cargo.lock`: version bump `0.1.27` → `0.1.28` (first
  slice in this scheduler effort with real, reachable user-visible
  behavior — the earlier 1a/1b slices were inert/internal-only and
  weren't bumped).

## Validation

- **macOS dev target:** `cargo fmt --all -- --check`, `cargo test
  --locked --all-targets` (65/65 passed, including the 5 actor tests
  gated to run here), `cargo clippy --locked --all-targets -- -D
  warnings` — all clean.
- **Local smoke test** (`cargo run --bin optic-daemon`, real HTTP calls):
  `/api/status` carries `schedule`; `/api/schedule/resume` immediately
  returns `run_state: Running` (no race); `/api/schedule/pause` returns
  to `Paused`; both durable and cache `schedule_run_state.json` correctly
  show the persisted value; `config.json` durable file was never created
  by any pause/resume call (confirms the two-file separation holds).
- **Pi-native build/deploy:** first attempt failed with a real `SIGSEGV`
  in the test binary (see Findings) — the deploy script's own rollback
  safety net correctly kept `optic-daemon` 0.1.27 active throughout, with
  no user-visible impact, confirmed via `systemctl --user is-active` and
  `/api/status`. Fixed the root cause (test gating) and redeployed
  clean: 68/68 tests passed, clippy clean, release build/install/rollback
  checks all passed, `optic-daemon` 0.1.28 active.
- **Real hardware end-to-end test (the actual proof):**
  1. Read current committed profile/settings via `/api/status`.
  2. Wrote a real preview config (via SSH, since no rules-editor UI
     exists yet) with one rule: `Interval { every_secs: 20,
     align_to_wall_clock: true }`, slug `hw-validation`, using the
     already-committed profile/settings.
  3. `POST /api/config/commit` → succeeded (slug validation passed);
     `config.schedule.rules` correctly reflected in `/api/status`.
  4. `POST /api/schedule/resume` → immediately returned
     `next_capture_at`/`next_capture_rules: ["hw-validation"]` — the
     actor picked up the new rule on the very first tick after resuming.
  5. Polled `/api/status`: **12 seconds later, `last_capture` showed
     `{success: true, error: None, rule_slugs: ["hw-validation"]}`** — a
     real capture, on the real IMX477 sensor, fired autonomously by the
     scheduler with no manual trigger.
  6. Confirmed in the capture history SQLite DB directly: `('testshot-2k-
     binning-<timestamp>', 'scheduler', 'binning_2k', 1)` — correct
     source attribution, correct profile, success.
  7. Confirmed `optic_sync`'s `transferred_files`/`transferred_bytes`
     increased — the scheduler-fired capture was automatically drained
     to the remote Mac, with zero manual intervention anywhere in the
     pipeline from "resume" to "arrived on the receiving host."
  8. Cleaned up: paused the scheduler, committed an empty rule set, so
     the 20-second test rule doesn't keep firing indefinitely on the
     user's real device.

## Remaining Limitations / Follow-up

- **No rules-editor UI (1d).** Everything above was exercised by writing
  JSON directly, which is exactly what 1d's dedicated `/scheduler.html`
  page needs to do instead, through the existing preview/commit/discard
  flow — no new backend work implied, just the frontend.
- **No Shot Forecaster (1d)** — `occurrences()` is ready to serve it
  (design doc §3's whole point), just not exposed via any endpoint yet.
- **Astronomy (Phase 2/3/4: Solar/Lunar/MilkyWay)** remains out of scope,
  per the phasing decision — `Trigger`/`Constraint` in this slice still
  only contain the Phase-1 variants.
- **`IDLE_RECHECK` (5 min) and `LOOKAHEAD` (48h)** are reasonable but
  unvalidated-under-load constants — fine for a single test rule; worth
  revisiting once real multi-rule, long-running usage exists.
