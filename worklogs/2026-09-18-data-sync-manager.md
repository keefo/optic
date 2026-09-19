# Dated Worklog: 2026-09-18 - In-Process Data Sync Manager (`optic_sync`)

Status: implemented, tested locally (macOS, non-native target), and deployed
to the Raspberry Pi (`optic.local`) as `optic-daemon` 0.1.9. See
Implementation Summary and Validation below for what changed from this
worklog's original plan and how it was verified. Implemented autonomously
per explicit user direction ("use your best judgement to finish this plan")
while the user was away; **not yet committed to git** at the user's request
— they want to review the file changes themselves first.

## Objective

Replace the shell-script-based Phase 6 capture transfer (`scripts/optic-capture-transfer.sh`
+ systemd timer), which the user considers development/testing scaffolding,
with a real, in-process Rust subsystem (`optic_sync`) that runs inside
`optic-daemon` itself, plus a dashboard section showing its state and basic
controls.

## Current State (found while scoping this)

`docs/optic-daemon.md` section 6 ("Subsystem 3: Data Sync Manager") describes
a *target* architecture that does not match what is actually deployed and
verified today. Concretely:

- **What's actually running (Phase 6, verified 2026-09-16):**
  `scripts/optic-capture-transfer.sh` runs from an `optic-capture-transfer.timer`
  systemd unit every `OnUnitActiveSec=15s` (see `scripts/setup-phase-06-pi-ram-transfer.sh`).
  It `find`s every non-dotfile directly under `/mnt/capture` (`$STAGING_DIR`,
  no `queue/` subdirectory — capture files and `config.json`/`preview_config.json`
  live flat in the same directory) older than 10s, and for each one: computes
  size + SHA-256, opens an SSH connection
  (`ssh -T -p 2222 -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes
  -o UserKnownHostsFile=~/.ssh/optic_capture_known_hosts -i ~/.ssh/optic_capture_ed25519
  admin@192.168.0.231`) to a **restricted forced command**
  (`scripts/optic-capture-receiver.sh`, installed as the SSH `command=` for a
  capture-only key on this Mac), sends a `put <base64-name> <size> <sha256>`
  request line, streams the file over stdin, and only deletes the local copy
  after the receiver confirms the stored file's size+SHA-256 match (and after
  re-verifying the *local* file's inode/size/mtime/SHA-256 didn't change
  during the upload). The receiver writes to `/Users/admin/Pictures/Optic` via
  a temp file + atomic rename, with its own independent checksum/size
  re-verification (`MAX_FILE_BYTES=268435456` cap) and idempotent handling of
  already-received files (`OK existing ...`). There's also a `ping` operation
  for connectivity checks. This is solid and already validated in production
  — it is the actual current transport contract, not the doc's `scp`/`rsync`
  description.
- **What the doc describes but doesn't exist:** a `config.toml`-based schema
  (`[station]`/`[schedule]`/`[exposure]`/`[storage]` with `remote_host =
  "liam@imac.local"`, `remote_dir = "/Volumes/Archive/optic/frames"`), a
  `optic_scheduler` subsystem, boot-time config pull, and config mirroring
  back to a remote "master." None of this exists. The real config
  (`src/camera.rs::AppConfig`, persisted as `config.json`/`preview_config.json`
  in `/mnt/capture`) is just `{ profile, settings }`, edited only through the
  web UI's commit/discard endpoints — there is no scheduler and no
  remote-authoritative config concept today.
- **A real bug found while reading the shell script**: because the sweep
  matches *any* non-dotfile in `/mnt/capture`, `config.json` and
  `preview_config.json` are structurally eligible for transfer-then-delete
  once their `mtime` is >10s old. If that sweep runs between edits, the
  config file(s) get shipped to `/Users/admin/Pictures/Optic` (harmless
  clutter there) and deleted from `/mnt/capture` (the daemon's status handler
  falls back to `AppConfig::default()` when the file is missing — not a
  crash, but a silent, surprising reset of at-rest settings). Confirmed by
  reading `src/web.rs` (`config_path`/`preview_config_path` live directly in
  `capture_dir`, no exclusion) and `optic-capture-transfer.sh`'s `find`
  invocation. **The new Rust sync manager must not have this problem** — it
  needs an explicit allowlist for capture files, not "everything that isn't a
  dotfile."
- Capture files themselves are named `optic-web-<profile-slug>-<unique_suffix>.{jpg,dng}`
  (`src/native_camera.rs:1018`), written directly into `capture_dir` via
  hidden-temp-file + atomic rename (already safe against partial-file races
  from the sync manager's perspective).

## Scope (per explicit user direction)

- Fully replace the shell/timer transfer mechanism with a Rust module living
  inside `optic-daemon`, managed like the existing `optic_camera` actor
  (bounded FIFO channel, single owning task) — this repo already has one
  established concurrency pattern for a background subsystem and this should
  follow it rather than inventing a second one.
- Reuse the **existing, already-deployed, already-working** wire protocol
  (`ping`/`put` over a restricted-command SSH session, base64 filename +
  size + SHA-256, temp-file + rename + re-verify on both ends) instead of the
  doc's `scp`/`rsync` description, so `scripts/optic-capture-receiver.sh` on
  the Mac needs **no changes** and nothing about the trust/authorization
  model (dedicated capture-only SSH key, forced command, known_hosts pinning)
  changes. The Rust side reimplements the *client* half of that protocol.
- Add dashboard UI: a state panel (connectivity, queue depth, last
  transfer result/time, backoff state, current mode) and basic controls
  (pause/resume, trigger-now / reset-backoff).
- Out of scope for this worklog: `optic_scheduler`, `config.toml`, any
  remote-authoritative config concept, decommissioning the Phase 6
  timer/scripts from the Pi (that's a deployment/cutover step decided
  separately once the Rust version is verified — this worklog covers
  building and verifying it, not the cutover).

## Design

### Module shape

New `src/optic_sync.rs`, following the `optic_camera.rs` actor pattern:
a bounded `mpsc` command channel, one owning `tokio::task` that:
1. On startup, does nothing remote-authoritative (no boot-time pull —
   out of scope); just starts watching.
2. Watches `capture_dir` for new files via the `notify` crate (inotify on
   Linux; the crate already gives us a portable fallback for local macOS
   dev, matching how `optic_camera`/`native_camera` are split by
   `cfg(target_os = "linux")` elsewhere in this codebase).
3. On a new/changed file event *or* a periodic tick (belt-and-suspenders —
   inotify can theoretically miss events under extreme load; the Phase 6
   timer's simple polling never had that risk), lists `capture_dir`,
   filters to files matching the capture naming pattern (explicit allowlist:
   `optic-web-*.jpg` / `optic-web-*.dng`, never `config.json`/
   `preview_config.json` or dotfiles), and attempts to drain them in
   chronological (mtime) order — mirrors the shell script's FIFO behavior.
4. For each file: shells out to the system `ssh` binary (`tokio::process::Command`,
   not a pure-Rust SSH implementation — avoids pulling in a new crypto/SSH
   dependency for a protocol that already works via the OS's vetted OpenSSH
   client, consistent with how this project already shells out to
   `rpicam-still`-adjacent native code elsewhere) with the same options the
   shell script uses, computes SHA-256 via the `sha2` crate (new dependency)
   rather than shelling out to `sha256sum`, streams the file to the child's
   stdin, parses the one-line response, and only deletes the local file after
   both the local pre/post re-verification and the receiver's confirmation
   line, matching the shell script's safety properties exactly.
5. On any transport failure (SSH exits non-zero, timeout, connection
   refused), stops draining for this cycle and enters exponential backoff
   (30s, 60s, 120s, ... capped at 15m, matching the doc's stated numbers —
   reasonable and already documented, no reason to invent different ones).
   A successful `ping` or `put` resets backoff to the base interval.
6. Exposes a status snapshot (connectivity, queue depth/bytes, last
   attempt/success time and outcome, current backoff interval, paused
   flag) via the command channel, same style as `OpticCamera::status()`.
7. Accepts `pause`/`resume` and `retry_now` commands over the channel.

### Configuration

New environment variables, following the existing `OPTIC_BIND_ADDR` /
`OPTIC_CAPTURE_DIR` / `OPTIC_WEB_ASSETS_DIR` convention in `src/main.rs`
(all optional with sane defaults so local `cargo run` doesn't require a
real remote host — sync simply stays disabled/idle if unconfigured):
- `OPTIC_SYNC_REMOTE_HOST` (e.g. `192.168.0.231`)
- `OPTIC_SYNC_REMOTE_PORT` (default matches current `2222`)
- `OPTIC_SYNC_REMOTE_USER` (e.g. `admin`)
- `OPTIC_SYNC_IDENTITY_FILE` (default `~/.ssh/optic_capture_ed25519`)
- `OPTIC_SYNC_KNOWN_HOSTS_FILE` (default `~/.ssh/optic_capture_known_hosts`)
- `OPTIC_SYNC_ENABLED` (default `true` when a remote host is configured,
  otherwise the manager idles and reports `disabled` in its status rather
  than erroring — needed for local dev where none of this is set up)

These map directly onto today's `/etc/optic/capture-transfer.conf` values
(same key material, same host/port/user), so cutover doesn't require
provisioning anything new on either machine — just pointing the daemon's
systemd unit at the same identity file and known_hosts file the shell
script already uses.

### API additions (`src/web.rs`)

- Extend `GET /api/status`'s response with a `sync` object (state, queue
  depth/bytes, last result, backoff), analogous to the existing `camera`
  and `capture_stage` fields — the dashboard already polls `/api/status`
  every 3s, so no new polling loop is needed.
- `POST /api/sync/pause`, `POST /api/sync/resume`, `POST /api/sync/retry-now`
  — mirrors the existing `/api/stream/*` POST-action style.

### UI additions (`src/web/index.html` / `app.js` / `styles.css`)

A new panel (matching the existing `section`/`aria-labelledby` pattern used
by `preview-card`/`profile-card`/`controls-card`/`telemetry-card`) showing
connectivity state, queue depth, last transfer outcome/time, and
pause/resume + retry-now buttons wired to the new endpoints. Since asset
loading is already dynamic (no rebuild needed for HTML/CSS/JS-only changes,
per the dynamic-asset-loading work from earlier this session), this can be
iterated on quickly once the backend fields exist.

### New dependency

`notify` (directory watching) and `sha2` (checksum) added to `Cargo.toml`.
Both are small, widely-used, no native/system library requirements — unlike
`libcamera`/`turbojpeg`, this doesn't touch the pinned native sysroot or
build environment described in `docs/optic-daemon-build-environment.md`.

## Test Plan (written before implementation)

- Unit tests (run on macOS dev target, no camera/SSH required):
  - Capture-file allowlist filter: `optic-web-*.jpg`/`.dng` accepted,
    `config.json`/`preview_config.json`/dotfiles/arbitrary other files
    rejected — this is the direct regression test for the bug found above.
  - Backoff sequencing: failure advances 30s→60s→120s→...→cap at 15m;
    success resets to base.
  - Status snapshot shape/serialization.
  - Command handling (pause/resume/retry-now) mutates state as expected
    without needing a real SSH connection (transport itself mocked/injected
    behind a trait or function pointer, matching how `optic_camera`
    abstracts over backend kinds).
- Integration test on the Pi (native target, required environment — this
  cannot be validated on macOS since it needs the real SSH key, known_hosts
  pinning, and reachable receiver on this Mac):
  - Point `OPTIC_SYNC_*` env vars at the existing key/known_hosts/host used
    by Phase 6 today (no new provisioning), drop a real file into
    `/mnt/capture`, confirm it's transferred to `/Users/admin/Pictures/Optic`
    and removed locally, with checksums matching, same as the existing
    `optic-capture-transfer.sh` validation record in `docs/optic-daemon.md`.
  - Confirm `config.json`/`preview_config.json` are left alone across
    multiple sync cycles (the bug fix).
  - Simulate a network failure (stop the receiver / block the port) and
    confirm files accumulate safely in `/mnt/capture` without being lost,
    backoff increases, and `/api/status`'s `sync` field reflects it.
  - Confirm `pause`/`resume`/`retry-now` work via `curl` against the live
    endpoints and reflect immediately in `/api/status`.
  - Dashboard: visually confirm the new panel renders and updates, and that
    pause/resume/retry-now buttons work from the browser.
- Explicitly **not** validated as part of this feature: decommissioning
  `optic-capture-transfer.timer` on the Pi. Both mechanisms can coexist
  during verification (they operate on disjoint file sets once the Rust
  side's allowlist excludes config files — worst case is a capture file
  raced between the two, which is safe by construction since both sides
  verify-then-delete). Cutover (disabling the old timer) is a explicit,
  separate, user-approved deployment step once this is verified, not
  bundled into this worklog.

## Remaining Open Questions (before implementation starts)

1. Confirm the env-var-based config approach above (vs., e.g., reading the
   existing `/etc/optic/capture-transfer.conf` directly for zero-touch
   cutover) is what's wanted.
2. Confirm reusing the exact existing `ping`/`put` wire protocol (vs.
   designing a new one) — this worklog assumes reuse since the receiver is
   already deployed, tested, and intentionally restrictive (forced command,
   size cap, checksum re-verification), and changing it would require
   redeploying the receiver side on this Mac too.
3. Confirm scope of "basic control" in the UI: pause/resume + retry-now
   covers the doc's described failure modes; flag if more (e.g. a manual
   "skip this file" / clear-queue action) is wanted.

Answered by the user before implementation started: proceed with the
in-process Rust rewrite as scoped, resolving the three questions above using
best judgement (user was AFK) — all three were implemented exactly as
proposed (env-var config, exact existing wire protocol reused unchanged,
pause/resume/retry-now as the full control surface). See Implementation
Summary below for the one real design deviation made along the way
(polling instead of `inotify`) and why.

---

## Implementation Summary

New module `src/optic_sync.rs` (~500 lines including tests), wired into
`src/main.rs` and `src/web.rs`. Mirrors the `optic_camera` actor shape:
`DataSyncManager` is a cheap `Clone` handle (an `mpsc::Sender` for commands +
a `watch::Receiver` for status) around a single owning `tokio::task`.

**Deliberate deviation from the plan: polling, not `notify`/`inotify`.**
The original plan called for the `notify` crate. Implemented instead as a
plain `tokio::time::interval` tick every 5 seconds that lists `capture_dir`
and filters to capture files. Reasoning: this was being built and deployed
in one autonomous pass with no opportunity for iterative on-hardware
debugging of an inotify-to-tokio bridge, the polling approach is simpler and
has a much smaller failure surface, it's *still* faster to notice a new file
than the Phase 6 timer's existing 15s interval, and directory-listing a
handful of files every 5s is negligible overhead on a Pi 5. `notify` was
**not** added to `Cargo.toml`. Flagged here explicitly as a scope decision
made unilaterally — real `inotify` watching (for near-zero latency, e.g. if
5s ever proves too slow) is a clean, isolated follow-up if wanted: it would
only touch the `tokio::select!` arm in `run_actor`, nothing about the
public API, status shape, or protocol.

**Everything else matches the plan:**
- `is_capture_filename` allowlists exactly `optic-web-*.jpg` / `optic-web-*.dng`
  — the direct fix for the config-file-deletion bug found while scoping this.
- Transport shells out to the system `ssh` binary (`tokio::process::Command`)
  with the same options, forced-command protocol (`put <base64-name> <size>
  <sha256>`), and response format (`OK stored ...` / `OK existing ...`) as
  `scripts/optic-capture-transfer.sh`, so `scripts/optic-capture-receiver.sh`
  needed **zero changes**. SHA-256 via the new `sha2` dependency; base64 is a
  ~20-line inline RFC 4648 encoder (avoided a crate for something this small
  and easily unit-tested against the RFC's own test vectors).
- Before deleting a transferred file, re-checks its size and mtime against
  what was captured at listing time (cheaper analogue of the shell script's
  inode/size/mtime/SHA re-verification) — in practice this should never
  actually fire, since `native_camera.rs` writes captures via a hidden
  temp file + atomic `rename`, so a filename is never visible until fully
  written; kept as cheap insurance rather than a load-bearing guarantee.
- Exponential backoff: 30s → 60s → 120s → 240s → 480s → capped at 900s (15m),
  reset on any successful full drain or an operator `retry-now`. Verified by
  unit test stepping through all seven transitions including the cap.
- Config resolution (`src/main.rs::resolve_sync_config`): `OPTIC_SYNC_REMOTE_HOST`
  is the only required variable (its absence is a normal, silent `disabled`
  state — this is what makes local `cargo test`/`cargo run` work without any
  SSH setup); `OPTIC_SYNC_ENABLED=false` force-disables regardless.
  `OPTIC_SYNC_REMOTE_PORT`/`_USER`/`_IDENTITY_FILE`/`_KNOWN_HOSTS_FILE`
  default to the exact values `scripts/setup-phase-06-pi-ram-transfer.sh`
  already provisions on the Pi (port `2222`, user `admin`,
  `~/.ssh/optic_capture_ed25519`, `~/.ssh/optic_capture_known_hosts`), so
  `systemd/optic-daemon.service` only needed one new line
  (`Environment=OPTIC_SYNC_REMOTE_HOST=192.168.0.231`) to enable it against
  the exact same receiver Phase 6 already talks to — no new key generation,
  no new `authorized_keys` entry, no changes to
  `scripts/optic-capture-receiver.sh`.
- API: `sync` object added to `GET /api/status`'s response (connectivity,
  paused, queued/transferred file counts and bytes, last error, backoff
  seconds, seconds-until-retry computed live from a stored `SystemTime`
  rather than going stale between polls). `POST /api/sync/{pause,resume,retry-now}`,
  each returning the refreshed status; a `SyncUnavailable` → `AppError`
  mapping (503) added for the case the actor already shut down, mirroring
  the existing `CameraError` → `AppError` pattern.
- UI: new "Data Sync" panel in `src/web/index.html` (a `<section>` sibling
  of the telemetry card, same `aria-labelledby`/`dl`/`pill`/`button-row`
  conventions as the rest of the page — needed **zero new CSS**), with
  `app.js`'s `renderSync()` mapping connectivity/paused into a status pill
  and the three buttons wired via a small `syncAction()` helper that POSTs
  and immediately re-polls status.
- **One bug found and fixed during manual local testing** (not in the
  original plan): `retry-now` cleared the backoff timer fields but left the
  displayed `connectivity` at `"backoff"` until the next scan tick, which
  could show a stale "Retrying" pill with no countdown for a few seconds.
  Fixed by having `exit_backoff()` also reset `connectivity` to `Idle` when
  it was `Backoff`; added a unit-test assertion for it.
- `docs/optic-daemon.md` section 6 rewritten: the original target design
  (boot-time `config.toml` pull, `scp`/`rsync`, `inotify`) is kept under an
  "As Originally Designed (not implemented)" subheading for historical
  reference, with a new "As Implemented" subheading describing the above.
  The top-level implementation-status callout updated accordingly.

## Validation

**Local (macOS, non-native `cargo` target — no libcamera, so this validates
only the sync/web layers, same caveat as every other worklog in this repo):**
- `cargo build` / `cargo test` — 24/24 tests pass, including 8 new
  `optic_sync` tests: base64 RFC 4648 vectors, capture-filename allowlisting
  (explicitly asserting `config.json`/`preview_config.json` are rejected —
  the regression test for the bug that motivated the allowlist),
  oldest-first FIFO ordering, queue-count accuracy, full backoff sequencing
  through the cap plus the `retry-now` connectivity-reset fix, a
  disabled-manager smoke test, and a pause/resume status test.
- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`
  — both clean.
- Manual runtime testing against the real local binary (not just unit
  tests): confirmed the `disabled` path end-to-end (`OPTIC_SYNC_REMOTE_HOST`
  unset → `/api/status` reports `"connectivity": "disabled"`,
  pause/resume/retry-now all return `200` harmlessly). Confirmed the
  `enabled`-but-unreachable path with a real queued file and a deliberately
  unreachable host/port/identity file: `/api/status` showed
  `queued_files: 1`, `connectivity: "backoff"`, `backoff_secs: 30`,
  `next_retry_in_secs` counting down, and `last_error` containing the
  actual `ssh` stderr; confirmed `retry-now` clears backoff and triggers an
  immediate (failing, since still unreachable) re-attempt; confirmed
  `pause` stops attempts (file stays queued, no new backoff) and `resume`
  restarts them. Confirmed the dashboard HTML/JS wiring end-to-end via
  `curl` (panel markup present, `renderSync`/`syncAction` present in the
  served `app.js`, all three POST endpoints return `200`).
- Re-ran the Pi's Biome binary against the final `app.js`/`index.html`
  (same workflow established in `worklogs/2026-09-17-dynamic-asset-loading.md`):
  one formatter-only diff from the new `renderSync` code, applied and
  re-verified clean before deploying.

**On the Pi (native `aarch64-unknown-linux-gnu` target, via
`./scripts/build-deploy-optic-daemon.sh`):**
First deploy attempt (as `0.1.9`) built, tested (32/32 `cargo test`, including
all `optic_sync` tests against the real native target), linted, and installed
cleanly — but live capture transfer failed with an uninformative `Broken
pipe (os error 32)`. Manually replicating the exact same `ssh`/`put`
invocation from an interactive `liam` shell on the Pi worked perfectly
(`OK stored ...`), which ruled out the protocol/credentials/receiver and
pointed at something specific to the daemon's own execution context.

Root-caused by first fixing the error handling (`0.1.10`): when the stdin
write to `ssh` fails, the code now still waits for and captures the child's
exit status/stderr instead of surfacing a bare I/O error. Redeploying with
just that diagnostic fix revealed the real cause: `Bad owner or permissions
on /etc/ssh/ssh_config.d/20-systemd-ssh-proxy.conf` — OpenSSH's built-in
config-file ownership/permission check rejecting a file that is, in fact,
perfectly normal (`root:root 0644`, confirmed via `stat -L` on the Pi) when
viewed from an ordinary shell. This only reproduces inside
`optic-daemon.service`'s own `ProtectSystem=strict` mount-namespace sandbox,
most likely an interaction between that sandboxing and the Pi's OverlayFS
root (`README.md`'s "read-only root safeguards (OverlayFS)") changing how
that file's permissions are exposed inside the private mount namespace —
plausible but not conclusively proven; a `systemd-run --user --scope`
reproduction attempt with matching `-p` properties was tried to isolate it
further without a full redeploy cycle, but `ProtectSystem` isn't settable on
transient scope units, so it wasn't pursued further given the fix below is
correct regardless of the exact mechanism.

Fixed (`0.1.11`, final) by adding `-F /dev/null` to the `ssh` invocation in
`put_file`, which makes OpenSSH skip system *and* user `ssh_config`
discovery entirely — reasonable regardless of root cause, since every
option this client needs is already passed explicitly via `-o`, and it's
generally good practice for a restricted automated script not to depend on
ambient host SSH configuration. Verified fine from an interactive shell
first (still connects and transfers correctly with `-F /dev/null` added),
then confirmed the real fix by redeploying and checking `/api/status` on
the live Pi: `connectivity` went from `backoff` straight to `idle`, and all
4 files that had been stuck in the queue since the `0.1.9` attempt (~60 MB
total, two Master Archive JPEG+DNG pairs left over from earlier manual
testing) were transferred and removed from `/mnt/capture` within one poll
cycle.

**Bonus fix found during this same investigation:** noticed `queued_files`/
`queued_bytes` in `/api/status` stayed at their pre-drain values
immediately after a successful drain (only refreshing on the *next* 5s
tick) — confirmed by querying `/api/status` right after the successful
drain above and seeing `queued_files: 4` alongside `transferred_files: 4`
simultaneously. Fixed by calling `update_queue_counts` again immediately
after `drain_cycle` in the actor loop, not just before it. Confirmed fixed
by re-querying a few seconds later (`queued_files: 0`) and by the
end-to-end test below showing `queued_files: 0` throughout.

**Final end-to-end production test** (after all three fixes, at the final
deployed `0.1.11`): triggered a real capture through the live dashboard's
own API (`POST /api/capture`, `binning_2k` profile, no DNG — chosen to be
fast/small) rather than using a pre-staged test file, to exercise the
actual capture → queue → sync pipeline exactly as an operator would trigger
it. The file (`optic-web-2k-binning-1789721261540.jpg`, 93,712 bytes)
appeared in `/mnt/capture`, was picked up and transferred **automatically,
with zero manual intervention**, within one ~5s poll cycle
(`transferred_files` incremented from `0` to `1`, `queued_files` stayed at
`0` throughout — confirming the queue-refresh fix), and was independently
verified present and byte-for-byte correct at
`/Users/admin/Pictures/Optic/optic-web-2k-binning-1789721261540.jpg` on
this Mac (SHA-256 spot-checked, matches).

Also re-verified after all fixes: `cargo test` on the Pi's native target —
32/32 passing (same set as the first deploy attempt, unaffected by the
`ssh`-invocation and queue-refresh fixes since those aren't covered by unit
tests — they were only caught by live testing, which is exactly why this
worklog's test plan called for Pi-side integration testing in addition to
unit tests). `cargo fmt --all -- --check` and
`cargo clippy --all-targets -- -D warnings` clean throughout every
iteration. Biome clean throughout (only the one formatter fix already
described, applied before the first deploy attempt).

Not tested: the `pause`/`resume`/`retry-now` buttons from an actual browser
(only verified via `curl` against the live endpoints, both locally against
a synthetic backoff scenario and against the real deployed Pi); the
dashboard panel's visual appearance in a browser at all — everything above
was verified via `curl`/API responses, not by loading
`http://optic.local:8000/` in a browser and looking at it.

## Remaining Limitations / Risks / Next Steps

- **Phase 6 (`optic-capture-transfer.timer`) is still enabled and running in
  parallel on the Pi.** This was intentional per the original scope (both
  are safe to coexist — `optic_sync`'s allowlist only ever touches
  `optic-web-*.jpg`/`.dng`, and both sides verify-then-delete), but it means
  captures are now typically raced by two independent transfer mechanisms,
  and Phase 6's 15s sweep still has the config-file-sweep bug this worklog
  found (it just hasn't been observed to actually cause harm here since
  `optic_sync` tends to win the race on capture files, but nothing prevents
  Phase 6 from grabbing `config.json` on any given cycle). Decommissioning
  Phase 6 is an explicit, separate, user-approved deployment step — not done
  here, and worth doing soon given the bug it carries.
- **The `-F /dev/null` root cause isn't fully proven**, only worked around.
  If this Pi's OverlayFS/`ProtectSystem=strict` interaction is a real,
  reproducible systemd/kernel quirk, it could in principle affect other
  file paths the daemon reads under the same sandbox — nothing else has hit
  it so far (the daemon already reads `capture_dir`, the web asset
  directory, and the sysroot's shared libraries without issue), but it's
  worth keeping in mind if a similarly strange "works interactively, fails
  under the service" report shows up again.
- **No real `inotify`** — see the Implementation Summary's "deliberate
  deviation" note. 5-second polling is working correctly in production
  (confirmed by the end-to-end test above, which saw the file within one
  cycle), but true event-driven watching remains a clean, isolated future
  improvement if 5s ever proves insufficient.
- **Dashboard not visually verified in a browser** — see above. Functionally
  confirmed via direct API calls, but nobody has looked at the actual
  rendered "Data Sync" panel yet.
- **`optic_scheduler` and any remote-authoritative `config.toml` remain
  unimplemented**, as scoped from the start — `optic_sync` has no config to
  mirror because nothing produces one yet.
- Two of the four files that got stuck in the queue during the first
  (`0.1.9`) deploy attempt were **Master Archive JPEG+DNG pairs from
  manual testing earlier in this session** (not real timelapse captures);
  they were transferred to `/Users/admin/Pictures/Optic` as part of
  verifying the fix and are harmless there, but the user may want to
  clean them up (`optic-web-master-archive-1789713405041.*`,
  `optic-web-master-archive-1789713450956.*`) along with the one
  `binning_2k` test capture from the final end-to-end test
  (`optic-web-2k-binning-1789721261540.jpg`).

## Not Committed

Per explicit user instruction, none of the above has been `git add`ed or
committed. `git status`/`git diff` on `src/optic_sync.rs` (new),
`src/main.rs`, `src/web.rs`, `src/web/index.html`, `src/web/app.js`,
`Cargo.toml`, `Cargo.lock`, `systemd/optic-daemon.service`,
`docs/optic-daemon.md`, and this worklog (new) are all sitting as working-tree
changes for the user to review before deciding whether/how to commit.

## Follow-up: dashboard field expansion (`0.1.12`)

After reviewing the live dashboard, the user asked why "Next retry" always
showed `—` (answer: it's `null` by design whenever `connectivity` isn't
`backoff` — nothing to retry when transfers are succeeding, which they were)
and asked to also clarify whether scanning is file-change-triggered (it's
not — see the "deliberate deviation" note above, it's a plain 5s poll).

Requested and implemented:
- `SyncStatus` gained `next_scan_in_secs`: `ActorState` now tracks
  `last_scan: Option<SystemTime>`, set at the top of every actual poll
  tick (not on command-only loop iterations); `next_scan_in_secs` is
  computed the same way `next_retry_in_secs` already was (relative to
  `now()` at query time, so it doesn't go stale between polls, and is
  `None` while disabled since a disabled manager never scans). This is an
  estimate — a slow scan/drain pushes the real next tick later — not a
  precise deadline; documented as such in the code.
- Dashboard "Data Sync" panel's `dl` expanded from 3 rows (Queued,
  Transferred, Next retry) to 7: added raw `Connectivity`, `Backoff`,
  `Next scan`, and `Last error` rows, so the actual field values are
  visible directly rather than only folded into the single friendly status
  pill. The separate `<p id="sync-error">` notice paragraph was removed in
  favor of the new `Last error` row (single source instead of two places
  showing the same thing).
- Unrelated small fix requested in the same pass: the global
  `button:disabled` rule used `cursor: wait`, which implies an in-progress
  operation rather than "unavailable right now" — changed to `cursor: auto`
  in `src/web/styles.css`. Applies to every disabled button on the page,
  not just the sync ones.

Verified: `cargo build`/`test` (24/24)/`fmt`/`clippy` clean; Biome clean
against the updated `app.js`/`index.html`; local runtime test confirmed
`next_scan_in_secs` present and correctly `None` while disabled, and
counting down (`3` → `0` observed) while enabled. Bumped to `0.1.12` and
redeployed; confirmed live on the Pi — `/api/status` returns
`next_scan_in_secs`, the new `dl` rows and their ids are present in the
served `index.html`, and `styles.css` serves the corrected
`cursor: auto` rule. Not yet visually confirmed in an actual browser (same
caveat as the rest of this worklog).

