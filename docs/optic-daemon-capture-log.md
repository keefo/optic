# Design Document: Capture History Log (`optic_capture_log`)

> **Status:** Implemented, unit-tested, and deployed
> (`src/optic_capture_log.rs`, `optic-daemon` 0.1.16). Hardware-validated
> on the Pi: real success and failure captures produced correct
> `.log.json` files and SQLite rows, `optic_sync` transferred both to the
> Mac receiver, and `history.db` survived a service restart. This is the
> design reference for the 4th daemon subsystem (see `docs/optic-daemon.md`
> section 1). Implementation history lives in
> `worklogs/2026-09-18-capture-history-module.md`; this doc is the living
> design record, updated as decisions are made or the design changes,
> independent of any single dated worklog entry.

## 1. Problem Statement

Today, a capture's outcome is only ever visible transiently — in the
`tracing` output added during the [capture-performance investigation](optic-daemon-capture-performance.md),
which doesn't even persist anywhere reliable (journald has no persistent
storage on this Pi; see that doc's §2.2). There is no record of *what*
capture happened, with *what* settings, how long it took, or whether it
succeeded, once the moment passes. This module exists to fix that: a
durable, queryable record of every capture request the daemon ever
processes.

Motivating use cases (from design discussion):
- "How many photos were taken on the first Monday of last month?"
- "What has capture time looked like over the last year — is there
  performance degradation (e.g. thermal, SD card wear, sensor drift)?"
- General debugging: correlate a specific output file with exactly what
  request produced it and how long each stage took, without needing a live
  `tracing` session running at the time.

## 2. Scope

`optic_capture_log` is a **passive observer**, not a participant in the
capture path. It has no camera access, no network access, and cannot cause
a capture to succeed or fail — it only records what already happened,
learned from the same `CaptureRequest`/`CaptureResult`/error information
that already flows through `OpticCamera::capture_to_stage` and the `web.rs`
handler that calls it. This mirrors how `optic_sync` has "zero awareness of
camera state or exposure logic" — this module has zero ability to *affect*
either, only to watch.

### What gets recorded, per capture

**Request identity/provenance**
- Unique capture ID (correlates the log file, the SQLite row, and the
  output JPEG/DNG filenames — see §3.1 on the shared-basename convention)
- Requester/source: `web_ui` today; `scheduler` once `optic_scheduler`
  exists (see `worklogs/2026-09-18-timelapse-scheduler.md`)
- Trigger detail (manual click vs. scheduled tick vs. retry, once those
  distinctions exist)

**Capture configuration**
- Profile, full `CameraSettings` (rotation/flip/awb/metering/exposure/ev/
  gain/shutter_us/denoise), `save_dng`
- Whether these matched the currently-committed `config.json` or differed

**Timing/performance** (v1, as implemented) — requested-at / completed-at
timestamps and total `duration_ms`, taken from the same `Instant` the
`http_handler_total` trace already uses in `web.rs::capture()`, so the
persisted number matches what's already visible live.

**Deferred to a fast-follow** (see worklog "Decisions Made" §3, §5): the
full per-stage breakdown that already exists as `tracing::info!("capture
perf", stage = ..., elapsed_ms = ...)` events inside `native_camera.rs`
(`stop_existing_preview`, `pipeline_start`, `warmup_and_capture`,
`camera_stop`, `jpeg_encode`, `dng_decode_and_encode`, `disk_write` — see
`docs/optic-daemon-capture-performance.md` §2.1) is **not** persisted by
v1. Capturing it here would mean threading those per-stage numbers back
through `OpticCamera`'s actor protocol into `web.rs`, which v1 deliberately
avoided touching. Likewise, actor queue wait time (time between the HTTP
handler enqueuing the request and the native worker picking it up) is not
currently measured anywhere and is deferred for the same reason.

**Outcome**
- Success/failure; on failure, which `CameraError` variant and message

**Output artifact metadata** (v1: filenames, byte sizes, actual resolution
produced, whether DNG was included — captured today; checksum is not, see
below)
- Filenames, byte sizes, actual resolution produced, whether DNG was
  included
- **Deferred:** checksum (would enable cross-referencing with what
  `optic_sync` later confirms it transferred) — not computed in v1, since
  `optic_sync` already independently hashes each file for its own
  transfer-integrity check (§3.1's transport reuse); adding a second
  SHA-256 pass here was judged not worth the extra capture-path latency
  until there's a concrete need to cross-reference the two.

**Downstream lifecycle — deferred, v1 non-goal** (see §6). Would require
light coordination with `optic_sync` to know when a given file was
actually drained/transferred:
- Time spent queued in `/mnt/capture` before `optic_sync` drained it
- Transfer outcome and final disposition

**System context at capture time — deferred, v1 non-goal.** None of these
are captured yet; each would need a new read at the hook point in
`web.rs::capture()` (cheap individually, but scoped out of v1 to keep the
first cut small — see the worklog):
- Free space in `/mnt/capture` before/after (relevant given the tmpfs-fill
  risk already flagged in the Data Sync Manager and Scheduler design docs)
- Was live preview active immediately before this request
- Daemon uptime, detected sensor/backend

## 3. Storage Design

Two complementary, deliberately redundant mechanisms, decided after working
through the tradeoffs of several alternatives (embedding data directly in
JPEG/DNG metadata; a single shared batched JSONL log) — both were rejected
in favor of this combination:

### 3.1 Per-capture log file (paired with the JPEG/DNG, synced to the Mac)

One small JSON file per capture, using the **same basename** the JPEG/DNG
already use (`testshot-<profile>-<suffix>`, per `native_camera.rs`), e.g.:

```
testshot-master-archive-1789753359227.jpg
testshot-master-archive-1789753359227.dng
testshot-master-archive-1789753359227.log.json
```

This makes the association between a capture's three artifacts trivial and
filename-visible, which was the explicit preference over a shared/batched
log file (clarity over marginally less sync overhead). It's written to
`/mnt/capture` alongside the image files and gets picked up by
`optic_sync`'s existing allowlist-and-drain loop exactly like they do —
`is_capture_filename` in `src/optic_sync.rs` is extended to also match the
`.log.json` suffix. No new transfer protocol work: the same `ping`/`put`
mechanism, same checksum verification, same receiver script, unchanged.

**Failure case (not illustrated by the example above):** a failed capture
produces no JPEG/DNG, so there's no basename to share. `capture_id` is
instead a synthetic id, minted the same way `native_camera.rs`'s
`unique_suffix()` works (monotonic-with-wall-clock, via an atomic counter)
but independently, since that function lives in Linux-only code:
`testshot-<profile-slug>-failed-<unix-ms>.log.json`, e.g.
`testshot-master-archive-failed-1789753412001.log.json`.

**Implementation note — how `capture_id` is actually derived (v1):** rather
than generating a `capture_id` in `web.rs` and threading it down through
`OpticCamera`'s actor protocol into `native_camera.rs::publish_capture` so
the real image basename could be built from it, v1 derives `capture_id`
*after the fact* — on success, straight from `result.files[0].filename`
(stripping the extension); on failure, via the synthetic id above. This
gets the same shared-basename outcome with zero changes to the actor
protocol or to `native_camera.rs` (the real-hardware capture path, not
buildable/testable on a non-Linux dev machine). See the worklog's
"Decisions Made" §3 for the full reasoning.

Rejected alternative — embedding this data directly into the JPEG/DNG's own
metadata instead of a separate file: technically feasible for both formats
(DNG is a homegrown TIFF writer already, so a custom private tag is easy;
JPEG would need manual marker-segment splicing around the `turbojpeg-sys`
output). Rejected because a failed capture produces **no** image file to
embed into — exactly the records most worth keeping — and because
retention policy for images and for history data are plausibly different
(you might want to keep performance history indefinitely while eventually
pruning old images), which embedding entangles.

### 3.2 Local SQLite database (Pi-side, for fast dashboard queries)

The per-capture log files alone don't support the motivating use cases in
§1 efficiently — "count by date" and "trend over a year" are exactly what
indexed SQL does well and what scanning N JSON files does badly, especially
on a 1GB Pi. So, additionally: one row per capture in a local SQLite
database.

- **Path:** `~/.local/state/optic-daemon/history.db` — a sibling of
  `~/.local/bin/` (which holds the binary and web assets), deliberately
  *not* inside `~/.local/bin/` itself, to stay clear of
  `scripts/setup-optic-daemon-phase-01.sh`'s install/cleanup logic (which
  currently only touches the binary file and wholesale replaces the `web/`
  subdirectory).
- **Mode:** WAL (`PRAGMA journal_mode=WAL`). This is a crash-safety
  decision, not (only) a wear optimization — unattended 365-day operation
  means an abrupt power loss is a realistic event, and WAL substantially
  reduces the chance of a corrupted database file from a write interrupted
  mid-transaction.
- **Schema (finalized, as implemented in `src/optic_capture_log.rs`):**
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
  Structured columns cover every §1 motivating query (date-range counts,
  duration trend, success rate) with simple `WHERE`/`GROUP BY`/`AVG`
  queries; `detail_json` keeps full per-capture fidelity (settings, full
  entry) without widening the table for fields that don't need indexing.
- **Filesystem/sandboxing requirement — resolved.** Confirmed live on the
  deployed Pi (2026-09-18) that the root filesystem is currently plain
  `ext4 rw` (not yet OverlayFS-read-only — see §4), so this path is
  genuinely writable at the OS level today. The daemon's own
  `systemd/optic-daemon.service` sandboxing (`ProtectSystem=strict` +
  `ProtectHome=read-only`) is carved open for this path via
  `ReadWritePaths=/mnt/capture /dev/shm %h/.local/state/optic-daemon`.
  Since `ReadWritePaths=` targets must already exist before the unit
  starts, `scripts/setup-optic-daemon-phase-01.sh` creates
  `~/.local/state/optic-daemon` (mode `0700`) during install, alongside its
  existing `~/.local/bin`/systemd-unit-directory setup.

Mac-side durability for this data was the original motivating request, but
is now satisfied primarily through §3.1 (the per-capture log files are
already synced via the existing `optic_sync` pipeline) rather than by
syncing the SQLite file itself. Periodically shipping a SQLite backup/
snapshot to the Mac as well remains a possible future addition (SQLite has
its own backup/merge tooling — `VACUUM INTO`, or `ATTACH` + `INSERT
SELECT`) but isn't required for the Pi-side query use case and isn't
planned for the initial implementation.

## 4. Decision: OverlayFS Phase 9 Compatibility

This project's stated long-term design (`README.md`, `verify.sh` Phase 9)
is to make the Pi's root filesystem read-only via OverlayFS specifically to
protect against corruption over a year of unattended operation. **That has
not been enabled on the live deployment as of this writing** — root is
plain writable ext4. If it ever is enabled, a database file living under
`/home/liam/.local/state/` would stop being writable the same way the rest
of `/` would, unless it's deliberately carved out as its own writable
mount — the same way `/mnt/capture` already is (a separate tmpfs, not part
of the overlay at all).

**Decided: Option A — design for today's state only.** The `ReadWritePaths=`
entry described in §3.2 is the whole implementation; no dedicated writable
partition was provisioned. Rationale: the live Pi is plain writable ext4
and Phase 9 has no scheduled enablement date, so building a dedicated
mount now would be speculative work against a timeline that doesn't exist
yet. This is a traded-off decision, not a silent one: if Phase 9 is ever
enabled, `history.db` writes will start failing — `CaptureLog::open`
already logs a warning and disables history recording gracefully rather
than crashing the daemon (see `src/main.rs`), so the failure mode is loud
and non-fatal, but the feature itself would go dark until this is
revisited (likely by moving to Option B — a dedicated writable
partition/bind-mount for `~/.local/state/` and `~/.local/bin/` together —
at whatever point Phase 9 is actually scheduled).

## 5. Relationship to Other Subsystems

- **`optic_camera` / `optic_web`:** the source of the events this module
  records. **No changes to `optic_camera`'s actor protocol** —
  `optic_capture_log` observes the same `&Result<CaptureResult,
  CameraError>` the HTTP handler already has, via a call to
  `CaptureLog::record` right after `web.rs::capture()` awaits the actor's
  reply. The actor itself has no awareness this module exists.
- **`optic_sync`:** transports the per-capture `.log.json` file exactly
  like it transports JPEG/DNG files (§3.1). `is_capture_filename` is
  extended to match the `.log.json` suffix; no protocol changes.
- **`optic_scheduler`:** once implemented, becomes a second source of
  capture events alongside the web UI, distinguished via the
  requester/source field (§2, currently hardcoded to `"web_ui"`). No
  dependency in the other direction — this module doesn't need the
  scheduler to exist first.

## 6. Non-Goals (v1)

- No remote/Mac-side merged history database (see §3.2's closing note).
- No UI work yet — this doc covers the data layer only. The dashboard
  panel for querying this history is a separate, later effort.
- No dedicated writable partition for OverlayFS Phase 9 survival (§4,
  Option A decided) — a known, accepted, revisit-later tradeoff, not an
  oversight.
- No per-stage capture timing breakdown persisted (§2) — only total
  `duration_ms`. The full `native_camera.rs` stage-by-stage trace remains
  `tracing`-only for now.
- No system-context fields (§2: free space, preview-active flag, uptime,
  sensor/backend) and no output-file checksum (§2) — scoped out to keep
  the first cut small; none require an actor protocol change to add
  later.
- No downstream lifecycle tracking (§2's "downstream lifecycle" field
  group) — coordinating `optic_sync` drain/transfer status back into a
  capture's history row would add a second writer to the same row and a
  new coupling between two independently-designed subsystems; deferred
  until the simpler parts of this module have proven useful in practice.
- No changes to `optic_camera`'s or `NativeCameraBackend`'s existing actor
  protocol, and no changes to `native_camera.rs` at all.
