# Dated Worklog: 2026-09-20 - Scheduler Phase 2c: Storage/Bandwidth Forecaster

Status: **implemented, deployed, and verified.**

## Objective

Design doc §8 specifies a Storage/Bandwidth Forecast alongside the Shot
Forecaster: per-shot size × forecasted frame count → total bytes and a
bytes/hour rate, cross-referenced against the 256 MiB `/mnt/capture`
tmpfs to warn the operator before a dangerous rule set actually fills it
— the proactive half of §7's full-disk answer (skip+report, already
implemented in Phase 1, is the backstop). Not implemented until now;
confirmed absent via reading `scheduler.js`'s `renderForecast` before
starting.

## Acceptance Criteria

- Per-shot byte size estimated from the *currently committed* capture
  profile (design doc §6: scheduled captures always use whatever profile
  is committed, never a scheduler-specific override).
- Total estimated bytes over the forecast window, counting **physical**
  (post-merge) shots, not raw per-rule occurrences — same requirement
  §8 states for the Shot Forecaster's own frame count.
- A warning surfaces when the projected fill time is short enough to be
  a real risk, per §8's own example framing ("fills in ~40 min at this
  rate if sync stalls") — i.e. the *rate*, not the raw 48h total,
  compared against the tmpfs capacity, and counting up from whatever's
  already queued right now, not from zero.
- `Dci4k`'s per-shot size, explicitly unmeasured in the design doc
  (flagged "TBD/measure" in §8/§11), gets a real measurement instead of
  a guess.

## Real Measurements Taken This Session

Performed live captures against the actual Pi/camera (2026-09-20,
~03:10-03:20 UTC) via `POST /api/capture`, reading the real `bytes` the
daemon reports for each file — not estimated, not reused from an old
design-doc table:

| Profile | save_dng | JPEG bytes | DNG bytes | Total |
|---|---|---|---|---|
| MasterArchive | true (mandatory) | 1,724,196 (tonight) / 7,783,245 (earlier session) | 24,673,306 / 24,661,360 | 26,397,502 / 32,444,605 |
| Dci4k | true | 360,453 | 17,531,218 | 17,891,671 |
| Dci4k | false | 357,067 | — | 357,067 |
| Binning2k | false (forbidden) | 51,830 (tonight) / ~527-536K (earlier session, 5 samples) | — | same as JPEG |

**A real, worth-stating finding**: the JPEG component varies
dramatically with scene content — the *same* MasterArchive profile on
the *same* Pi/sensor measured 7.78 MB in an earlier (daytime) session
vs. 1.72 MB just now (nighttime), a ~4.5x spread. The DNG component does
**not** vary this way (raw sensor data, size fixed by resolution/bit
depth) and measured consistently stable across sessions (24.66 vs 24.67
MB). `estimated_bytes_per_shot` (`src/web.rs`) uses the **larger** of
any available same-profile samples for exactly this reason — a
storage-fill warning erring toward "over-cautious" is far less costly
than one erring toward "falsely reassuring."

`Dci4k` had no prior daytime sample at all (the one profile the design
doc explicitly flagged as unmeasured) — both its figures here are from
tonight only, and are noted in code as "the weakest of the three
estimates" for exactly that reason: unlike MasterArchive/Binning2k,
there's no second (daytime) sample to take the larger of.

## Implementation Summary

### `src/web.rs`

- `CAPTURE_TMPFS_CAPACITY_BYTES`: `256 * 1024 * 1024`, matching the
  already-established 256 MiB tmpfs size from §7/§2.1.
- `estimated_bytes_per_shot(profile, save_dng) -> u64`: resolves the
  effective DNG inclusion per `validate_raw_policy`'s own rules
  (`MasterArchive` ignores `save_dng`, always includes it;
  `Binning2k` ignores it too, never includes it; `Dci4k` is the one
  profile that actually reads it) rather than trusting the passed
  `save_dng` blindly — a rule using `Binning2k` with a stale
  `save_dng: true` in its config must not report DNG-inflated bytes it
  will never actually produce.
- `ForecastResponse` gained `estimated_total_bytes`,
  `estimated_bytes_per_shot`, `capture_stage_queued_bytes`,
  `capture_tmpfs_capacity_bytes`, and
  `estimated_seconds_to_fill_if_sync_stalled` (`None` when the forecast
  has zero shots — no rate to project from).
- `schedule_forecast` now also reads `queue_usage(&state.capture_dir)`
  (already used by `status()`, same function, same cost) so the fill-time
  estimate counts up from whatever's *actually* sitting in
  `/mnt/capture` right now, not a hypothetical empty queue — a rule set
  that looks safe from empty but is evaluated while 200 MiB is already
  queued (e.g. sync already struggling) needs a warning that reflects
  that, not one that pretends the queue is empty.
- Rate calculation: `bytes_per_hour = estimated_total_bytes /
  horizon_hours`, then `remaining_capacity / bytes_per_hour * 3600` for
  seconds-to-fill — the *average* rate over the whole requested horizon,
  not attempting to model actual capture timing variance within it
  (matches the same "average interval" simplification the Shot
  Forecaster's own `forecast-avg-interval` stat already uses).

### `src/web/scheduler.html` / `scheduler.js`

- Two new stats in the Forecast card's `<dl>`: "Est. storage (48h)"
  (total + per-shot) and "Capture queue (now)" (current queued bytes /
  256 MiB capacity).
- New `#forecast-storage-warning` notice, hidden by default, shown in
  one of two tiers: `warning` (amber, `< 2h` to fill) or `error` (red,
  `< 30min` to fill) — thresholds are a judgment call, not derived from
  anything in the design doc (which only ever gives one illustrative
  number, "~40 min," as an example of what a warning should look like,
  not a specific threshold to encode). Nothing shown above 2h or when
  there are no forecasted shots at all.
- `renderStorageForecast()` is a new function, called from the existing
  `renderForecast()` rather than folded into it — kept the byte/warning
  logic in one place separate from the existing count/interval/table
  logic, matching how `renderRunState`/`renderSaveDiscardButtons` etc.
  already each own one concern in this file.
- `formatBytes`/`formatDurationShort`: `scheduler.js` and `app.js` are
  two independent page scripts with no shared module (a `<script
  src="/app.js">` on the dashboard, a separate `<script
  src="/scheduler.js">` here) — `app.js` already has a `formatBytes`,
  but it isn't reachable from this page, so a local copy was added
  rather than introducing a shared-script refactor out of scope for this
  change.

### `src/web/styles.css`

- New `.notice[data-kind="warning"]` (amber, `var(--orange)`) alongside
  the existing `success`/`error` kinds — the storage warning's two-tier
  distinction needed a third color that wasn't there before.

## Validation

- `cargo fmt --all` / `cargo test --locked`: **91 passed; 0 failed; 0
  ignored** (88 prior + 3 new `estimated_bytes_per_shot` tests: DNG
  ignored for the two profiles that force it either way, DNG strictly
  increases size for `Dci4k`, and the three profiles rank in the
  expected relative order — a transposed-constant regression would be
  caught immediately here rather than only surfacing as a wrong-looking
  number in the UI).
- `cargo clippy --locked --all-targets -- -D warnings`: clean.
- `node --check src/web/scheduler.js` + `npx @biomejs/biome@2.5.14 check`
  (all four web files): clean.
- Deployed via full `./scripts/build-deploy-optic-daemon.sh` (Rust
  change in `web.rs`, not assets-only).
- **Live verification** (2026-09-20, ~03:25 UTC): confirmed
  `GET /api/schedule/forecast?hours=48` against the live daemon returns
  the five new fields with sane values for the real committed
  `MasterArchive` profile: 2,880 shots/48h (the committed `every1min`
  rule, exactly 60×48 — correct), `estimated_bytes_per_shot: 32444605`
  (matches the constant exactly), and
  `estimated_seconds_to_fill_if_sync_stalled: 496` (~8.3 minutes) — this
  closely matches this project's own much earlier historical finding
  ("~8 captures' worth of headroom at MasterArchive+DNG," referenced in
  the design doc's §2.1), an independent cross-check that the whole
  calculation chain (per-shot bytes × rate × remaining capacity) is
  self-consistent with prior real measurements, not just internally
  consistent with itself.
  - Staged a profile switch to `Binning2k` via `POST
    /api/stream/reconfigure` (the endpoint `app.js`'s own camera-settings
    UI uses to stage profile changes — first call used a `save_dng`
    field that doesn't belong on `StreamRequest`, which correctly
    rejected it with a clear "unknown field" error; the real shape is
    `{profile, settings, control_revision}`, confirmed from the error
    message itself rather than guessed twice) and re-read the forecast:
    `estimated_bytes_per_shot` dropped to `530000` (the `Binning2k`
    constant, exactly) and the fill-time estimate rose to `30388`
    seconds (~8.4h) — a ~61.3x increase, matching the ~61.2x ratio
    between the two byte constants almost exactly, confirming the rate
    math scales correctly end to end.
  - Discarded the staged profile change (`POST /api/config/discard`),
    confirmed the committed profile reverted to `master_archive`, and
    `run_state` stayed `Paused` throughout.
- Confirmed served `scheduler.html`/`scheduler.js`/`styles.css` are
  byte-identical to source after deploy.

## Remaining Limitations / Follow-up

- Byte estimates are, as stated throughout, worst-case-leaning
  approximations with real multi-fold variance possible (scene/lighting/
  season-dependent JPEG compression) — not a guarantee, and documented
  as such directly in the constants' own doc comment so this doesn't
  quietly become treated as precise later.
- `Dci4k`'s figures come from a single nighttime session with no
  daytime counterpart to take the larger of, unlike the other two
  profiles — worth re-measuring in daylight at some point and updating
  the constant if it turns out meaningfully larger, the same caveat the
  design doc originally raised, now narrowed from "unmeasured" to
  "measured once, under one lighting condition."
- The 30min/2h warning thresholds are an implementation judgment call,
  not user-confirmed — easy to tune later (`renderStorageForecast` in
  `scheduler.js`) if they prove too noisy or too quiet in practice.
