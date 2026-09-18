# Dated Worklog: 2026-09-17 - Dynamic Filesystem Asset Loading

Status: implemented, tested locally, and deployed to the Raspberry Pi (`optic.local`) as `optic-daemon` 0.1.8. Live-verified over the LAN — see Deployment. A regression found live in 0.1.7 (see Follow-up: crash on control input) was fixed and shipped as 0.1.8.

We transition the `optic-daemon` web resource delivery system away from compile-time macro embeds (`include_str!`) to dynamic filesystem loads. This allows rapid hot-reloading of UI modifications without triggering full compilations.

## Objective & Acceptance Criteria
1. Web assets (`index.html`, `app.js`, `styles.css`) are loaded dynamically from the directory where the `optic-daemon` executive binary resides.
2. The daemon resolves files by calling `std::env::current_exe()` and querying sibling paths.
3. Added robust error logging and user-friendly error statuses if assets are missing.
4. Preserved all Content Security Policies, response headers, and testing functionality.
5. Setup automated deployment scripts to copy files side-by-side next to the target `/home/liam/.local/bin/optic-daemon`.
6. (Added after initial implementation, per follow-up request) Serving is asset-file-agnostic: any file placed under the asset directory is servable without registering a new route or filename in the source, and requests are strictly confined to the asset directory (no path traversal, including via symlinks).

## Test Plan
- Run tests on both local workspace and remote Raspberry Pi.
- Confirm daemon runtime serves modified frontend files immediately upon browser refresh.

---

## Implementation Summary

- `src/web.rs`: removed the `INDEX_HTML`/`APP_JS`/`STYLES_CSS` `include_str!` constants used for serving. Added `asset_dir: Arc<PathBuf>` to `AppState` (new required constructor argument). Routing is now asset-agnostic instead of one hardcoded route per file: `GET /` serves `index.html` explicitly, and `GET /{*asset}` serves any other relative path resolved under `asset_dir` (`app.js`, `styles.css`, or anything dropped in later, including in a subdirectory) — no route or filename list to edit when adding a new asset.
  - `safe_asset_path(asset_dir, requested)` walks the requested path's `Component`s and accepts only `Component::Normal` segments, rejecting `..`, a leading `/`, and `.` structurally before touching the filesystem.
  - `serve_asset` then canonicalizes both the asset directory and the candidate path (resolving symlinks) and confirms the resolved file is still inside the canonical asset directory before reading it — closing the symlink-escape case a purely lexical check would miss (e.g. a symlink placed inside the asset directory pointing outside it).
  - Anything rejected by either check logs via `tracing::warn!` and returns `400 Bad Request` without echoing the attempted path back to the client; a structurally valid path that fails to read (missing/unreadable file) logs via `tracing::error!` with the resolved path and OS error, and returns `500` with a short plain-text body (`asset_unavailable_response`). Both paths keep the same `Content-Security-Policy`, `X-Content-Type-Options: nosniff`, and `Cache-Control: no-store` headers as successful responses.
  - `static_response` now serves raw bytes (`Vec<u8>`) rather than a `String`, since arbitrary asset types (images, fonts, JSON) are no longer necessarily UTF-8 text.
  - `content_type_for(path)` infers `Content-Type` from the file extension (`html`, `js`/`mjs`, `css`, `json`/`map`, `svg`, `png`, `jpg`/`jpeg`, `ico`, `webp`, `woff`/`woff2`, `txt`), defaulting to `application/octet-stream`, since there's no longer a per-route content type to hardcode.
- `src/main.rs`: added `resolve_web_asset_dir()`. Resolution order: `OPTIC_WEB_ASSETS_DIR` env override, then a `web/` directory sibling of `std::env::current_exe()` (used if it exists), then a compile-time fallback to `$CARGO_MANIFEST_DIR/src/web` for local `cargo run`/`cargo test`. Logs the resolved directory at startup (`info` if present, `warn` if not — non-fatal, since the acceptance criterion is a friendly per-request error, not a startup failure).
- `src/web.rs` tests: kept the original compile-time content assertions (`embedded_assets_are_present`) by declaring local `include_str!` consts inside `#[cfg(test)] mod tests` (decoupled from production serving code, so they still catch accidental content regressions in `src/web/*`). Added tests calling `serve_asset`/`safe_asset_path` directly against temp directories: hot-reload (two reads of an edited file return different content), serving a brand-new, never-route-registered file including in a subdirectory with an auto-detected content type (`serve_asset_serves_any_file_dropped_into_the_asset_directory`), missing-file friendly error, header preservation on success/failure, `safe_asset_path` unit tests for `..`, absolute paths, and `.`, an end-to-end `serve_asset` test for several traversal strings (`400` for each), and a Unix-only symlink-escape test confirming a symlink inside the asset directory pointing outside it is rejected.
- `scripts/setup-optic-daemon-phase-01.sh`: now checks only that `$PROJECT_ROOT/src/web` exists with an `index.html`, and installs the entire directory tree (`cp -r`, then normalizes permissions) into `~/.local/bin/web/`, replacing the previous fixed three-file `install` list — a new file under `src/web/` is deployed automatically, and stale files removed from the source are removed from the install too (the directory is `rm -rf`'d before the copy).
- `scripts/build-deploy-optic-daemon.sh`: removed the stale `grep -aF '${profile.previewFps} FPS' "$BINARY"` check (no longer meaningful — the JS text is no longer embedded in the binary). Added backup/rollback handling for `~/.local/bin/web/` symmetric with the existing binary/unit rollback, and replaced the fixed-filename `cmp` loop with `diff -rq` over the whole `src/web` vs. installed `web/` directory tree. The existing end-to-end `curl` checks against `/app.js` and `/` after install now exercise the real disk-read path.
- `docs/optic-daemon.md`, `docs/optic-daemon-build-environment.md`, `setup.md`: updated "embedded"/`include_str!` language to describe disk-based loading; the "Web Asset Loading" subsection under the Web Service design now documents the agnostic route, path-containment mechanism (lexical component check + canonicalize-and-prefix check), content-type inference, resolution order, deployed layout, and failure behavior. Added a `GET /{*asset}` line to the API endpoint reference.

## Validation

Environment: local macOS development workspace (`cargo` targeting the host, not the Pi's `aarch64-unknown-linux-gnu` target — the camera backend on this platform is the non-Linux stub, so this validates the web layer only).

Commands and results:
- `cargo build` — passed.
- `cargo test` — 17/17 passed, including all `serve_asset`/`safe_asset_path` tests (hot-reload, agnostic serving, missing-file, headers, traversal strings, symlink escape) and the unchanged `embedded_assets_are_present` content check.
- `cargo fmt --all -- --check` — passed (after running `cargo fmt --all` to apply formatting each round).
- `cargo clippy --all-targets -- -D warnings` — passed, no warnings.
- `bash -n scripts/build-deploy-optic-daemon.sh` and `bash -n scripts/setup-optic-daemon-phase-01.sh` — both pass shell syntax checking.
- Manual runtime verification (round 1, fixed-route version): ran `target/debug/optic-daemon` locally against a temp capture dir with `OPTIC_BIND_ADDR=127.0.0.1:8123`.
  - Startup log confirmed dev fallback resolved to `src/web` (no sibling `web/` dir next to the debug binary).
  - `curl /healthz`, `curl /`, `curl /app.js` returned expected content; `curl -I /app.js` showed unchanged `Content-Type`, CSP, `X-Content-Type-Options: nosniff`, `Cache-Control: no-store`.
  - Appended a marker comment to `src/web/app.js` while the daemon was running (no restart): the next `curl /app.js` immediately returned the marker; reverting the file and re-curling showed it gone — confirms hot reload with zero rebuild.
  - Renamed `src/web/styles.css` away: `curl /styles.css` returned `500` with `optic-daemon: web asset unavailable: styles.css` and the same security headers; daemon log showed the `tracing::error!` line with the resolved path and `No such file or directory (os error 2)`. Restored the file: `curl /styles.css` returned `200` again on the next request, no daemon restart needed.
  - Ran the daemon a second time with `OPTIC_WEB_ASSETS_DIR=/tmp/optic-web-override` pointing at a directory containing a custom `index.html`: `curl /` returned that overridden content, confirming the env override takes precedence over the exe-sibling/dev-fallback resolution.
- Manual runtime verification (round 2, after switching to the agnostic `/{*asset}` route): re-ran `target/debug/optic-daemon` against a fresh temp capture dir on `127.0.0.1:8125`.
  - `curl /` → `200`; `curl /app.js` → `200` with `Content-Type: text/javascript; charset=utf-8` (auto-detected, no dedicated route).
  - Dropped a brand-new file `src/web/extra-test-asset.js` (never referenced anywhere in the router) while the daemon was running: `curl /extra-test-asset.js` immediately returned its contents with `Content-Type: text/javascript; charset=utf-8` — confirms the "no route registration needed" requirement, then removed the test file.
  - Sent three real-HTTP traversal attempts against the running server: `/..%2f..%2f..%2fetc%2fpasswd`, `/%2e%2e/%2e%2e/%2e%2e/etc/passwd`, and (with `curl --path-as-is`) `/../../../etc/passwd`. All three returned `400`, and the daemon log recorded `rejected a web asset request with an unsafe path requested="../../../etc/passwd"` for each (the URL-encoded variants decode to the same string via axum's path-param decoding). `/etc/passwd` was never read.
  - All temp files, directories, and background processes from both manual-verification rounds were cleaned up afterward (`pkill -f target/debug/optic-daemon`, `rm -rf` on temp dirs).

## Remaining Limitations / Next Steps

- No change to `systemd/optic-daemon.service`: it already runs with the process's own working directory undefined by the unit (no `WorkingDirectory=` set), so asset resolution relies entirely on `current_exe()`, not `$PWD` — confirmed by reading the unit file and by the live deployment below (resolved to `/home/liam/.local/bin/web`).
- `web.rs` has no real `404 Not Found`: any unmatched path (a typo, a removed endpoint, anything not one of the explicit `/api/*`/`/healthz` routes) falls into the `/{*asset}` catch-all and gets treated as a file lookup, returning `500` instead of `404` when the "file" doesn't exist. Flagged to the user during this session; not fixed — no decision yet on whether/how to add a real fallback. Wrong-method requests to real routes correctly return `405` (unaffected).

## Deployment (0.1.7)

Bumped `Cargo.toml`/`Cargo.lock` version `0.1.6` → `0.1.7` per this project's convention (every source/web-asset change requires a version bump + redeploy, per `setup.md`).

Three pre-existing/unrelated issues were found and fixed while getting `./scripts/build-deploy-optic-daemon.sh` to actually run, before any of this feature's deploy-side changes could be validated:

1. **Local Biome check was silently broken.** It depended on `npx @biomejs/biome` fetching from the npm registry, but this Mac's Node install has a broken TLS trust store (`UNABLE_TO_GET_ISSUER_CERT_LOCALLY` against `registry.npmjs.org` specifically — `curl` to the same host works fine, confirmed with `openssl s_client`; this is a local Node/npm CA-bundle issue, not a project or network problem). The user has a Biome binary already present on the Pi at `/home/liam/biome` (`v2.5.14`, aarch64 Linux ELF — can't run on the Mac). Moved the check into the remote deployment step (`REMOTE_SCRIPT` in `build-deploy-optic-daemon.sh`, right before `cargo fmt --check`), running `"$HOME/biome" check src/web/app.js src/web/index.html` against the uploaded source tree instead of `npx` locally.
2. **`biome.json` was still v1.8.3 schema**; the Pi's v2.5.14 binary rejects it outright (`organizeImports` moved under `assist.actions.source`, `recommended` → `preset`). Ran `biome migrate --write` (via the Pi binary, against a throwaway scratch copy) and applied the mechanical result to the repo's `biome.json` — no rule/behavior change, just modernized keys.
3. **The packaging `tar` step referenced stale root-level doc paths** (`optic-daemon-build-environment.md`, `optic-daemon-camera.md`, `optic-daemon.md`) that had already been moved under `docs/` in an earlier, unrelated change; and separately, moving the Biome check into the remote step (fix #1) meant `biome.json` also needed to be in the uploaded tarball, which it wasn't. Both `tar` invocations and the pre-flight existence-check loop in `build-deploy-optic-daemon.sh` were updated to the correct `docs/*.md` paths and to include `biome.json`. (First deploy attempt failed at packaging on the stale doc paths; second attempt got further but failed Biome's real lint pass with the *default* Biome style — tabs — because `biome.json` was missing from the tarball, so Biome silently fell back to its built-in defaults instead of erroring; third attempt, after including `biome.json`, packaged and ran correctly.)

Per explicit user request, before continuing the deploy, also fixed the **pre-existing** Biome findings themselves (not introduced by this session, confirmed via `git show HEAD:src/web/index.html` / usage search before touching anything):
- `src/web/index.html:44`: `<div class="responsiveness-panel" aria-labelledby="responsiveness-title">` — `aria-labelledby` isn't valid on a plain `<div>` (role "generic"). Changed the tag to `<section>` (open/close), matching the exact pattern already used by the four sibling cards (`preview-card`, `profile-card`, `controls-card`, `telemetry-card`), which all pair `aria-labelledby` with `<section>`. Confirmed no CSS/JS selects that element by tag (only by `.responsiveness-panel` class), so this is a pure a11y fix.
- `src/web/app.js`: removed the redundant top-of-file `"use strict";` (module code is strict by default; flagged by `noRedundantUseStrict`). Removed three genuinely dead variables after confirming (via `rg`) they are assigned but never read anywhere: `activePreviewProfile` (declared + 2 assignment sites, no reads — deleted the declaration and both assignment lines, left the surrounding logic untouched) and `activeSliderTimer`/`sliderIsActiveDragging` (declared, never assigned or read anywhere else — deleted both declarations outright, not just renamed). Changed the one `catch (error) { … }` block that never used `error` (in `refreshStatus()`, the periodic status-poll handler) to the bare `catch { … }` form (valid ES2019+ optional catch binding) — every other `catch (error)` in the file does use `error.message`, confirmed by inspecting all 8 catch sites, so this was the one real outlier rather than a style choice to preserve.
- Ran Biome's own formatter (`biome check --write`, safe fixes only) for the remaining pure-whitespace reflow (330-line diff in `app.js`: multi-line object literals, wrapped boolean chains and template-string arguments past the 100-col limit, a couple of pre-existing over-indented top-level blocks corrected to column 0, added a missing trailing newline). Reviewed the full diff before applying — confirmed every hunk is whitespace-only, no operator/operand/logic changes. `index.html` needed no formatter changes.
- Re-ran the Pi's Biome binary against the fixed files: clean pass (`Checked 2 files ... No fixes applied.`, exit 0) before re-attempting deployment.

**Validation on the Pi** (native `aarch64-unknown-linux-gnu`, via `./scripts/build-deploy-optic-daemon.sh`):
- Biome check: clean.
- `cargo fmt --all -- --check`: passed.
- `cargo test --locked --all-targets`: 29/29 passed (4 `libcamera_probe` + 25 `main`, including all `web::tests::*` added this session — this is the real native-camera-linked build, unlike the earlier macOS-only validation).
- `cargo clippy --locked --all-targets -- -D warnings`: passed, no warnings.
- Release build: succeeded; binary correctly linked against `libcamera.so.0.7`/`libturbojpeg.so.0` (`ldd` check), contains the `0.1.7` version string.
- Install (`scripts/setup-optic-daemon-phase-01.sh`, invoked by the deploy script): mirrored `src/web/` (now including `icon.svg`, never explicitly listed anywhere in either deploy script — proving the agnostic-install path) into `~/.local/bin/web/`; service enabled, active, healthy.
- Deploy script's own post-install checks: installed binary and every installed web asset (`diff -rq`) match the release artifact; `/healthz`, `/api/status` (version `0.1.7`), `/app.js` (contains `previewFps`), and `/` (contains `Preview responsiveness`/`First visible`) all verified over `http://127.0.0.1:8000` on the Pi; no new error-level journal entries since deploy start.
- **Independent verification from the Mac** after the script reported success (not just trusting the script's own checks): `curl http://optic.local:8000/healthz` → `ok`; `/api/status` → `version: 0.1.7`, camera `detected: true`, `streaming: true` (real IMX477, not a stub); `curl /icon.svg` → `200`, `image/svg+xml`; `/` HTML contains both the favicon `<link>` and the header `<img>` referencing `icon.svg`; a real-HTTP path-traversal attempt (`/..%2f..%2f..%2fetc%2fpasswd`) → `400`.
- **Live hot-reload test on the deployed instance**: backed up `~/.local/bin/web/styles.css` on the Pi, appended a marker comment, confirmed `curl http://optic.local:8000/styles.css` returned it immediately with **no service restart**, then restored the original file and confirmed (`diff`) it matches the deployed source exactly and the marker is gone. Service remained healthy throughout (`/healthz` → `ok`, `/api/status` unaffected).

## Follow-up: `icon.svg`

The user added `src/web/icon.svg` and asked to wire it into the page. Confirmed this needed no `src/web.rs` change at all — that's the point of the agnostic route and the extension-based `content_type_for` mapping (`svg` → `image/svg+xml` was already present). Only the frontend files changed:
- `src/web/index.html`: added `<link rel="icon" type="image/svg+xml" href="/icon.svg">` in `<head>`, and wrapped the header title in a `.brand` div with an `<img class="brand-icon" src="/icon.svg" alt="" width="48" height="48">` (empty `alt` since the adjacent "PROJECT OPTIC" / "HQ Camera Control" text already conveys the same information — the icon is decorative).
- `src/web/styles.css`: added `.brand` (flex row, centered, gapped) and `.brand-icon` (48×48, `flex-shrink: 0`) rules next to the existing `.site-header` rule.

Verified by rebuilding and running the daemon locally: `curl /icon.svg` returned `200` with `Content-Type: image/svg+xml` and the same CSP/`nosniff`/`no-store` headers as other assets; `curl /` showed both the `<link rel="icon">` and `<img class="brand-icon">` tags referencing it. `cargo test` (17/17) still passes. Later re-verified end-to-end on the deployed Pi (see Deployment below): `icon.svg` was mirrored to `~/.local/bin/web/` automatically (never explicitly listed in any deploy script) and served correctly in production.

## Follow-up: crash on control input (0.1.7 → 0.1.8)

User reported a live browser console error while using the deployed 0.1.7 dashboard:

```
app.js:396 Uncaught TypeError: Cannot set properties of null (setting 'textContent')
    at beginMeasurement (app.js:396:39)
    at beginControlMeasurement (app.js:414:3)
    at HTMLSelectElement.<anonymous> (app.js:850:5)
```

Root cause: pre-existing, unrelated to this session's dynamic-asset-loading work. `git bisect`-by-hand (`git log -p -- src/web/index.html`, then checking each commit for the removed markup) traced it to commit `629c65a` ("Introduce persistent staged sandbox, AEC-Free manual exposure, and unified Biome build constraints", 2026-09-17), which deliberately removed the "3A stable" `<dd id="settled-latency">` metric row from `index.html`'s responsiveness panel but never cleaned up the corresponding `app.js` references. `elements.settledLatency` (`document.querySelector("#settled-latency")`) has been permanently `null` ever since; every call to `beginMeasurement` — triggered by any control input (select, slider, checkbox) — crashed trying to write `.textContent` on it, breaking live-measurement tracking for every control change.

Fix (`src/web/app.js`): removed the dead `elements.settledLatency` field and all 5 write sites (`beginMeasurement`, three branches in `recordRenderedFrame`, one in `updateLivePreview`'s catch handler). Where the removed writes lived inside `recordRenderedFrame`'s stale/settled-detection branching, collapsed the three duplicate `activeMeasurements.delete(revision)` calls (previously one per branch, each gated only by which text they used to display) into a single `settled` boolean condition — the settling computation itself (`stableFrames`, `aeSettled`/`awbSettled`, `SETTLE_TIMEOUT_MS`) is still needed to decide when to stop tracking a measurement, it just no longer drives a display that doesn't exist. Verified via `rg -n "settledLatency|settled-latency"` across `app.js`/`index.html` that no references remain, `node --check app.js` for syntax, and a re-run of the Pi's Biome binary (clean, `exit=0`) before redeploying.

Bumped `Cargo.toml`/`Cargo.lock` to `0.1.8` and re-ran `./scripts/build-deploy-optic-daemon.sh` in full (chosen over a partial/manual fix specifically because a Rust-independent asset-only fix still deserves a reproducible, versioned deploy when it's going out as "the" fix for a reported bug, rather than an ad hoc live patch). Deploy succeeded: Biome clean, 29/29 `cargo test` on the Pi's native target, Clippy clean, release build linked correctly, installed, service healthy.

## Follow-up: direct asset sync for a same-session UI tweak

While 0.1.8 was mid-deploy, the user made two further, purely cosmetic edits directly in the editor: `src/web/index.html` header `<h1>` text ("HQ Camera Control" → "Camera Control") and `src/web/styles.css` `h1` font-size (`clamp(2rem, 5vw, 3.8rem)` → `clamp(2rem, 5vw, 2rem)`, i.e. now a fixed 2rem regardless of viewport width — flagged to the user as possibly unintentional since min/max are now equal, but left as-is since it's a deliberate on-disk edit and renders correctly).

The `styles.css` edit happened to land before that deploy's `tar` packaging step and made it into 0.1.8. The `index.html` title edit landed after packaging (confirmed by diffing `curl http://optic.local:8000/` against the local file post-deploy), so it was still serving the old title.

User asked whether an asset-only change like this needs a version bump and full daemon rebuild. Answer given and acted on: **no** — that's the point of this feature. `/api/status`'s `version` field is compiled in and only changes with a real rebuild, but the served page content is read from disk on every request, independent of that. Synced just the one changed file directly: `scp src/web/index.html liam@optic.local:~/.local/bin/web/index.html`, then verified live (`curl http://optic.local:8000/` shows the new `<h1>`). No version bump, no rebuild, no service restart.

Traded off knowingly: this makes `~/.local/bin/web/` (what's actually served) diverge from the versioned source tree at `~/.local/src/optic-daemon-0.1.8/` (what the last full deploy recorded) until the next full `./scripts/build-deploy-optic-daemon.sh` run, which always repackages fresh from the working directory and will reconcile the two. Told to the user explicitly before doing it.

## User Verification Steps

Deployment and the checks above are done; user has confirmed the deployed dashboard at `http://optic.local:8000/` works well, including after the 0.1.8 crash fix and the direct-sync title change. Remaining optional acceptance steps, if wanted:

1. Open `http://optic.local:8000/` in a browser and visually confirm the favicon, header icon, and updated title render as expected (only verified via `curl`/content-type/grep in this session, not visually in a browser).
2. Exercise a control input (e.g. change white balance or rotation) in the live dashboard and confirm no console error and that "First visible"/"Median" measurements still update — the crash path (`beginMeasurement` → `beginControlMeasurement`) is exactly what's exercised by any control change.
3. Decide on the open `404` question noted under Remaining Limitations (unmatched paths currently return `500`, not `404`) — no action taken pending that decision.
4. Confirm the `h1` font-size change (now fixed at `2rem` instead of scaling up to `3.8rem` on wide viewports) is the intended final sizing.
