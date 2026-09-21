# Dated Worklog: 2026-09-18 - System Control Panel (Pi Management via Web Dashboard)

Update 2026-09-20: the Reboot button (B) was later fixed and
user-verified on real hardware (`worklogs/2026-09-19-reboot-nonewprivileges-fix.md`).
No worklog records a verified press of the Restart-daemon button, so that
part remains untested. Dashboard auth is still deferred. The original
status follows.

Status: **A (read-only health) implemented, deployed (`optic-daemon`
0.1.21), and empirically verified correct on real hardware** across three
bug-fix rounds (see Validation). **B (reboot/restart-daemon) implemented
and deployed, functionally untested** — pending the user exercising the
dashboard buttons directly, or explicit go-ahead to trigger them
programmatically (`reboot` is disruptive). Dashboard-level auth
deliberately deferred per explicit user decision ("we do auth later" —
Open Question 2, not resolved, not defaulted).

## Decided Scope (v1)

**A — read-only health**, all surfaced through one new endpoint,
`GET /api/system/status`, polled independently from the existing 3s
`/api/status` loop (see Open Question 4 — resolved: separate, slower poll
rather than adding expensive-to-gather fields to the hot path):
- Memory (total/available)
- Disk usage for root `/`, `/mnt/capture`, and the capture-log state dir
- Uptime
- CPU temperature (Linux/`vcgencmd` only — `None` on macOS dev target)
- Network interfaces + addresses
- Capture health rollup (last 24h): success rate, last capture time/
  outcome, average duration — the first consumer of `optic_capture_log`'s
  SQLite DB, per that module's own §6 non-goal note

**B — actionable controls**, v1 scope narrowed to the two concrete,
low-complexity items:
- `POST /api/system/reboot` — reboots the Pi (`sudo systemctl reboot`)
- `POST /api/system/restart-daemon` — restarts just `optic-daemon.service`
  (`systemctl --user restart`, no sudo needed — it's the user's own
  service)

**Explicitly deferred out of B, not part of this pass:** dashboard log
viewing. It needs the daemon to start writing its own rotating log file
(journald has neither persistence nor working live-follow on this Pi,
confirmed during the capture-performance investigation) — meaningfully
different, bigger scope than the other two B items, which are one-shot
commands. Tracked as a fast-follow, not folded in here.

**Sudo scoping — decided, not narrowed.** Implementing reboot requires some
passwordless-sudo path. The existing grant (`/etc/sudoers.d/liam-nopasswd`,
blanket `NOPASSWD: ALL`, added earlier today for one narrow
WiFi-debugging task) already covers it. I offered to narrow it to exactly
`sudo systemctl reboot` as a hygiene improvement (a strict reduction of
existing privilege); attempting that change was itself blocked by the
permission system as an unattended system-persistence edit, which is
correct — it's exactly the kind of change that should get an explicit
human decision rather than happen automatically. Asked the user directly;
their answer: keep the blanket grant, they don't mind the wider
permission. Not revisiting this unless they ask.

## Incidental Finding: Hardware Watchdog Was Causing "Mysterious" Reboots

While deploying this feature, the Pi rebooted unexpectedly mid-build twice
in a row (the user confirmed neither was intentional the second time).
Root-caused via `dmesg`: `systemd[1]: Watchdog running with a hardware
timeout of 15s` — the Broadcom BCM2835 SoC's hardware watchdog, managed by
systemd (`RuntimeWatchdogUSec=15s`), resets the board instantly and
silently if PID 1 can't "pet" it in time. Heavy parallel compilation
(`rayon`/`bindgen`/`crossbeam`/`sysinfo` all building at once on this
990MB-RAM Pi) is enough scheduling/memory pressure to miss that 15s
deadline — with no crash log, no graceful shutdown, nothing: it just looks
like a dead SSH connection, indistinguishable at first from the network
flakiness diagnosed earlier in the DNG-fix deploy sessions today. **This
likely explains several of those earlier "SSH connection died mid-deploy"
incidents too, not just this one** — worth keeping in mind if it happens
again on unrelated work.

Not a concern for the deployed daemon's actual year-long reliability — the
watchdog is a legitimate safety net for genuine hangs during normal
operation, and the daemon's own runtime workload is lightweight. It only
tripped because of heavy *build* activity happening on the box, which
never happens during normal unattended operation.

**Fix (user-approved):** raised the timeout via a systemd drop-in,
`/etc/systemd/system.conf.d/90-watchdog-headroom.conf`:
```
[Manager]
RuntimeWatchdogSec=90s
```
Applied immediately via `sudo systemctl daemon-reload` (no reboot
needed); verified with `systemctl show -p RuntimeWatchdogUSec` →
`1min 30s`. Gives real headroom for heavy compiles while still catching
genuine multi-minute hangs during normal operation.

## Test Plan (written before implementation)

- Unit tests (macOS dev target — unlike `native_camera`/`native_codec`,
  the new stats-gathering module is deliberately **not** Linux-gated, so
  it can actually be built and tested locally this time):
  - Memory/disk/uptime reads return non-zero, sane values on whatever
    machine the test runs on (can't assert exact values, but can assert
    `total >= available`, `total > 0`, etc.).
  - Disk-usage-for-a-path correctly picks the filesystem whose mount
    point is the longest matching prefix of the target path (regression
    test: a naive "first disk that contains this path as a substring"
    approach would be wrong when e.g. both `/` and `/mnt/capture` are
    candidates).
  - CPU temperature returns `None` on non-Linux (macOS dev target) rather
    than erroring.
  - `optic_capture_log`'s new recent-health query: insert a mix of
    success/failure rows with known timestamps into a temp SQLite DB,
    assert the computed success rate, last-capture fields, and average
    duration match hand-computed expectations; assert rows older than the
    24h window are excluded.
- Pi-native build/test: `cargo test --locked --all-targets` on the Pi,
  same as every other change this session — confirms the Linux-only
  `vcgencmd` temperature path and network-interface enumeration actually
  work on real hardware, not just compile.
- Empirical hardware validation (real deploy required):
  - `GET /api/system/status` returns plausible real numbers for this Pi
    (990MB-ish total memory, real disk usage, non-zero uptime, a real CPU
    temperature, at least one non-loopback network interface with an IP).
  - `POST /api/system/restart-daemon`: daemon comes back up within a few
    seconds at the same version, `systemctl --user status` shows a clean
    restart (not a crash/failure).
  - `POST /api/system/reboot`: the Pi actually reboots and the daemon
    (systemd-enabled) comes back up automatically afterward — **this test
    is destructive/disruptive by nature and needs the user's explicit
    go-ahead immediately before running it**, not just general approval
    to build the feature.

## Implementation Summary

- `src/system_status.rs` (new): `SystemStatusReader` (memory/disk/uptime
  via `sysinfo`, CPU temp via `vcgencmd` subprocess on Linux only, network
  interfaces via `if-addrs`), `best_matching_disk` (longest-mount-point-
  prefix resolution), `reboot_host()`/`restart_daemon_detached()`.
  Deliberately not platform-gated — builds and unit-tests on macOS.
- `src/optic_capture_log.rs`: `CaptureLog::recent_health(window_hours)` —
  the first consumer of this module's SQLite DB, rolling up success rate/
  last-capture outcome/average duration over a time window. Needed
  `CaptureHealthStatus::empty()` made `pub` so `web.rs` can construct a
  zeroed response when `capture_log` is `None`.
- `src/web.rs`: `AppState` gained a `system_status: SystemStatusReader`
  field; three new routes — `GET /api/system/status` (combines a
  `SystemStatus` snapshot with a 24h `CaptureHealthStatus`),
  `POST /api/system/reboot`, `POST /api/system/restart-daemon`.
- `src/main.rs`: constructs `SystemStatusReader` with the capture dir and
  the capture-log DB's parent directory as watched paths.
- `src/web/index.html`/`app.js`/`styles.css`: new "System" dashboard card
  — memory/disk/temp/uptime/network/capture-health `dl`, restart-daemon
  and reboot buttons (native `confirm()` dialogs, no custom modal — v1
  scope), independently polled every 15s (vs. the existing 3s
  `/api/status` loop). `formatBytes` extended with a GiB tier.
- `Cargo.toml`: added `sysinfo = "0.32"` and `if-addrs = "0.13"`, both
  cross-platform (no `target_os = "linux"` gate needed).
- `systemd/optic-daemon.service`: `RestrictAddressFamilies` extended with
  `AF_NETLINK` — see Validation below for why.

## Validation

- **macOS dev target:** `cargo build`/`cargo test --all-targets` (46
  passed after the fixes below), `cargo fmt --check`, `cargo clippy -D
  warnings` all clean.
- **Pi-native build/test/Clippy (`optic-daemon` 0.1.19,
  first deploy attempt):** all passed on the real target — 46/46 tests,
  clean Clippy, clean release build, clean install with health check.
  Getting to this point took several retries; see the "Incidental
  Finding" section above (hardware watchdog reboots during heavy
  compilation, fixed by raising `RuntimeWatchdogSec` to 90s).
- **Empirical hardware validation, round 1 — found two real bugs:**
  `GET /api/system/status` on the live 0.1.19 deploy returned correct
  memory (987MB total, matching this Pi), correct CPU temperature
  (43.9°C via `vcgencmd`), correct uptime, and a correctly-computed
  capture-health rollup (5/7 succeeded in the last 24h, matching real
  capture history) — **but `disks: []` and `network_interfaces: []` were
  both empty**, which should not have been possible.
  - **Disk bug:** `system_status.rs` called `Disks::refresh()`, whose own
    doc comment states it does nothing on an empty list — only
    `refresh_list()` actually enumerates mounted filesystems. Since the
    `Disks` instance was constructed via `Disks::new()` (empty) and never
    had `refresh_list()` called on it, every snapshot silently returned
    zero disks. **This should have been caught locally**: the existing
    unit test only asserted properties *of* each disk inside a `for`
    loop, never that any disks were found at all, so it passed vacuously
    against an always-empty list on macOS too. Fixed both the bug
    (`refresh()` → `refresh_list()`) and the test (added an explicit
    `assert!(!status.disks.is_empty())` first).
  - **Network bug:** `if-addrs` uses libc's `getifaddrs()`, which opens an
    `AF_NETLINK` socket internally on Linux — not in the service's
    `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6` allowlist, so the
    sandbox silently blocked it (the code's own error handling then
    quietly returned an empty list, matching observed behavior exactly).
    Fixed by adding `AF_NETLINK` to the systemd unit. This one is
    inherently Pi-sandbox-specific and can't be fully regression-tested
    on macOS (no such sandboxing there); strengthened the local test to
    at least assert non-emptiness on the unsandboxed dev machine, with an
    explicit code comment noting the Pi-specific gap.
- **Empirical hardware validation, round 2 (`optic-daemon` 0.1.20):**
  deployed via the real deploy script (46/46 tests, clean Clippy, clean
  release build, clean install — no watchdog reboot this time, headroom
  fix held). Live `GET /api/system/status`: `disks` and
  `network_interfaces` both correctly non-empty this time — `root` (235GB,
  matches `df`), `state` dir (same filesystem as root, correct), and
  `wlan0` with its real IP (`192.168.0.195`). **Found a third real bug in
  the same check:** `capture`'s entry reported *identical* figures to
  `root` (251,503,996,928 bytes total for both) — but `/mnt/capture` is a
  256MB tmpfs (confirmed via `findmnt`/`df` on the Pi:
  `tmpfs 256M 0 256M 0% /mnt/capture`, completely different from root's
  235GB `/dev/mmcblk0p2`). Root-caused to `sysinfo` excluding tmpfs mounts
  from `Disks::refresh_list()` by default on Linux (confirmed by reading
  `sysinfo`'s own source: `"tmpfs" => !cfg!(feature = "linux-tmpfs")` in
  `src/unix/linux/disk.rs`) — `best_matching_disk` then fell back to root
  as the only remaining candidate prefix match, silently returning wrong
  data rather than erroring. **This is exactly the tmpfs-fill risk this
  panel exists to catch**, so silently misreporting it as "228GB
  available" would have been actively counterproductive. Fixed by adding
  `features = ["linux-tmpfs"]` to the `sysinfo` dependency in
  `Cargo.toml`. Inherently Linux/tmpfs-specific — no meaningful way to
  regression-test this on macOS, where there's no equivalent small tmpfs
  mount to distinguish from root; documented directly in
  `system_status.rs`'s module doc comment instead.
- **Empirical hardware validation, round 3 (`optic-daemon` 0.1.21):**
  deployed clean (46/46 tests, Clippy clean, release build clean, install
  clean — watchdog headroom held again, no unplanned reboot). Live
  `GET /api/system/status`: `capture` now correctly reports
  `total_bytes: 268435456` (256MB, matching `size=262144k` from
  `findmnt`) and `available_bytes: 268435456` (fully empty, correct —
  nothing queued at check time) — genuinely distinct from `root`'s 235GB,
  fixing the misleading-data bug from round 2. `root`/`state`/`network`/
  `memory`/`capture_health` all continue to report correct live values
  across all three rounds. **All three read-only-health bugs (A) are now
  fixed and empirically confirmed correct on real hardware.**
- **`POST /api/system/restart-daemon` / `POST /api/system/reboot` (B):**
  not yet exercised — pending either the user testing them directly via
  the dashboard's own buttons (the more representative test, since that's
  the actual UI flow being validated) or an explicit go-ahead to trigger
  them programmatically. `reboot` in particular is disruptive by nature
  and needs the user's go-ahead immediately before running it, not just
  general approval to build the feature (per the Test Plan above).

## Follow-up: htop-Style Usage Bars (`optic-daemon` 0.1.22, then fixed in 0.1.23)

User feedback after seeing the deployed panel: wanted memory/disk shown as
a filled bar (like `htop`) rather than plain "X used / Y total" text.
Added `.usage-bar`/`.usage-bar-fill` (green/orange/red at 70%/90%
thresholds)/`.usage-bar-label` to `styles.css`, and a `usageBarHtml(used,
total)` helper in `app.js` used for memory and both disk `dd` entries
(root, capture), building HTML strings with an inline `style="width:
X%"` attribute and assigning via `.innerHTML`.

**0.1.22 shipped a real, screenshot-confirmed bug:** every bar rendered
at the same ~37% fill regardless of the actual percentage (61% memory,
9% root disk, 0% capture disk all looked identical). Root cause: the
dashboard's own CSP is `style-src 'self'` with no `unsafe-inline`
(`SECURITY_POLICY` in `web.rs`) — inline `style="..."` attributes parsed
from markup (including markup assigned via `.innerHTML`) are silently
dropped by the browser under that policy. The percentage math itself was
correct throughout (same `pct` variable drove both the label text and the
now-discarded width style) — only the visual bar was affected, matching
exactly the observed symptom (right numbers, wrong-looking bars).

**Fix (0.1.23):** stopped building HTML strings with inline styles
entirely. `index.html` now has static markup for each bar
(`<div class="usage-bar"><div id="system-*-bar"
class="usage-bar-fill"></div></div>` + a label `<span>`), and `app.js`
sets `barEl.style.width` via direct DOM/CSSOM property assignment instead
of an HTML attribute — exempt from `style-src` (it's script territory,
governed by `script-src`, which this dashboard already allows for
`'self'`). Also fixed a smaller layout issue in the same screenshot:
"Disk (capture)"/"Disk (root)" wrapped awkwardly in the 90px label
column; renamed to "Capture disk"/"Root disk" (fits on one line, reads
fine given "Memory" already establishes the section's about resources).

## Objective

Project Optic is meant to run unattended for a full year. Today, anything
beyond what the dashboard already exposes (live preview, capture
profile/settings, manual capture, config commit/discard, data-sync
status/pause/resume/retry) requires SSH — or, worse, physically attaching a
keyboard and monitor to the Pi. That's the problem this feature is meant to
solve: give the web dashboard enough visibility and control over the Pi
itself that day-to-day operation and troubleshooting don't need a terminal.

This is explicitly scoped as *dashboard/daemon* work — extending
`optic_web` (and probably a small new stats-gathering helper module), not a
new architectural subsystem on the level of `optic_sync`/`optic_capture_log`.

## How This Was Scoped

Direct request: "I want to add a system control feature... imagine we are
developing this year long time lapse shotting system. we need some kind of
pi management interface. otherwise we have to connect pi to keyboard and
monitor." No specific feature was named — this is an open brainstorm.

## Brainstormed Feature Candidates

Grouped by risk/complexity, not by priority — priority is an open decision
(see below).

### A. Read-only system health (safe, high value, should be easy)

- **Disk/storage usage:** `/mnt/capture` tmpfs usage (already flagged
  elsewhere as a real fill risk), the new `~/.local/state/optic-daemon`
  SQLite state dir, and root filesystem free space. All three matter for a
  year-long deployment in ways a single dashboard glance could catch early.
- **Memory usage:** this Pi has only 990MB RAM (confirmed this session via
  `free -h` while debugging deploy flakiness) — a genuinely tight budget
  for a year of unattended operation. A simple used/free/available gauge,
  maybe with a threshold-based warning color, would surface memory
  pressure before it causes a problem.
- **CPU temperature:** `vcgencmd measure_temp` (already used ad hoc this
  session). Thermal throttling risk matters for a device that might run in
  an enclosure or direct sun for a year.
- **Uptime / last boot time:** cheap, and useful for correlating "did it
  reboot unexpectedly" with other symptoms.
- **Network status:** current IP address(es) and active interface
  (WiFi vs Ethernet), so if the Pi's IP changes there's still a way to
  find it without router access. WiFi signal strength if applicable.
- **Capture health rollup, powered by `optic_capture_log`'s SQLite DB:**
  this module was explicitly built with "no dashboard UI yet" as a stated
  non-goal (see `docs/optic-daemon-capture-log.md` §6) — this feature is
  the natural first consumer. Recent success rate, average capture
  duration trend, last successful capture timestamp. Directly answers "is
  the year-long timelapse actually working" without SSH.

### B. Actionable controls (higher value, need real safety thought)

- **Reboot the Pi.** The single most direct answer to "otherwise we have
  to connect it to a keyboard and monitor" — the scenario that's
  historically forced a physical trip is a hung/unresponsive system that
  only a power cycle fixes. Needs a `sudo`-capable command from a
  non-interactive context (the passwordless-sudo setup from the DNG
  investigation earlier today — currently scoped broadly as
  `liam-nopasswd`, not narrowly to a specific reboot command — is directly
  relevant prior art here; see Open Questions).
- **Restart just the `optic-daemon` service** (lighter than a full
  reboot) — useful if the camera pipeline gets stuck without the whole Pi
  being unresponsive. Structurally tricky: the HTTP handler serving the
  request would need to trigger `systemctl --user restart
  optic-daemon.service` in a way that survives the daemon's own process
  being killed mid-response (e.g. spawn a detached process that waits a
  beat before issuing the restart, so the HTTP response can flush first).
- **View recent daemon logs from the dashboard.** Directly useful for
  diagnosing problems without SSH, but journald on this Pi has no
  persistent storage *and* live-follow was found not to capture anything
  either (confirmed during the capture-performance investigation) — real
  log access would need the daemon to write its own rotating log file
  instead of/alongside relying on journald, which is a bigger change than
  it sounds.

### C. Considered but likely deferred (real value, more risk/complexity than a first pass warrants)

- **Self-update / trigger a redeploy from the dashboard.** Tempting given
  how much of this session was spent on manual deploy cycles, but
  self-updating systems are a real design space (a bad update could brick
  remote access entirely, with no physical access as a fallback) — not
  something to build casually as part of a first "system control" pass.
- **Purge/clear a stuck `/mnt/capture` sync queue from the UI.** Real
  operational value if `optic_sync` ever gets stuck, but exposing "delete
  files" as a button is a genuine data-loss risk that deserves its own
  careful design (confirmation flow, dry-run, etc.) rather than being
  folded into a general system-control feature.
- **System clock / RTC visibility.** Worth having eventually — clock drift
  on an unattended embedded device is a real failure mode, and it matters
  more once `optic_scheduler` (solar-cadence timing) exists — but there's
  no RTC-dependent feature built yet, so it's not urgent today.
- **Shutdown (vs. reboot).** Less clearly useful for an always-on
  timelapse box than reboot is; only relevant for planned maintenance
  windows. Low cost to add alongside reboot if reboot gets built, not
  worth its own separate design effort.

## Open Questions (need a decision before implementation starts)

1. **Which feature(s) actually ship first?** This worklog deliberately
   doesn't recommend a priority order yet — that's the next conversation.
2. **Auth/safety for privileged actions.** `optic_web` today has no
   authentication — it's reachable to anyone on the local network. That's
   a reasonable tradeoff for "take a photo" or "pause sync," but reboot/
   service-restart are more consequential. Does this feature need to
   introduce *any* access control (even something minimal, like a
   confirmation step or a shared token), or is "it's on my home network"
   an acceptable trust boundary for this project? Should be decided
   explicitly, not defaulted into.
3. **How does a privileged action actually get permission to run?** The
   passwordless-sudo rule added earlier today
   (`/etc/sudoers.d/liam-nopasswd`, blanket `NOPASSWD: ALL`) would
   technically make a `sudo reboot` from the daemon trivial to implement,
   but it was added for a narrow ad hoc purpose (WiFi power-save toggling
   during interactive debugging) with much broader scope than that one
   use justified. Building a permanent daemon feature on top of a blanket
   passwordless-sudo grant is worth a deliberate decision, not an
   accidental dependency — narrowing it to the specific commands this
   feature needs (`sudo systemctl reboot`, etc.) via a scoped sudoers rule
   is probably the right move regardless of which features ship.
4. **Where do system stats get read from, and how expensive is polling
   them?** `/proc/meminfo`, `vcgencmd`, `findmnt`/`df` for disk usage are
   all cheap, but the dashboard's existing `/api/status` poll cadence
   (frequent, per the live preview's responsiveness needs) means whatever
   gets added here needs to actually be cheap at that frequency, or get
   its own slower-polled endpoint.
5. **Log access, if pursued (§B):** would need the daemon to write to a
   real file (rotated, size-bounded) rather than relying on journald,
   which is a small but real scope addition beyond just adding a dashboard
   panel.

## Explicitly Out of Scope (this worklog / brainstorm phase)

- No code, no schema, no API design yet — this is intentionally
  brainstorm-only, per the project's required workflow (understand scope
  before planning tests before implementing).
- No decision yet on `optic_scheduler` integration, since a future
  scheduler might want its own health/status surfaced here too, but that
  module doesn't exist yet.
