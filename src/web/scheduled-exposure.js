// Scheduled exposure (the day-to-night ramp) on the dashboard: the toggle
// below Shutter, its settings, the locked live Shutter/Gain/White balance
// values, and the preview-only exposure override. Design:
// docs/optic-daemon-exposure-ramping.md §10.
//
// The pure functions at the top are exported for Node tests
// (tests/web/scheduled-exposure.test.js). app.js calls the hooks on
// window.OpticScheduledExposure and listens for the
// "scheduled-exposure-change" event (a user edit: re-request the preview and
// refresh status) and "scheduled-exposure-preview-change" (plan- or
// compare-driven: re-request the preview only).

(() => {
  // Mirrors `RampSettings::default()` in src/exposure_ramp.rs.
  const RAMP_DEFAULTS = {
    min_shutter_us: 100,
    max_shutter_us: 5000000,
    max_gain: 8,
    day_bias_ev: 0,
    night_drop_ev: 2,
    max_step_ev: 1 / 3,
    smoothing: 0.5,
    clip_budget_percent: 1,
  };
  // The Night look slider runs Dark (0) to Bright (NIGHT_LOOK_MAX); the
  // ramp's `night_drop_ev` is its mirror image.
  const NIGHT_LOOK_MAX = 4;
  const MAX_PREVIEW_GAIN = 16;
  // Leave the preview's frame time a little headroom so a clamped shutter
  // never slows the stream down.
  const PREVIEW_FRAME_FRACTION = 0.95;

  function nightLookFromDrop(dropEv) {
    return Math.min(NIGHT_LOOK_MAX, Math.max(0, NIGHT_LOOK_MAX - dropEv));
  }

  function dropFromNightLook(look) {
    return NIGHT_LOOK_MAX - look;
  }

  // A preview can't run a multi-second shutter, so it shows the plan's total
  // exposure (shutter × gain) with the shutter clamped to the preview frame
  // time and the rest moved into gain: same brightness and colour, more
  // noise. `null` plan -> no override; a seeding plan previews with auto
  // exposure and AWB, like the seed frame itself.
  function previewEquivalent(plan, previewFps) {
    if (!plan) return null;
    if (plan.seeding) {
      return { shutter_us: 0, gain: 0, shortfall_ev: 0 };
    }
    const frameUs = Math.floor((1e6 / previewFps) * PREVIEW_FRAME_FRACTION);
    const shutter = Math.max(100, Math.min(plan.shutter_us, frameUs));
    const neededGain = (plan.shutter_us * plan.gain) / shutter;
    const gain = Math.round(Math.min(MAX_PREVIEW_GAIN, Math.max(1, neededGain)) * 100) / 100;
    return {
      shutter_us: shutter,
      gain,
      // How much darker the preview is than the real frame when even 16×
      // gain can't make up for the shorter shutter.
      shortfall_ev: neededGain > MAX_PREVIEW_GAIN ? Math.log2(neededGain / MAX_PREVIEW_GAIN) : 0,
    };
  }

  function formatShutter(us) {
    if (us >= 1e6) return `${(us / 1e6).toFixed(us >= 1e7 ? 0 : 1)} s`;
    if (us >= 1e5) return `${(us / 1e3).toFixed(0)} ms`;
    return `1/${Math.round(1e6 / us)} s`;
  }

  function formatEv(ev) {
    const rounded = Math.round(ev * 10) / 10;
    return `${rounded > 0 ? "+" : rounded < 0 ? "−" : ""}${Math.abs(rounded).toFixed(1)} EV`;
  }

  // Display strings for the locked fields; also used to detect changes.
  // A ramped plan shows its planned values. While seeding, the next frame
  // uses the camera's auto exposure, and so does the preview, so the fields
  // show what auto exposure is choosing right now: `preview` is the latest
  // preview frame's metadata ({ exposureUs, analogueGain, colourGains },
  // app.js `frameMetadata`), or null when no preview is running.
  // White balance is not part of the ramp: scheduled captures use the
  // dashboard's own White balance control, which stays editable, so nothing
  // about it is shown here (user decision, 2026-09-22).
  function describePlan(plan, preview = null) {
    if (!plan) return null;
    if (plan.seeding) {
      const exposureUs = preview?.exposureUs > 0 ? preview.exposureUs : null;
      const gain = preview?.analogueGain > 0 ? preview.analogueGain : null;
      return {
        shutter: exposureUs ? formatShutter(exposureUs) : "Auto",
        // One decimal: auto exposure jitters, and the field shouldn't flicker.
        gain: gain ? `${gain.toFixed(1)}×` : "Auto",
      };
    }
    return {
      shutter: formatShutter(plan.shutter_us),
      gain: `${plan.gain.toFixed(2)}×`,
    };
  }

  // What the preview runs for a ramped plan (design doc §5.7): the next frame
  // with the highlight guard at its target, or without the guard while
  // *hold to compare* is pressed. A daemon without these fields gets the
  // plan itself, as before.
  function previewSource(plan, compare = false) {
    if (!plan || plan.seeding) return plan ?? null;
    const setting = compare ? plan.preview_unguarded : plan.preview;
    return setting ? { seeding: false, shutter_us: setting.shutter_us, gain: setting.gain } : plan;
  }

  // Smallest preview exposure change worth re-requesting the stream for.
  // Each re-request moves the analogue gain, and at night the shadows'
  // colour with it (worklog 2026-09-23, Part 3).
  const PREVIEW_HYSTERESIS_EV = 1 / 6;

  // Whether the preview must be re-requested to show `next` (a
  // previewSource() result or null) when it last ran `lastSent`.
  function previewNeedsUpdate(lastSent, next, thresholdEv = PREVIEW_HYSTERESIS_EV) {
    if (!lastSent || !next) return lastSent !== next;
    if (Boolean(lastSent.seeding) !== Boolean(next.seeding)) return true;
    if (next.seeding) return false;
    const log2 = (source) => Math.log2(source.shutter_us * source.gain);
    return Math.abs(log2(next) - log2(lastSent)) > thresholdEv;
  }

  function formatPercent(fraction) {
    const percent = fraction * 100;
    return `${percent.toFixed(percent < 10 ? 2 : 1)}%`;
  }

  // The highlight guard line under the caption (design doc §5.7).
  // `clippedFraction` is the latest preview frame's X-Optic-Clipped, or null.
  function describeGuardLine(plan, budgetPercent, clippedFraction = null) {
    if (!plan || plan.highlight_active === undefined) return "";
    const clipped =
      clippedFraction == null ? "" : ` · clipped now ${formatPercent(clippedFraction)}`;
    if (!(budgetPercent > 0)) return `Highlight guard off (budget 0%)${clipped}`;
    if (!plan.highlight_active) {
      const sun =
        plan.sun_elevation_deg == null
          ? "no station set"
          : `sun ${plan.sun_elevation_deg.toFixed(1).replace("-", "−")}°`;
      return `Highlight guard inactive (${sun}); it acts only with the sun below the horizon${clipped}`;
    }
    return (
      `Highlight guard · budget ${formatPercent(budgetPercent / 100)}${clipped}` +
      ` · preview ${formatEv(plan.highlight_target_ev ?? 0)}` +
      ` · timelapse ${formatEv(plan.highlight_ev ?? 0)}`
    );
  }

  const pure = {
    RAMP_DEFAULTS,
    nightLookFromDrop,
    dropFromNightLook,
    previewEquivalent,
    formatShutter,
    formatEv,
    describePlan,
    previewSource,
    previewNeedsUpdate,
    describeGuardLine,
  };

  if (typeof module !== "undefined" && module.exports) {
    module.exports = pure;
  }
  if (typeof document === "undefined") return;

  // ---- Browser controller -------------------------------------------------

  const $ = (selector) => document.querySelector(selector);
  const ui = {
    toggle: $("#scheduled-exposure"),
    panel: $("#scheduled-exposure-panel"),
    caption: $("#scheduled-exposure-caption"),
    controls: $("#camera-control-grid"),
    nightLook: $("#ramp-night-look"),
    nightLookValue: $("#ramp-night-look-value"),
    maxShutter: $("#ramp-max-shutter"),
    maxGain: $("#ramp-max-gain"),
    maxGainValue: $("#ramp-max-gain-value"),
    dayBiasValue: $("#ramp-day-bias-value"),
    dayBias: $("#ramp-day-bias"),
    maxStep: $("#ramp-max-step"),
    smoothing: $("#ramp-smoothing"),
    minShutter: $("#ramp-min-shutter"),
    clipBudget: $("#ramp-clip-budget"),
    guardLine: $("#ramp-guard-line"),
    compare: $("#ramp-compare"),
    shutterLive: $("#shutter-ramp-value"),
    gainLive: $("#gain-ramp-value"),
    shutter: $("#shutter"),
    gain: $("#gain"),
  };

  // Each input writes only its own field, so display rounding never leaks
  // back into the staged settings.
  const fields = [
    [ui.nightLook, "night_drop_ev", nightLookFromDrop, dropFromNightLook],
    [ui.maxShutter, "max_shutter_us", (us) => us / 1e6, (s) => Math.round(s * 1e6)],
    [ui.maxGain, "max_gain", (v) => v, (v) => v],
    [ui.dayBias, "day_bias_ev", (v) => Math.round(v * 3) / 3, (v) => Math.round(v * 3) / 3],
    [ui.maxStep, "max_step_ev", (v) => Math.round(v * 100) / 100, (v) => v],
    [ui.smoothing, "smoothing", (v) => v, (v) => v],
    [ui.minShutter, "min_shutter_us", (us) => us / 1000, (ms) => Math.round(ms * 1000)],
    [ui.clipBudget, "clip_budget_percent", (v) => v, (v) => v],
  ];

  let exposure = { mode: "Dashboard" };
  let plan = null;
  let lastShown = null;
  let previewShortfallEv = 0;
  let previewMetadata = null;
  let clippedFraction = null;
  let comparing = false;
  // The preview source the running stream was last requested with.
  let lastSentSource = null;
  let guardLineAt = 0;
  // The guard line follows every preview frame; refresh it at most this often.
  const GUARD_LINE_MIN_INTERVAL_MS = 500;
  // Live preview values can change several times a second; pulse at most
  // this often per field.
  const PULSE_MIN_INTERVAL_MS = 2000;

  function autoRamp() {
    return exposure.mode === "AutoRamp";
  }

  function rampSettings() {
    return { ...RAMP_DEFAULTS, ...(autoRamp() ? exposure : {}) };
  }

  function renderSettings() {
    ui.toggle.checked = autoRamp();
    ui.panel.hidden = !autoRamp();
    const settings = rampSettings();
    for (const [input, field, toInput] of fields) {
      if (document.activeElement !== input) input.value = String(toInput(settings[field]));
    }
    updateSliderLabels();
  }

  function updateSliderLabels() {
    const look = Number(ui.nightLook.value);
    ui.nightLookValue.value =
      look <= 0.5
        ? "Dark"
        : look >= NIGHT_LOOK_MAX - 0.5
          ? "Bright"
          : `${formatEv(-dropFromNightLook(look))} at night`;
    ui.maxGainValue.value = `${Number(ui.maxGain.value).toFixed(1)}×`;
    ui.dayBiasValue.value = formatEv(Number(ui.dayBias.value));
  }

  function setLocked(locked) {
    ui.controls.classList.toggle("ramp-on", locked);
    for (const [input, live] of [
      [ui.shutter, ui.shutterLive],
      [ui.gain, ui.gainLive],
    ]) {
      input.disabled = locked;
      input.hidden = locked;
      live.hidden = !locked;
    }
  }

  function pulse(element) {
    const now = Date.now();
    if (now - (Number(element.dataset.pulsedAt) || 0) < PULSE_MIN_INTERVAL_MS) return;
    element.dataset.pulsedAt = String(now);
    element.classList.remove("ramp-pulse");
    // Restart the animation on every change.
    void element.offsetWidth;
    element.classList.add("ramp-pulse");
  }

  function renderPlan() {
    const locked = autoRamp();
    setLocked(locked);
    if (!locked) {
      lastShown = null;
      return;
    }
    const shown = describePlan(plan, previewMetadata) ?? { shutter: "…", gain: "…" };
    for (const [key, element] of [
      ["shutter", ui.shutterLive],
      ["gain", ui.gainLive],
    ]) {
      if (element.textContent !== shown[key]) {
        element.textContent = shown[key];
        if (lastShown) pulse(element);
      }
    }
    lastShown = shown;
    ui.caption.textContent = captionFor(plan);
    renderGuardLine();
  }

  function captionFor(current) {
    if (!current) return "Waiting for the ramp plan…";
    const sun =
      current.sun_elevation_deg == null
        ? "no station set, so day level"
        : `sun ${current.sun_elevation_deg.toFixed(1).replace("-", "−")}°`;
    const target = `target ${formatEv(current.target_bias_ev)} (${sun})`;
    if (current.seeding) {
      const live = previewMetadata
        ? "showing the camera's live auto exposure"
        : "start the preview to see its auto exposure";
      return `Next scheduled frame is a seed: auto exposure and white balance (${live}), then the ramp takes over · ${target}`;
    }
    const learned = current.ramp_updated_at
      ? ` · learned ${new Date(current.ramp_updated_at).toLocaleTimeString()}`
      : "";
    const darker =
      previewShortfallEv >= 0.1
        ? ` · preview ≈ ${previewShortfallEv.toFixed(1)} EV darker than the frame (preview frames are shorter)`
        : "";
    return `Next scheduled frame · ${target} · shutter cap ${formatShutter(current.max_shutter_us)}${learned}${darker}`;
  }

  function renderGuardLine() {
    guardLineAt = Date.now();
    if (!ui.guardLine) return;
    const text = autoRamp()
      ? describeGuardLine(plan, rampSettings().clip_budget_percent, clippedFraction)
      : "";
    ui.guardLine.textContent = comparing && text ? `Comparing without the guard · ${text}` : text;
    ui.guardLine.hidden = !text;
    if (ui.compare) ui.compare.hidden = !text || !plan || plan.seeding;
  }

  function setComparing(next) {
    if (comparing === next) return;
    comparing = next;
    ui.compare?.setAttribute("aria-pressed", String(comparing));
    renderGuardLine();
    notifyPreviewChange();
  }

  // A user edit: re-request the preview and refresh status straight away.
  function notifyChange() {
    document.dispatchEvent(new CustomEvent("scheduled-exposure-change"));
  }

  // A plan- or compare-driven change: re-request the preview only. Refreshing
  // status here fed back into onStatus and re-requested the stream at
  // network speed (worklog 2026-09-23, Part 3).
  function notifyPreviewChange() {
    document.dispatchEvent(new CustomEvent("scheduled-exposure-preview-change"));
  }

  async function stage(next) {
    const response = await fetch("/api/schedule/exposure", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(next),
    });
    if (!response.ok) {
      let message = `${response.status} ${response.statusText}`;
      try {
        message = (await response.json()).error || message;
      } catch (_) {
        // Keep the HTTP status when the response is not JSON.
      }
      throw new Error(message);
    }
  }

  async function update(next) {
    const previous = exposure;
    exposure = next;
    renderSettings();
    renderPlan();
    try {
      await stage(next);
      ui.caption.dataset.kind = "";
    } catch (error) {
      exposure = previous;
      renderSettings();
      renderPlan();
      ui.caption.textContent = `Scheduled exposure not staged: ${error.message}`;
      ui.caption.dataset.kind = "error";
    }
    notifyChange();
  }

  ui.toggle.addEventListener("change", () => {
    void update(
      ui.toggle.checked ? { mode: "AutoRamp", ...rampSettings() } : { mode: "Dashboard" },
    );
  });
  // Hold to compare: the preview drops the highlight guard while pressed.
  if (ui.compare) {
    ui.compare.addEventListener("pointerdown", (event) => {
      if (event.button !== 0) return;
      setComparing(true);
      // Keeps the release on this button even if the pointer slides off.
      // Best effort: compare must work even where capture is refused.
      try {
        ui.compare.setPointerCapture(event.pointerId);
      } catch (_) {
        // Released by pointerup/pointercancel/blur instead.
      }
    });
    for (const type of ["pointerup", "pointercancel", "lostpointercapture", "blur"]) {
      ui.compare.addEventListener(type, () => setComparing(false));
    }
    ui.compare.addEventListener("keydown", (event) => {
      if ((event.key === " " || event.key === "Enter") && !event.repeat) {
        event.preventDefault();
        setComparing(true);
      }
    });
    ui.compare.addEventListener("keyup", (event) => {
      if (event.key === " " || event.key === "Enter") setComparing(false);
    });
  }

  for (const [input, field, , toWire] of fields) {
    input.addEventListener("input", updateSliderLabels);
    input.addEventListener("change", () => {
      const value = Number.parseFloat(input.value);
      if (!Number.isFinite(value) || !autoRamp()) return;
      void update({ ...exposure, [field]: toWire(value) });
    });
  }

  // Hooks for app.js ------------------------------------------------------

  // Called with the server config on page load and after a discard.
  function applyConfig(config) {
    exposure = config.schedule?.exposure ?? { mode: "Dashboard" };
    renderSettings();
    renderPlan();
  }

  // Called on every status poll.
  function onStatus(status) {
    plan = autoRamp() ? (status.exposure_plan ?? null) : null;
    renderPlan();
    if (previewNeedsUpdate(lastSentSource, previewSource(plan, comparing))) {
      notifyPreviewChange();
    }
  }

  // Called with every rendered preview frame's metadata. Only a seeding
  // plan shows it: a ramped plan's preview runs the brightness-equivalent
  // override, whose values aren't the ones the scheduled frame will use.
  function onPreviewFrame(metadata) {
    previewMetadata = metadata?.exposureUs > 0 ? metadata : null;
    clippedFraction = Number.isFinite(metadata?.clippedFraction) ? metadata.clippedFraction : null;
    if (autoRamp() && plan?.seeding) renderPlan();
    else if (Date.now() - guardLineAt >= GUARD_LINE_MIN_INTERVAL_MS) renderGuardLine();
  }

  // Called when the preview stops, so stale values aren't shown.
  function clearPreview() {
    previewMetadata = null;
    clippedFraction = null;
    if (autoRamp() && plan?.seeding) renderPlan();
    else renderGuardLine();
  }

  // The preview-only override for the next stream request (or null).
  function previewOverride(previewFps) {
    const source = autoRamp() ? previewSource(plan, comparing) : null;
    lastSentSource = source;
    const equivalent = previewEquivalent(source, previewFps);
    previewShortfallEv = equivalent?.shortfall_ev ?? 0;
    if (!equivalent) return null;
    const { shortfall_ev: _shortfall, ...override } = equivalent;
    return override;
  }

  renderSettings();
  renderPlan();
  window.OpticScheduledExposure = {
    ...pure,
    applyConfig,
    onStatus,
    previewOverride,
    onPreviewFrame,
    clearPreview,
  };
})();
