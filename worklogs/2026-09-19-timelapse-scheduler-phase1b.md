# Dated Worklog: 2026-09-19 - Timelapse Scheduler, Phase 1b (Config Reboot-Durability)

Status: **implemented and hardware-verified via a real reboot on the Pi**
— the pre-existing gap this slice fixes (nothing in `config.json`
survives a real reboot) is confirmed both before the fix (earlier this
session, via `worklogs/2026-09-19-reboot-nonewprivileges-fix.md`'s
incidental discovery) and after (below): a distinctive test value
committed before a reboot is present afterward.

## Objective

Per `docs/optic-daemon-scheduler.md` §2.1 and the phase-1a worklog's
deferred item: relocate `config.json`/`preview_config.json`'s underlying
storage so committed config survives a real hardware reboot, without
adding SD-card read load to the existing every-few-seconds `/api/status`
poll. This is the prerequisite for the scheduler's own
start/pause/resume-survives-reboot requirement (§2.1's whole reason for
existing), but fixes it for the *existing* profile/settings config too,
not just anything scheduler-specific — the gap predates the scheduler
work entirely.

## Acceptance Criteria

- `config.json`'s durable copy lives on the real, persistent root
  filesystem (`~/.local/state/optic-daemon/`), not `/mnt/capture`
  (confirmed tmpfs, wiped on reboot).
- A fast tmpfs mirror (`/dev/shm/optic-daemon/`) is hydrated from the
  durable copy once at daemon startup and updated write-through on every
  commit; every frequent/polled read (`GET /api/status`, discard) uses
  only this mirror, never the durable path directly.
- `preview_config.json` is untouched — stays on `/mnt/capture` exactly as
  before; it's disposable staged-edit state by design and was never a
  durability candidate.
- A value committed before a real reboot is present in `/api/status`
  after it, verified on actual hardware, not inferred from code reading
  or unit tests alone.
- `src/durable_state.rs`'s generic hydrate/write-through/read-cached
  functions are reusable as-is for `schedule_run_state.json` once 1c
  needs it (not built in this slice — nothing consumes it yet).

## Test Plan (written before implementation)

- Unit tests (macOS dev target, no hardware): `durable_state.rs`'s
  `hydrate_cache`/`write_through`/`read_cached` against temp directories
  — hydrate copies durable→cache; hydrate is a no-op when durable is
  missing (fresh install); write_through updates both paths, repeatedly;
  `read_cached` only ever touches the cache path, never the durable one
  (explicit isolation check, not just "happens to work").
- Local smoke test (macOS, `cargo run --bin optic-daemon` against temp
  dirs via env var overrides): exercise the real HTTP handlers
  end-to-end — commit writes both files with correct content; status
  reads the cache when no preview exists; discard reads the cache and
  recreates preview from it.
- Pi-native build/test/clippy, matching every other change this session.
- **Real hardware reboot test** (the actual claim this slice makes):
  commit a distinctive, never-used-before setting value via the real
  API; confirm both the durable and cache copies contain it; trigger a
  real reboot via `POST /api/system/reboot` (already fixed and validated
  earlier this session); after the Pi comes back up, confirm
  `/mnt/capture`'s old preview file and `/dev/shm`'s old cache are both
  genuinely gone (proving tmpfs really was wiped, not a no-op test); then
  confirm `/api/status` still reports the distinctive value.

## Implementation Summary

- `src/durable_state.rs` (new): generic durable-path + tmpfs-cache-mirror
  primitives — `hydrate_cache`, `write_through`, `read_cached` — with 4
  unit tests. Not scheduler-specific; usable for any future durable+cache
  pair (`schedule_run_state.json` in 1c).
- `src/web.rs`: `AppState` gained `config_cache_path` alongside
  `config_path` (now meaning the *durable* path, no longer derived from
  `capture_dir`). `status()` and `discard_config()` now read
  `config_cache_path` via `durable_state::read_cached` instead of the old
  raw `config_path` read. `commit_config()` now writes through
  `durable_state::write_through(config_path, config_cache_path, ...)`
  instead of a single atomic write to the old tmpfs `config_path`.
  `preview_config.json` handling (`reconfigure_stream`, `stop_stream`)
  untouched.
- `src/main.rs`: registered `mod durable_state;`. Computes the new
  durable `config_path` (`state_dir.join("config.json")`, reusing the
  `state_dir` already computed for `optic_capture_log`) and
  `config_cache_path` (new `resolve_scheduler_cache_dir()`, `/dev/shm/
  optic-daemon` on Linux, `$TMPDIR/optic-daemon-cache` on macOS dev —
  `/dev/shm` isn't a real writable path on macOS, so this needed a
  platform split, same rationale as the existing `DEV_ASSET_DIR`
  fallback). Calls `durable_state::hydrate_cache` once at startup, before
  constructing `AppState`.
- `AppState::new()`'s signature grew two parameters (`config_path`,
  `config_cache_path`) — `#[allow(clippy::too_many_arguments)]` added,
  matching how this constructor was already long before this change.

## Validation

- **macOS dev target:** `cargo fmt --all -- --check` — clean. `cargo test
  --locked --all-targets` — 60/60 passed (4 new `durable_state` tests, 0
  regressions). `cargo clippy --locked --all-targets -- -D warnings` —
  clean.
- **Local smoke test** (`cargo run --bin optic-daemon` with
  `OPTIC_CAPTURE_DIR`/`OPTIC_CAPTURE_LOG_DB`/`OPTIC_CACHE_DIR` pointed at
  scratch temp dirs, `OPTIC_SYNC_ENABLED=false`): committed a config via
  the real `/api/stream/reconfigure` + `/api/config/commit` endpoints;
  confirmed both the durable and cache files were written with the
  correct content; removed the preview file and confirmed `/api/status`
  still returned the committed values (proving the cache read path, not
  preview); confirmed `/api/config/discard` reads the cache and
  recreates preview from it.
- **Pi-native build/deploy (`scripts/build-deploy-optic-daemon.sh`, still
  `optic-daemon` 0.1.27):** remote `cargo fmt --check`, `cargo test
  --locked --all-targets` (68/68 passed), `cargo clippy --locked
  --all-targets -- -D warnings` all clean. Release build/install/rollback
  checks all passed as usual.
- **Real hardware reboot test (the actual proof):**
  1. Committed `ev: 0.777` (a value never used elsewhere) via the live
     API. Confirmed both `/home/liam/.local/state/optic-daemon/
     config.json` and `/dev/shm/optic-daemon/config.json` contained it.
  2. `POST /api/system/reboot` → Pi actually rebooted.
  3. Post-reboot: `/mnt/capture/` no longer had the old
     `preview_config.json` (confirmed genuinely wiped — this is what
     proves the test is real, not vacuous), and `/dev/shm/optic-daemon/`
     was freshly recreated at the reboot's timestamp (confirming
     `hydrate_cache` ran at startup, not carried over).
  4. `GET /api/status` → `ev: 0.777`, still there.
  5. Durable SD-card copy unaffected throughout, as expected.

## Remaining Limitations / Follow-up

- `schedule_run_state.json` (the separate durable file for
  `ScheduleRunState`, per §2.1's commit-clobbers-a-pause race avoidance)
  is not built in this slice — `durable_state.rs`'s primitives are ready
  for it, but nothing calls them for run-state yet since the scheduler
  actor that would own that state doesn't exist until 1c.
- 1c (wire `optic_scheduler` into `AppState`, the actor loop,
  pause/resume endpoints, fold `schedule` into `AppConfig`) and 1d (the
  dedicated `/scheduler.html` page) remain, per the phase-1a worklog's
  scoping.
