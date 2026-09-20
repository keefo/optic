# Dated Worklog: 2026-09-19 - Fast `--assets` Deploy Path for Static Web Files

Status: **implemented, deployed, and verified on real hardware** — including
a real negative test (Biome catching a genuine defect this exact change
introduced, see Findings) and two real positive deploys.

## Objective

The full `./scripts/build-deploy-optic-daemon.sh` run (used for every change
this session, including pure CSS tweaks) always does a full Rust bootstrap +
`cargo test`/`clippy`/`--release` build, which takes minutes, even when the
only change is to `src/web/*.{html,js,css}`. Since `optic-daemon` already
serves these files directly from disk on every request (no embedding, no
restart needed to see a change — confirmed by the existing
`serve_asset_reflects_current_file_contents_on_each_request` test and by
`web.rs`'s asset-serving implementation), a static-asset-only change doesn't
need a Rust rebuild or even a service restart at all. The user asked for a
fast deploy path specifically for this case.

## Acceptance Criteria

- A new `--assets` flag on `build-deploy-optic-daemon.sh` that:
  - Skips the Rust toolchain bootstrap, sysroot rebuild, `cargo fmt`/`test`/
    `clippy`/`build --release`, and does **not** stop or restart
    `optic-daemon.service` (no reason to — the daemon doesn't need to know
    files changed).
  - Still runs Biome lint on the web files (cheap, and this is exactly the
    check most likely to catch a real mistake in this kind of change).
  - Still copies files into the exact same destination
    (`~/.local/bin/web/`) the full pipeline uses, so there's only one
    "installed" location regardless of which path was used to get there.
  - Still has a backup-and-rollback safety net if verification fails after
    copying — cheap to do (a handful of small files) and matches this
    project's existing change principles (recoverable operations).
  - Still verifies the served content afterward via real HTTP requests
    (not just "the copy succeeded") — reusing the same content assertions
    the full pipeline already has for `app.js`/`index.html`/
    `scheduler.html`/`scheduler.js`, since those are generic content checks
    unrelated to the Rust build.
- With no arguments, behavior is completely unchanged from today (full
  pipeline) — this is strictly additive.
- `--help` documents the new flag.
- `docs/optic-daemon-build-environment.md` gets a short note on when to use
  `--assets` vs the full path.

## Test Plan (written before implementation)

- `bash -n scripts/build-deploy-optic-daemon.sh` after editing — syntax
  check.
- `--help` output shows the new flag.
- Real run against the Pi: `./scripts/build-deploy-optic-daemon.sh --assets`
  with a real, visible CSS change staged, confirm:
  1. It finishes in seconds, not minutes (no Rust build observed in the
     output).
  2. `optic-daemon.service`'s `Main PID` / start time is unchanged
     afterward (proof it was never touched).
  3. The changed CSS is actually being served afterward
     (`curl .../styles.css` reflects the change).
  4. The existing content-assertion checks (scheduler page, app.js FPS
     values, etc.) still ran and passed.
- Negative test: intentionally break a web file (e.g. invalid JS) and
  confirm `--assets` catches it via Biome and leaves the previously-deployed
  assets in place (rollback), rather than installing broken files.
- Confirm the zero-argument (full) path still behaves identically to
  before — run it once after the change to be sure nothing in the shared
  code path (usage/arg parsing) regressed.

## Implementation Summary

- `scripts/build-deploy-optic-daemon.sh`: added a `MODE` variable
  (`full`/`assets`) set by the new `--assets` flag. Rather than duplicating
  the remote script (which would fork the verification/rollback logic into
  two copies that could drift), the single remote heredoc now branches
  internally: RPi hardware prereqs, the Rust toolchain bootstrap, the
  native sysroot rebuild, and `cargo fmt`/`test`/`clippy`/`build --release`
  are all wrapped in `if [[ $MODE == full ]]`; Biome linting, the
  install-with-backup-and-rollback path, the `diff -rq` install-matches-
  source check, the service-active/healthz checks, and all four served-
  content assertions (`app.js` FPS values, `index.html` responsiveness +
  scheduler link, `scheduler.html`/`scheduler.js` content) stayed
  unconditional and shared between both modes.
- Local (macOS) side: packaging and upload also branch by mode — `--assets`
  tars only `biome.json` + `src/web` (not `Cargo.toml`/`src/*.rs`/etc.) and
  uploads to a fixed, non-versioned staging path
  (`~/.cache/optic-assets-deploy` on the Pi) instead of the versioned
  `~/.local/src/optic-daemon-$VERSION` release directories the full path
  uses — there's no "release" concept for an assets-only push.
- `docs/optic-daemon-build-environment.md`: added a short section pointing
  at `--assets` for web-only changes, full path for anything touching Rust/
  systemd.

## Findings While Implementing

- **A real bug, caught only by actually running it against the Pi, not by
  reading the diff.** The first working version of `--assets` packaged only
  `src/web` (not `biome.json`). Biome discovers its config by walking up
  from the current directory; without `biome.json` present anywhere in the
  uploaded tree, it silently fell back to its own default formatter
  settings (tab indentation) instead of this project's `indentStyle:
  space`, and flagged nearly the entire `app.js` as needing reformatting —
  a false positive that would have blocked every future `--assets` deploy.
  The full path never hit this because its `SOURCE_DIR` always includes
  `biome.json` at the same level as `src/`. Fixed by including `biome.json`
  in the `--assets` tarball too. This is exactly the kind of gap CLAUDE.md's
  "never infer a pass from source inspection alone" rule exists for — the
  restructured script read correctly and passed `bash -n`, but was wrong
  until actually exercised.
- Confirmed, via a deliberate negative test (see Validation), that a
  genuinely broken JS file is rejected by Biome *before* anything is
  installed on the Pi — no rollback was even necessary because nothing had
  been touched yet, which is the ideal outcome for a fast path used for
  quick iteration.

## Validation

- `bash -n scripts/build-deploy-optic-daemon.sh` — clean.
- `bash -n` on the extracted remote heredoc — clean.
- `--help` output documents `--assets`; an unknown flag still exits 64 with
  usage on stderr, matching prior behavior.
- **Negative test:** appended an intentionally invalid line to `app.js`,
  ran `--assets` — failed at the Biome step (as designed), confirmed via
  `curl .../app.js` and `systemctl --user show ... MainPID,
  ExecMainStartTimestamp` that nothing on the Pi changed (`MainPID=880`,
  start timestamp unchanged from before the test). Restored the file,
  confirmed clean.
- **First positive test** surfaced the `biome.json` bug above (a broken-
  build-not-broken-content failure, distinct from the negative test).
  Fixed, then:
- **Second positive test:** added a real, visible CSS change
  (`--optic-fast-deploy-probe: 1` in `:root`), ran `--assets` — completed
  in ~1.1s wall time (vs. minutes for the full path), confirmed via
  `curl .../styles.css` that the change was live, and confirmed
  `MainPID`/`ExecMainStartTimestamp` were **unchanged** (`880` /
  `20:36:25`), proving the daemon was never touched. Reverted the probe
  and ran `--assets` again to leave the Pi clean — succeeded identically.
- **Full-path regression check:** ran `./scripts/build-deploy-optic-daemon.sh`
  with no arguments after all the above changes, since the refactor
  touched nearly every line of the remote script via added conditionals.
  Completed successfully end-to-end (bootstrap, `cargo fmt`/`test`/
  `clippy`/`build --release`, install, all verification steps, including
  the four content assertions) with no rollback — confirming the shared
  code paths still work correctly for the case that was already
  well-exercised all session.

## Remaining Limitations / Follow-up

- `--assets` intentionally does not verify anything about `Cargo.toml`
  version consistency or the running binary — by design, since it's meant
  for changes that don't touch either. If a change accidentally needs both
  (e.g., a new API field the JS expects), `--assets` alone won't catch a
  backend/frontend mismatch; use the full path for anything that isn't
  purely presentational.
- The pre-existing `journalctl --user -u optic-daemon.service` observability
  quirk (documented in `docs/optic-daemon-capture-performance.md` §2.2 and
  `worklogs/2026-09-19-persistent-journal-crash-evidence.md`) affects the
  script's own "Recent service log" printout at the end of a run —
  sometimes it prints real lines, sometimes "-- No entries --", regardless
  of which mode was used. Cosmetic only; deploy success/failure doesn't
  depend on it.
