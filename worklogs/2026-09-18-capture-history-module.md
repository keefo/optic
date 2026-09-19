# Dated Worklog: 2026-09-18 - Capture History Log (`optic_capture_log`)

Status: implemented, unit-tested, deployed (`optic-daemon` 0.1.16), and
hardware-validated on the Pi — see Validation below. Full design detail
lives in `docs/optic-daemon-capture-log.md`; this worklog covers scope,
decisions, and validation, and should be read alongside that doc rather
than duplicating it.

## Objective

Implement `optic_capture_log`, the 4th daemon subsystem (see
`docs/optic-daemon.md` section 1): a passive observer that records every
capture request — settings, per-stage timing, outcome, and output artifact
metadata — so capture history survives beyond a single `tracing` session
and can be queried by the dashboard (e.g. "how many captures on a given
date", "capture-time trend over the last year").

## How This Was Scoped

Arose directly out of the capture-latency investigation
(`docs/optic-daemon-capture-performance.md`): that work proved the ~5s
capture delay is a real, measurable, stage-by-stage phenomenon, but the
only way to see it was a live `tracing` session — nothing persists,
because journald has no persistent storage on this Pi (confirmed;
see that doc's §2.2). The natural follow-up was "so where should this
actually live long-term," which led to a multi-round design discussion
covering:

- Whether to embed capture metadata directly in the JPEG/DNG files
  (rejected — failed captures produce no file to embed into; see design
  doc §3.1)
- Whether to use a single shared/batched log (JSONL, rotated and shipped
  like capture files) vs. one log file per capture (the user's explicit
  preference, for clarity, despite marginally more sync overhead — design
  doc §3.1)
- Whether Pi-side history storage should be a flat file or a real database
  — settled on SQLite once the actual query patterns (date-range counts,
  year-long trend/degradation analysis) were articulated; JSONL doesn't
  support that kind of query efficiently on a 1GB Pi (design doc §3.2)
- Where that database can actually live given the daemon's systemd
  sandboxing (`ProtectSystem=strict`, `ProtectHome=read-only`) and this
  project's read-only-root design goal (OverlayFS, not yet enabled on the
  live Pi) — this remains open, see Decisions Needed below

## Decisions Made

1. **OverlayFS Phase 9 compatibility** (design doc §4): **Option A —
   design for today's writable-root state.** The live Pi is plain
   writable ext4 and Phase 9 has no scheduled enablement date; a dedicated
   writable mount now would be speculative work for a phase that may not
   land soon. Traded off explicitly, not silently: if Phase 9 is ever
   enabled, `~/.local/state/optic-daemon` becomes read-only along with the
   rest of `/home`, and `history.db` writes start failing. `CaptureLog`
   degrades gracefully when that happens (see "How this fails" below), so
   the failure mode is a loud, logged one rather than a crash — but the
   history feature itself would go dark until revisited. Implemented as a
   single `ReadWritePaths=` addition in `systemd/optic-daemon.service`.
2. **SQLite schema — finalized:**
   ```sql
   CREATE TABLE IF NOT EXISTS captures (
       capture_id TEXT PRIMARY KEY,
       captured_at INTEGER NOT NULL,   -- unix seconds, completion time
       source TEXT NOT NULL,           -- "web_ui" today; "scheduler" later
       profile TEXT NOT NULL,          -- "master_archive" | "dci_4k" | "binning_2k"
       save_dng INTEGER NOT NULL,      -- 0/1
       success INTEGER NOT NULL,       -- 0/1
       duration_ms INTEGER NOT NULL,
       bytes_total INTEGER NOT NULL,
       error TEXT,                     -- NULL on success
       detail_json TEXT NOT NULL       -- full CaptureLogEntry, same as the .log.json file
   );
   CREATE INDEX IF NOT EXISTS idx_captures_captured_at ON captures(captured_at);
   CREATE INDEX IF NOT EXISTS idx_captures_success ON captures(success);
   ```
   Structured columns cover every motivating query from design doc §1
   (date-range counts, duration trend, success-rate); `detail_json` keeps
   full fidelity (settings, per-request detail) without a wider table.
3. **How `optic_capture_log` observes capture completion — confirmed, and
   narrower than originally sketched.** The hook lives entirely in
   `web.rs::capture()`, reading `&Result<CaptureResult, CameraError>`
   after the actor replies (fires on both outcomes) — **no changes to
   `OpticCamera`'s or `NativeCameraBackend`'s actor protocol**, and no
   changes to `native_camera.rs` (the real-hardware capture path) at all.
   This is a deliberately smaller scope than the design doc's filename
   convention implied: rather than threading a pre-generated
   `capture_id` down through the actor so `native_camera.rs` can use it as
   the file basename, the `capture_id` is *derived after the fact* — on
   success, from the JPEG's own basename (`result.files[0].filename`,
   minus extension), which already gets the same shared-basename outcome
   design doc §3.1 wants, with zero touch to untestable-from-this-machine
   native code. On failure (no output file exists), a synthetic id is
   minted (`testshot-<slug>-failed-<unix-ms>`) — a case the original
   design doc's example didn't cover; `docs/optic-daemon-capture-log.md`
   §3.1 has been updated to document it.
4. **Downstream lifecycle tracking — deferred, out of v1 scope**, exactly
   as flagged as an option in the original decision. Coordinating
   `optic_sync` drain/transfer status back into a capture's history row
   adds a second writer to the same SQLite row and a coupling between two
   independently-designed subsystems; not worth it before the simpler
   parts of this module have run in production. `docs/optic-daemon-capture-log.md`
   §6 now lists this explicitly as a non-goal for v1.
5. **Actor queue wait time — deferred, out of v1 scope.** Same reasoning:
   it needs a new timestamp threaded through `OpticCamera`'s enqueue path,
   which is exactly the kind of actor-protocol change decision 3 avoided
   for v1. Fast-follow candidate once the rest of this module has proven
   useful.

## Design (as implemented)

See `docs/optic-daemon-capture-log.md` in full. Summary:

- New `src/optic_capture_log.rs`: a passive recorder, not an actor with
  its own command queue (unlike `optic_camera`/`optic_sync`) — no external
  callers need request/reply semantics, just an internal `record(entry)`
  call site in `web.rs::capture()`.
- `CaptureLog::open(db_path, capture_dir)` opens (creating if needed) the
  SQLite database, sets WAL mode, and runs the DDL above. Called once at
  startup in `main.rs`; failure to open (e.g. `ReadWritePaths=` not yet
  deployed) is logged as a warning and leaves `AppState.capture_log` as
  `None` rather than failing the whole daemon — captures still work, they
  just aren't recorded to history until the state directory is writable.
- `CaptureLog::record(entry)` does two best-effort writes per capture:
  a `<capture_id>.log.json` file into `capture_dir` (async, via
  `tokio::fs::write`) and an `INSERT OR REPLACE` row into `history.db`
  (via `tokio::task::spawn_blocking`, since `rusqlite` is blocking I/O).
  Either write failing is logged and swallowed — per design doc §2, this
  module cannot affect whether the capture itself succeeds.
- `optic_sync::is_capture_filename` extended to also allowlist the
  `.log.json` suffix, so these files get drained to the Mac exactly like
  the JPEG/DNG they're paired with, no new transfer code.

## Test Plan

- Unit tests (macOS dev target) — **all implemented and passing, see
  Validation below**:
  - Log-file JSON shape/serialization round-trips correctly.
  - SQLite schema: insert + query round-trip for the common query
    patterns from §1 of the design doc (count by date, average duration
    over a range) against an in-memory or temp-file database.
  - `optic_sync`'s extended `is_capture_filename` correctly allowlists
    `.log.json` alongside `.jpg`/`.dng` and still rejects everything else
    (regression test for the existing config-file-sweep protection).
  - Recording a *failed* capture (no output files) still produces a
    history record with the error detail — the specific case that ruled
    out the metadata-embedding alternative in the design doc.
- Integration test on the Pi (native target, required environment) — **not
  yet run; see Validation below**:
  - Confirm the SQLite file is actually created and writable under the
    real systemd sandboxing after the `ReadWritePaths=` change.
  - Trigger several real captures (success and at least one induced
    failure, e.g. an invalid profile/DNG combination) and confirm both the
    `.log.json` files and the SQLite rows match reality, including that
    the `.log.json` files get synced to the Mac by `optic_sync` like any
    other capture file.
  - Confirm a real dashboard-style query (date range, count, average
    duration) against the live database returns correct results and runs
    fast enough not to be noticeable on page load.
  - Restart the daemon (simulating a reboot) and confirm the SQLite
    database survives (proves it's genuinely on persistent storage, not
    accidentally still under `/mnt/capture`'s tmpfs).

## Explicitly Out of Scope (v1)

- Dashboard UI for querying this history (data layer only, per design doc
  §6).
- `optic_scheduler` integration (it doesn't exist yet) — this module is
  designed to accept a second event source later without rework, but
  wiring it up is out of scope until the scheduler itself is built.
- Mac-side merged/backup database (design doc §3.2's closing note).
- Per-stage capture timing breakdown, downstream lifecycle tracking,
  system-context fields, and output-file checksums — see design doc §6
  for the complete list and reasoning; none require an actor-protocol
  change to add later.
- A dedicated writable partition for OverlayFS Phase 9 survival — Option A
  was decided (design doc §4); this is a deliberate, documented tradeoff,
  not a punt.

## Implementation Summary

Files changed:
- `src/optic_capture_log.rs` (new) — `CaptureLogEntry`, `CaptureLog`
  (`open`/`record`), `derive_capture_id`/`failure_capture_id`,
  `profile_key`/`profile_slug`. 10 unit/integration tests.
- `src/camera.rs` — added `Deserialize` to `CaptureFile` (needed for
  `CaptureLogEntry` to round-trip through JSON in tests).
- `src/web.rs` — `AppState` gained a `capture_log: Option<CaptureLog>`
  field; `capture()` builds a `CaptureLogEntry` from the request fields and
  `&result` (read-only, before `result` is consumed by the response) and
  calls `capture_log.record(entry).await` when present.
- `src/main.rs` — registers the `optic_capture_log` module; resolves the
  history DB path (`OPTIC_CAPTURE_LOG_DB`, defaulting to
  `~/.local/state/optic-daemon/history.db`); opens `CaptureLog` at startup,
  degrading to `None` (logged as a warning) rather than failing the daemon
  if the open fails.
- `src/optic_sync.rs` — `is_capture_filename` extended to allowlist the
  `.log.json` suffix; new test cases for both the success- and
  failure-case filename shapes.
- `Cargo.toml` / `Cargo.lock` — added `rusqlite = { version = "0.32",
  features = ["bundled"] }` (compiles its own SQLite via `cc`; no new
  system package needed on either macOS dev or the Pi, since `gcc`/`g++`
  are already required build tools there).
- `systemd/optic-daemon.service` — `ReadWritePaths=` extended with
  `%h/.local/state/optic-daemon`.
- `scripts/setup-optic-daemon-phase-01.sh` — creates
  `~/.local/state/optic-daemon` (mode `0700`) before the systemd unit is
  (re)started, since `ReadWritePaths=` targets must exist beforehand.
- `docs/optic-daemon.md` — module 4's list entry updated from "Planned" to
  "Implemented and unit-tested... not yet deployed or hardware-validated",
  and the per-stage-timing claim corrected to "total duration" (v1 scope).
- `docs/optic-daemon-capture-log.md` — updated throughout: OverlayFS
  decision resolved (§4), SQLite schema finalized (§3.2), `capture_id`
  derivation approach documented (§3.1), and every field/design element
  not actually implemented in v1 explicitly marked deferred (§2, §6).

## Validation

- **`cargo build`** (macOS dev target): clean, no warnings.
- **`cargo test --all-targets`** (macOS dev target): **30 passed, 0
  failed** — all pre-existing tests still pass, plus the 6 new
  `optic_capture_log` tests (JSON round-trip, success/failure
  `capture_id` derivation, `record()` writing both the `.log.json` file
  and the SQLite row for a success and for a failure, and the date-range/
  average-duration SQL query patterns from design doc §1).
- **`cargo fmt --all -- --check`**: clean (after running `cargo fmt --all`
  once to apply two formatting fixes in the new/edited files).
- **`cargo clippy --all-targets -- -D warnings`** (macOS dev target):
  clean, no warnings — matches the strict Clippy gate
  `scripts/build-deploy-optic-daemon.sh` runs on the Pi before every
  deploy.
- **Pi-native integration test plan — run 2026-09-18 after deploying
  `optic-daemon` 0.1.16** via `scripts/build-deploy-optic-daemon.sh`
  (bootstrap, `cargo test --locked --all-targets`: 38 passed on the native
  target including the 6 `optic_capture_log` tests; strict Clippy: clean;
  release build; install; health check — all passed, see the script's own
  output). Then manually verified against the live service:
  - `ReadWritePaths=` sandboxing: `~/.local/state/optic-daemon/history.db`
    (+ `-shm`/`-wal`) exists and is owned by `liam`, confirming
    `CaptureLog::open` succeeded under the real systemd unit — no
    "capture history log unavailable" fallback triggered.
  - Real success capture (`binning_2k`, no DNG) via `POST /api/capture`:
    produced `testshot-2k-binning-1789769627512.{jpg,log.json}` in
    `/mnt/capture`; `.log.json` content matched the request/outcome
    exactly; `optic_sync` drained both files within one poll cycle
    (`transferred_files: 2`, byte count matching jpg+log.json exactly);
    both arrived intact on the Mac receiver
    (`/Users/admin/Pictures/Optic/`).
  - Real failure capture (`master_archive` with `save_dng: false`, a raw
    validation error before the actor is even invoked): produced a
    synthetic-id `.log.json`
    (`testshot-master-archive-failed-1789769660161.log.json`, no paired
    image, as designed) that also synced to the Mac; SQLite row recorded
    `success = 0` with the exact `CameraError` message.
  - Restarted `optic-daemon.service` (`systemctl --user restart`) and
    confirmed both prior rows (success and failure) were still present
    afterward, and the service came back at the same version — proves
    `history.db` is on genuinely persistent storage, not accidentally
    under `/mnt/capture`'s tmpfs.
  - Test artifacts (the two real capture files + both `.log.json` files
    that landed in `/Users/admin/Pictures/Optic/`) were deleted after
    validation; they were synthetic test data, not meaningful capture
    history.
  - All four Pi-native checks from the original test plan now pass. The
    one item not separately exercised: a real dashboard-style
    date-range/average-duration query against the live database under
    load — deferred until the dashboard UI itself exists (§6 non-goal),
    though the same query shapes are already covered by the macOS unit
    test against the same schema.

## Remaining Limitations / Follow-ups

- Per-stage capture timing, downstream lifecycle tracking, system-context
  fields, and output-file checksums are not implemented (see design doc §6
  and worklog "Decisions Made" §3–§5) — candidates for a fast-follow, none
  blocking.
- The OverlayFS Phase 9 tradeoff (design doc §4, Option A) means this
  module's persistence will silently need revisiting if/when Phase 9 is
  ever enabled — flagged here so it isn't forgotten, not because it's an
  open task now.
- No dashboard UI reads this data yet (see §6 non-goal) — `history.db` and
  the synced `.log.json` files are populated and queryable, but nothing in
  the web UI surfaces them.
