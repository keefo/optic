// Timelapse Scheduler page — rules editor + Shot Forecaster.
// Talks to /api/schedule/* and reuses /api/config/{commit,discard}.
//
// Wire format notes (verified directly against the backend's serde
// derives, not assumed):
// - Trigger::Interval -> {kind:"Interval", every_secs, align_to_wall_clock}
// - Trigger::RecurringTime -> {kind:"RecurringTime", days, time:"HH:MM:SS"}
// - RecurringDays is ADJACENTLY tagged (kind + value), not internally
//   tagged like Trigger — Every -> {kind:"Every"}, Weekdays ->
//   {kind:"Weekdays", value:["Mon", ...]}. This asymmetry is real, not a
//   typo: Weekdays wraps a Vec, which serde can only tag adjacently, not
//   internally (confirmed the hard way on the backend).
// - `constraints` is a fixed-shape OBJECT (`Constraints`), not an array of
//   tagged variants — one named, optional field per constraint type, so a
//   rule can have at most one of each (enforced by the Rust struct itself,
//   not a runtime check):
//   constraints: {
//     time_window, sun_elevation_window, moon_elevation_window,
//     moon_illumination_window, milky_way_elevation_window
//   } — each `{...} | null`.
// - Trigger::Ephemeris -> {kind:"Ephemeris", target, offset_secs}.
//   `target` is ADJACENTLY tagged (type + event):
//   {type:"Solar"|"Lunar"|"MilkyWay", event}. `event` is a plain string for
//   a unit variant (e.g. "Sunset"), or an object keyed by variant name for
//   a struct variant, e.g. {FixedElevation: {degrees, direction}} or
//   {Orientation: {azimuth_degrees}} — serde's default externally-tagged
//   representation (`SolarEvent`/`LunarEvent`/`MilkyWayEvent` have no
//   `#[serde(tag=...)]`, unlike `CelestialTarget`/`Trigger` which do).
//   `direction` is one of "Rising"|"Setting"|"Both".
// - `exposure` (ScheduleConfig.exposure) is edited on the Dashboard (the
//   Scheduled exposure toggle, scheduled-exposure.js); this page only shows
//   it and never sends it, so staging rules can't overwrite it —
//   docs/optic-daemon-exposure-ramping.md §10.

// One entry per named event per body: [wire value, label]. `degrees` marks
// events needing the elevation/azimuth-degrees input; `direction` marks
// ones that also need the Rising/Setting/Both select (Orientation takes an
// azimuth to cross, with no rise/set direction concept).
const EPHEMERIS_EVENTS = {
  Solar: [
    ["SolarNoon", "Solar noon (highest point)"],
    ["Nadir", "Solar midnight (lowest point)"],
    ["Sunrise", "Sunrise"],
    ["Sunset", "Sunset"],
    ["CivilDawn", "Civil dawn"],
    ["CivilDusk", "Civil dusk"],
    ["NauticalDawn", "Nautical dawn"],
    ["NauticalDusk", "Nautical dusk"],
    ["AstronomicalDawn", "Astronomical dawn"],
    ["AstronomicalDusk", "Astronomical dusk"],
    ["GoldenHourMorningStart", "Golden hour start (morning)"],
    ["GoldenHourMorningEnd", "Golden hour end (morning)"],
    ["GoldenHourEveningStart", "Golden hour start (evening)"],
    ["GoldenHourEveningEnd", "Golden hour end (evening)"],
    ["BlueHourMorningStart", "Blue hour start (morning)"],
    ["BlueHourMorningEnd", "Blue hour end (morning)"],
    ["BlueHourEveningStart", "Blue hour start (evening)"],
    ["BlueHourEveningEnd", "Blue hour end (evening)"],
    ["FixedElevation", "Custom elevation crossing", "degrees", "direction"],
  ],
  Lunar: [
    ["Moonrise", "Moonrise"],
    ["Moonset", "Moonset"],
    ["LunarTransit", "Lunar transit (highest point)"],
    ["LunarAntitransit", "Lunar antitransit (lowest point)"],
    ["NewMoon", "New moon"],
    ["FirstQuarter", "First quarter"],
    ["FullMoon", "Full moon"],
    ["LastQuarter", "Last quarter"],
  ],
  MilkyWay: [
    ["CoreRise", "Core rise"],
    ["CoreSet", "Core set"],
    ["CoreTransit", "Core transit (highest point)"],
    ["CoreElevation", "Custom elevation crossing", "degrees", "direction"],
    ["Orientation", "Custom azimuth crossing", "degrees"],
  ],
};

function ephemerisEventMeta(body, eventName) {
  return EPHEMERIS_EVENTS[body]?.find(([value]) => value === eventName);
}

const elements = {
  runStateDetail: document.querySelector("#run-state-detail"),
  nextCapture: document.querySelector("#next-capture"),
  nextCaptureRules: document.querySelector("#next-capture-rules"),
  lastCapture: document.querySelector("#last-capture"),
  runToggleBtn: document.querySelector("#run-toggle-btn"),
  notice: document.querySelector("#notice"),
  ruleFormNotice: document.querySelector("#rule-form-notice"),
  ruleList: document.querySelector("#rule-list"),
  addRuleBtn: document.querySelector("#add-rule-btn"),
  ruleEditorSection: document.querySelector("#rule-editor-section"),
  ruleForm: document.querySelector("#rule-form"),
  ruleFormTitle: document.querySelector("#rule-form-title"),
  ruleLabel: document.querySelector("#rule-label"),
  ruleSlug: document.querySelector("#rule-slug"),
  triggerKind: document.querySelector("#rule-trigger-kind"),
  intervalFields: document.querySelector("#interval-fields"),
  intervalSecs: document.querySelector("#rule-interval-secs"),
  intervalAlign: document.querySelector("#rule-interval-align"),
  recurringTimeFields: document.querySelector("#recurring-time-fields"),
  recurringTime: document.querySelector("#rule-recurring-time"),
  recurringDays: document.querySelector("#rule-recurring-days"),
  weekdayPicker: document.querySelector("#weekday-picker"),
  ephemerisFields: document.querySelector("#ephemeris-fields"),
  ephemerisBody: document.querySelector("#rule-ephemeris-body"),
  ephemerisEvent: document.querySelector("#rule-ephemeris-event"),
  ephemerisDegreesField: document.querySelector("#ephemeris-degrees-field"),
  ephemerisDegreesLabel: document.querySelector("#ephemeris-degrees-label"),
  ephemerisDegrees: document.querySelector("#rule-ephemeris-degrees"),
  ephemerisDirectionField: document.querySelector("#ephemeris-direction-field"),
  ephemerisDirection: document.querySelector("#rule-ephemeris-direction"),
  ephemerisOffset: document.querySelector("#rule-ephemeris-offset"),
  windowEnabled: document.querySelector("#rule-window-enabled"),
  windowFields: document.querySelector("#window-fields"),
  windowStart: document.querySelector("#rule-window-start"),
  windowEnd: document.querySelector("#rule-window-end"),
  windowDays: document.querySelector("#rule-window-days"),
  windowWeekdayPicker: document.querySelector("#window-weekday-picker"),
  sunElevationEnabled: document.querySelector("#rule-sun-elevation-enabled"),
  sunElevationFields: document.querySelector("#sun-elevation-fields"),
  sunElevationMin: document.querySelector("#rule-sun-elevation-min"),
  sunElevationMax: document.querySelector("#rule-sun-elevation-max"),
  moonElevationEnabled: document.querySelector("#rule-moon-elevation-enabled"),
  moonElevationFields: document.querySelector("#moon-elevation-fields"),
  moonElevationMin: document.querySelector("#rule-moon-elevation-min"),
  moonElevationMax: document.querySelector("#rule-moon-elevation-max"),
  moonIlluminationEnabled: document.querySelector("#rule-moon-illumination-enabled"),
  moonIlluminationFields: document.querySelector("#moon-illumination-fields"),
  moonIlluminationMin: document.querySelector("#rule-moon-illumination-min"),
  moonIlluminationMax: document.querySelector("#rule-moon-illumination-max"),
  milkyWayElevationEnabled: document.querySelector("#rule-milky-way-elevation-enabled"),
  milkyWayElevationFields: document.querySelector("#milky-way-elevation-fields"),
  milkyWayElevationMin: document.querySelector("#rule-milky-way-elevation-min"),
  milkyWayElevationMax: document.querySelector("#rule-milky-way-elevation-max"),
  cancelBtn: document.querySelector("#rule-form-cancel"),
  saveBtn: document.querySelector("#save-rules"),
  discardBtn: document.querySelector("#discard-rules"),
  forecastCount: document.querySelector("#forecast-count"),
  forecastAvgInterval: document.querySelector("#forecast-avg-interval"),
  forecastWindow: document.querySelector("#forecast-window"),
  forecastStorage: document.querySelector("#forecast-storage"),
  forecastQueueNow: document.querySelector("#forecast-queue-now"),
  forecastStorageWarning: document.querySelector("#forecast-storage-warning"),
  forecastAdvisories: document.querySelector("#forecast-advisories"),
  forecastBody: document.querySelector("#forecast-body"),
  exposureMode: document.querySelector("#exposure-mode"),
  rampStatus: document.querySelector("#ramp-status"),
};

let rules = [];
let station = null;
let editingRuleId = null;
// Whether the staged config differs from a fresh load/save/discard — drives
// Save rules' disabled state and Discard changes' visibility, so those
// buttons only ever appear actionable when there's actually something to
// act on.
let isDirty = false;

function renderSaveDiscardButtons() {
  elements.saveBtn.disabled = !isDirty;
  elements.discardBtn.hidden = !isDirty;
}

async function api(path, options = {}) {
  const response = await fetch(path, options);
  if (!response.ok) {
    let message = `${response.status} ${response.statusText}`;
    try {
      const body = await response.json();
      message = body.error || message;
    } catch (_) {
      // Keep the HTTP status when the response is not JSON.
    }
    throw new Error(message);
  }
  return response;
}

function showNotice(message, kind = "normal") {
  elements.notice.textContent = message;
  elements.notice.dataset.kind = kind;
}

// Rule-form validation errors (slug conflicts, missing fields) are about
// the currently-open editor, not the page-level scheduler state — they
// belong next to the form's own Save/Cancel buttons, not in the Run
// control section's shared notice, which is a separate part of the page
// (and, in the current layout, a different column entirely).
function showRuleFormNotice(message, kind = "error") {
  elements.ruleFormNotice.textContent = message;
  elements.ruleFormNotice.dataset.kind = kind;
}

function clearRuleFormNotice() {
  elements.ruleFormNotice.textContent = "";
  delete elements.ruleFormNotice.dataset.kind;
}

function slugify(label) {
  return label
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

// --- Load & render current state ---

async function loadInitial() {
  try {
    const response = await api("/api/status");
    const status = await response.json();
    rules = status.config.schedule.rules || [];
    station = status.config.schedule.station || null;
    renderExposureMode(status.config.schedule.exposure);
    // `config` reflects a staged-but-uncommitted preview when one exists
    // (see current_app_config on the backend), so a reload mid-edit must
    // not assume "clean" just because it succeeded — otherwise Save rules
    // stays disabled and Discard changes stays hidden while a real,
    // uncommitted edit sits on disk with no way left to act on it.
    isDirty = Boolean(status.config_staged);
    renderSaveDiscardButtons();
    renderRunState(status.schedule);
    renderRuleList();
    await refreshForecast();
    showNotice("Ready.");
  } catch (error) {
    showNotice(`Failed to load scheduler state: ${error.message}`, "error");
  }
}

function renderRunState(schedule) {
  const running = schedule.run_state === "Running";
  elements.runStateDetail.textContent = schedule.run_state;
  elements.nextCapture.textContent = schedule.next_capture_at
    ? new Date(schedule.next_capture_at).toLocaleString()
    : "—";
  elements.nextCaptureRules.textContent = schedule.next_capture_rules?.length
    ? schedule.next_capture_rules.join(", ")
    : "—";
  if (schedule.last_capture) {
    const lc = schedule.last_capture;
    const when = new Date(lc.at).toLocaleString();
    elements.lastCapture.textContent = lc.success
      ? `${when} — ${lc.rule_slugs.join(", ")} (ok)`
      : `${when} — ${lc.rule_slugs.join(", ")} (failed: ${lc.error})`;
  } else {
    elements.lastCapture.textContent = "—";
  }
  renderRampStatus(schedule.exposure);
  // One button, not two — its own label and target action flip with the
  // current state, rather than showing a disabled "Pause" next to an
  // enabled "Resume" (or vice versa) as two separate always-visible
  // buttons.
  elements.runToggleBtn.textContent = running ? "Pause" : "Resume";
  elements.runToggleBtn.className = running ? "" : "good";
  elements.runToggleBtn.dataset.action = running ? "pause" : "resume";
}

async function refreshRunStateOnly() {
  try {
    const response = await api("/api/status");
    const status = await response.json();
    renderRunState(status.schedule);
  } catch {
    // Transient — leave the last-known display as-is.
  }
}

function renderRuleList() {
  if (rules.length === 0) {
    elements.ruleList.innerHTML = '<p class="empty-state">No rules yet — add one below.</p>';
    return;
  }
  elements.ruleList.innerHTML = "";
  for (const rule of rules) {
    const row = document.createElement("div");
    row.className = "rule-row";
    row.innerHTML = `
      <input type="checkbox" ${rule.enabled ? "checked" : ""} aria-label="Enabled">
      <div class="rule-summary">
        <strong>${escapeHtml(rule.label)}</strong>
        <small>${escapeHtml(rule.slug)} · ${describeTrigger(rule.trigger)}${describeConstraints(rule.constraints)}</small>
      </div>
      <button type="button" class="text-button" data-action="edit">Edit</button>
      <button type="button" class="text-button bad" data-action="remove">Remove</button>
      <span></span>
    `;
    row.querySelector('input[type="checkbox"]').addEventListener("change", (event) => {
      rule.enabled = event.target.checked;
      void stageAndRefreshForecast();
    });
    row.querySelector('[data-action="edit"]').addEventListener("click", () => openRuleForm(rule));
    row.querySelector('[data-action="remove"]').addEventListener("click", () => {
      rules = rules.filter((r) => r.id !== rule.id);
      renderRuleList();
      void stageAndRefreshForecast();
    });
    elements.ruleList.appendChild(row);
  }
}

function describeTrigger(trigger) {
  if (trigger.kind === "Interval") {
    return `every ${trigger.every_secs}s${trigger.align_to_wall_clock ? " (aligned)" : ""}`;
  }
  if (trigger.kind === "RecurringTime") {
    const time = trigger.time.slice(0, 5);
    const days =
      trigger.days.kind === "Every" ? "every day" : `on ${trigger.days.value.join(", ")}`;
    return `${time} ${days}`;
  }
  if (trigger.kind === "Ephemeris") {
    return describeEphemerisTrigger(trigger);
  }
  return trigger.kind;
}

function describeEphemerisTrigger(trigger) {
  const body = trigger.target.type;
  const event = trigger.target.event;
  const bodyLabel = { Solar: "Sun", Lunar: "Moon", MilkyWay: "Milky Way" }[body] ?? body;
  let eventLabel;
  if (typeof event === "string") {
    eventLabel = ephemerisEventMeta(body, event)?.[1] ?? event;
  } else {
    const [variantName, value] = Object.entries(event)[0];
    eventLabel =
      variantName === "Orientation"
        ? `azimuth ${value.azimuth_degrees}°`
        : `${value.degrees}° (${value.direction})`;
  }
  const offsetMin = trigger.offset_secs ? Math.round(trigger.offset_secs / 60) : 0;
  const offsetText = offsetMin ? ` ${offsetMin > 0 ? "+" : ""}${offsetMin}min` : "";
  return `${bodyLabel}: ${eventLabel}${offsetText}`;
}

function describeConstraints(constraints) {
  if (!constraints) return "";
  const parts = [];
  const timeWindow = constraints.time_window;
  if (timeWindow) {
    const start = timeWindow.start.slice(0, 5);
    const end = timeWindow.end.slice(0, 5);
    const days =
      timeWindow.days.kind === "Weekdays" ? timeWindow.days.value.join(", ") : "every day";
    parts.push(`window ${start}–${end} (${days})`);
  }
  const sun = constraints.sun_elevation_window;
  if (sun) parts.push(`sun ${sun.min_deg}°–${sun.max_deg}°`);
  const moon = constraints.moon_elevation_window;
  if (moon) parts.push(`moon ${moon.min_deg}°–${moon.max_deg}°`);
  const illum = constraints.moon_illumination_window;
  if (illum) parts.push(`moon illum ${illum.min_pct}–${illum.max_pct}%`);
  const milkyWay = constraints.milky_way_elevation_window;
  if (milkyWay) parts.push(`Milky Way ${milkyWay.min_deg}°–${milkyWay.max_deg}°`);
  return parts.length ? ` · ${parts.join(", ")}` : "";
}

function escapeHtml(value) {
  const div = document.createElement("div");
  div.textContent = value;
  return div.innerHTML;
}

// --- Add/edit form ---

elements.addRuleBtn.addEventListener("click", () => openRuleForm(null));
elements.cancelBtn.addEventListener("click", closeRuleForm);
elements.triggerKind.addEventListener("change", updateTriggerFieldVisibility);
elements.recurringDays.addEventListener("change", updateTriggerFieldVisibility);
elements.windowEnabled.addEventListener("change", updateTriggerFieldVisibility);
elements.windowDays.addEventListener("change", updateTriggerFieldVisibility);
elements.ephemerisBody.addEventListener("change", () => {
  populateEphemerisEventOptions();
  updateTriggerFieldVisibility();
});
elements.ephemerisEvent.addEventListener("change", updateTriggerFieldVisibility);
elements.sunElevationEnabled.addEventListener("change", updateTriggerFieldVisibility);
elements.moonElevationEnabled.addEventListener("change", updateTriggerFieldVisibility);
elements.moonIlluminationEnabled.addEventListener("change", updateTriggerFieldVisibility);
elements.milkyWayElevationEnabled.addEventListener("change", updateTriggerFieldVisibility);
elements.ruleLabel.addEventListener("input", () => {
  if (editingRuleId === null && !elements.ruleSlug.dataset.userEdited) {
    elements.ruleSlug.value = slugify(elements.ruleLabel.value);
  }
});
elements.ruleSlug.addEventListener("input", () => {
  elements.ruleSlug.dataset.userEdited = "true";
});

// Toggles container-level `hidden` flags only — `#interval-fields`,
// `#recurring-time-fields`, and `#window-fields` each own every field
// inside them, so this function never needs to know about individual
// inputs/labels. That's deliberate: the previous version tracked 7
// separate elements here, and the trigger/constraint container split
// (`.trigger-box`/`.constraint-box` in scheduler.html) exists specifically
// so a case like this can't be missed again.
function updateTriggerFieldVisibility() {
  const kind = elements.triggerKind.value;
  elements.intervalFields.hidden = kind !== "Interval";
  elements.recurringTimeFields.hidden = kind !== "RecurringTime";
  elements.weekdayPicker.hidden =
    kind !== "RecurringTime" || elements.recurringDays.value !== "Weekdays";
  elements.ephemerisFields.hidden = kind !== "Ephemeris";
  if (kind === "Ephemeris") updateEphemerisFieldVisibility();

  // Every constraint below is independent of trigger type — e.g. a Fixed
  // Interval rule has no "days" concept of its own, so restricting it to
  // specific weekdays only works through the time-window constraint, not
  // through the trigger. Shown/hidden the same way regardless of which
  // trigger type is selected above.
  const windowOn = elements.windowEnabled.checked;
  elements.windowFields.hidden = !windowOn;
  elements.windowWeekdayPicker.hidden = !windowOn || elements.windowDays.value !== "Weekdays";
  elements.sunElevationFields.hidden = !elements.sunElevationEnabled.checked;
  elements.moonElevationFields.hidden = !elements.moonElevationEnabled.checked;
  elements.moonIlluminationFields.hidden = !elements.moonIlluminationEnabled.checked;
  elements.milkyWayElevationFields.hidden = !elements.milkyWayElevationEnabled.checked;
}

function populateEphemerisEventOptions() {
  const body = elements.ephemerisBody.value;
  const previous = elements.ephemerisEvent.value;
  elements.ephemerisEvent.innerHTML = EPHEMERIS_EVENTS[body]
    .map(([value, label]) => `<option value="${value}">${escapeHtml(label)}</option>`)
    .join("");
  const stillValid = EPHEMERIS_EVENTS[body].some(([value]) => value === previous);
  if (stillValid) elements.ephemerisEvent.value = previous;
}

function updateEphemerisFieldVisibility() {
  const meta = ephemerisEventMeta(elements.ephemerisBody.value, elements.ephemerisEvent.value);
  const wantsDegrees = meta?.includes("degrees") ?? false;
  const wantsDirection = meta?.includes("direction") ?? false;
  elements.ephemerisDegreesField.hidden = !wantsDegrees;
  elements.ephemerisDirectionField.hidden = !wantsDirection;
  elements.ephemerisDegreesLabel.textContent =
    elements.ephemerisEvent.value === "Orientation"
      ? "Azimuth (degrees, 0-360)"
      : "Elevation (degrees)";
}

function openRuleForm(rule) {
  editingRuleId = rule?.id ?? null;
  elements.ruleFormTitle.textContent = rule ? "Edit rule" : "Add rule";
  elements.ruleLabel.value = rule?.label ?? "";
  elements.ruleSlug.value = rule?.slug ?? "";
  elements.ruleSlug.dataset.userEdited = rule ? "true" : "";

  const trigger = rule?.trigger ?? { kind: "Interval", every_secs: 300, align_to_wall_clock: true };
  elements.triggerKind.value = trigger.kind;

  elements.ephemerisBody.value = trigger.kind === "Ephemeris" ? trigger.target.type : "Solar";
  populateEphemerisEventOptions();

  if (trigger.kind === "Interval") {
    elements.intervalSecs.value = trigger.every_secs;
    elements.intervalAlign.checked = trigger.align_to_wall_clock;
  } else if (trigger.kind === "RecurringTime") {
    elements.recurringTime.value = trigger.time.slice(0, 5);
    elements.recurringDays.value = trigger.days.kind === "Weekdays" ? "Weekdays" : "Every";
    const checked = trigger.days.kind === "Weekdays" ? trigger.days.value : [];
    for (const box of elements.weekdayPicker.querySelectorAll('input[type="checkbox"]')) {
      box.checked = checked.includes(box.value);
    }
  } else if (trigger.kind === "Ephemeris") {
    const event = trigger.target.event;
    if (typeof event === "string") {
      elements.ephemerisEvent.value = event;
      elements.ephemerisDegrees.value = 0;
      elements.ephemerisDirection.value = "Rising";
    } else {
      const [variantName, value] = Object.entries(event)[0];
      elements.ephemerisEvent.value = variantName;
      elements.ephemerisDegrees.value =
        variantName === "Orientation" ? value.azimuth_degrees : value.degrees;
      elements.ephemerisDirection.value = value.direction ?? "Rising";
    }
    elements.ephemerisOffset.value = Math.round((trigger.offset_secs ?? 0) / 60);
  }
  if (trigger.kind !== "Ephemeris") elements.ephemerisOffset.value = 0;

  const windowConstraint = rule?.constraints?.time_window;
  elements.windowEnabled.checked = Boolean(windowConstraint);
  elements.windowStart.value = windowConstraint ? windowConstraint.start.slice(0, 5) : "08:00";
  elements.windowEnd.value = windowConstraint ? windowConstraint.end.slice(0, 5) : "18:00";
  const windowDays = windowConstraint?.days ?? { kind: "Every" };
  elements.windowDays.value = windowDays.kind === "Weekdays" ? "Weekdays" : "Every";
  const windowChecked = windowDays.kind === "Weekdays" ? windowDays.value : [];
  for (const box of elements.windowWeekdayPicker.querySelectorAll('input[type="checkbox"]')) {
    box.checked = windowChecked.includes(box.value);
  }

  const sunWindow = rule?.constraints?.sun_elevation_window;
  elements.sunElevationEnabled.checked = Boolean(sunWindow);
  elements.sunElevationMin.value = sunWindow ? sunWindow.min_deg : -90;
  elements.sunElevationMax.value = sunWindow ? sunWindow.max_deg : 90;

  const moonWindow = rule?.constraints?.moon_elevation_window;
  elements.moonElevationEnabled.checked = Boolean(moonWindow);
  elements.moonElevationMin.value = moonWindow ? moonWindow.min_deg : -90;
  elements.moonElevationMax.value = moonWindow ? moonWindow.max_deg : 90;

  const illuminationWindow = rule?.constraints?.moon_illumination_window;
  elements.moonIlluminationEnabled.checked = Boolean(illuminationWindow);
  elements.moonIlluminationMin.value = illuminationWindow ? illuminationWindow.min_pct : 0;
  elements.moonIlluminationMax.value = illuminationWindow ? illuminationWindow.max_pct : 100;

  const milkyWayWindow = rule?.constraints?.milky_way_elevation_window;
  elements.milkyWayElevationEnabled.checked = Boolean(milkyWayWindow);
  elements.milkyWayElevationMin.value = milkyWayWindow ? milkyWayWindow.min_deg : 10;
  elements.milkyWayElevationMax.value = milkyWayWindow ? milkyWayWindow.max_deg : 90;

  updateTriggerFieldVisibility();
  clearRuleFormNotice();
  elements.ruleEditorSection.hidden = false;
  elements.ruleEditorSection.scrollIntoView({ behavior: "smooth", block: "nearest" });
}

function closeRuleForm() {
  elements.ruleEditorSection.hidden = true;
  clearRuleFormNotice();
  editingRuleId = null;
}

elements.ruleForm.addEventListener("submit", (event) => {
  event.preventDefault();
  const label = elements.ruleLabel.value.trim();
  const slug = elements.ruleSlug.value.trim();
  if (!label || !slug) {
    showRuleFormNotice("Label and slug are required.");
    return;
  }
  if (!/^[a-z0-9][a-z0-9-]*$/.test(slug)) {
    showRuleFormNotice("Slug must be lowercase letters, numbers, and hyphens only.");
    return;
  }
  const duplicate = rules.some(
    (r) => r.id !== editingRuleId && r.slug.toLowerCase() === slug.toLowerCase(),
  );
  if (duplicate) {
    showRuleFormNotice(`Slug "${slug}" is already used by another rule.`);
    return;
  }

  let trigger;
  if (elements.triggerKind.value === "Interval") {
    trigger = {
      kind: "Interval",
      every_secs: Number(elements.intervalSecs.value),
      align_to_wall_clock: elements.intervalAlign.checked,
    };
  } else if (elements.triggerKind.value === "RecurringTime") {
    const days =
      elements.recurringDays.value === "Weekdays"
        ? {
            kind: "Weekdays",
            value: Array.from(
              elements.weekdayPicker.querySelectorAll('input[type="checkbox"]:checked'),
            ).map((box) => box.value),
          }
        : { kind: "Every" };
    trigger = { kind: "RecurringTime", days, time: `${elements.recurringTime.value}:00` };
  } else {
    const body = elements.ephemerisBody.value;
    const eventName = elements.ephemerisEvent.value;
    const meta = ephemerisEventMeta(body, eventName);
    let event;
    if (meta?.includes("direction")) {
      event = {
        [eventName]: {
          degrees: Number(elements.ephemerisDegrees.value),
          direction: elements.ephemerisDirection.value,
        },
      };
    } else if (eventName === "Orientation") {
      event = { [eventName]: { azimuth_degrees: Number(elements.ephemerisDegrees.value) } };
    } else {
      event = eventName;
    }
    trigger = {
      kind: "Ephemeris",
      target: { type: body, event },
      offset_secs: Math.round(Number(elements.ephemerisOffset.value || 0) * 60),
    };
  }

  const constraints = {};
  if (elements.windowEnabled.checked) {
    const windowDays =
      elements.windowDays.value === "Weekdays"
        ? {
            kind: "Weekdays",
            value: Array.from(
              elements.windowWeekdayPicker.querySelectorAll('input[type="checkbox"]:checked'),
            ).map((box) => box.value),
          }
        : { kind: "Every" };
    constraints.time_window = {
      days: windowDays,
      start: `${elements.windowStart.value}:00`,
      end: `${elements.windowEnd.value}:00`,
    };
  }
  if (elements.sunElevationEnabled.checked) {
    constraints.sun_elevation_window = {
      min_deg: Number(elements.sunElevationMin.value),
      max_deg: Number(elements.sunElevationMax.value),
    };
  }
  if (elements.moonElevationEnabled.checked) {
    constraints.moon_elevation_window = {
      min_deg: Number(elements.moonElevationMin.value),
      max_deg: Number(elements.moonElevationMax.value),
    };
  }
  if (elements.moonIlluminationEnabled.checked) {
    constraints.moon_illumination_window = {
      min_pct: Number(elements.moonIlluminationMin.value),
      max_pct: Number(elements.moonIlluminationMax.value),
    };
  }
  if (elements.milkyWayElevationEnabled.checked) {
    constraints.milky_way_elevation_window = {
      min_deg: Number(elements.milkyWayElevationMin.value),
      max_deg: Number(elements.milkyWayElevationMax.value),
    };
  }

  const rule = {
    id: editingRuleId ?? `rule-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
    label,
    slug,
    enabled: true,
    trigger,
    constraints,
  };

  if (editingRuleId) {
    rules = rules.map((r) => (r.id === editingRuleId ? { ...rule, enabled: r.enabled } : r));
  } else {
    rules.push(rule);
  }
  closeRuleForm();
  renderRuleList();
  void stageAndRefreshForecast();
});

// --- Staging, save, discard, forecast ---

async function stageAndRefreshForecast() {
  isDirty = true;
  renderSaveDiscardButtons();
  try {
    await api("/api/schedule/preview", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ station, rules }),
    });
    await refreshForecast();
  } catch (error) {
    showNotice(`Failed to stage schedule: ${error.message}`, "error");
  }
}

async function refreshForecast() {
  try {
    const response = await api("/api/schedule/forecast?hours=48");
    const data = await response.json();
    renderForecast(data);
  } catch (error) {
    showNotice(`Failed to load forecast: ${error.message}`, "error");
  }
}

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KiB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MiB`;
  return `${(bytes / 1024 ** 3).toFixed(1)} GiB`;
}

function formatDurationShort(seconds) {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  return `${(seconds / 3600).toFixed(1)}h`;
}

// Design doc §7/§8: the storage forecast's warning is proactive advice,
// not a guarantee — sync is expected to keep draining the queue
// continuously in normal operation. Two tiers: "error" (red) once the
// projected stalled-sync fill time drops under 30 minutes, "warning"
// (amber) under 2 hours, nothing shown above that.
function renderStorageForecast(data) {
  elements.forecastStorage.textContent =
    data.shots.length === 0
      ? "—"
      : `${formatBytes(data.estimated_total_bytes)} (${formatBytes(data.estimated_bytes_per_shot)}/shot)`;
  elements.forecastQueueNow.textContent = `${formatBytes(data.capture_stage_queued_bytes)} / ${formatBytes(data.capture_tmpfs_capacity_bytes)}`;

  const fillSecs = data.estimated_seconds_to_fill_if_sync_stalled;
  if (fillSecs == null) {
    elements.forecastStorageWarning.hidden = true;
    return;
  }
  const fillText = `At this rate, /mnt/capture would fill in ~${formatDurationShort(fillSecs)} if data sync stalled.`;
  if (fillSecs < 1800) {
    elements.forecastStorageWarning.hidden = false;
    elements.forecastStorageWarning.textContent = fillText;
    elements.forecastStorageWarning.dataset.kind = "error";
  } else if (fillSecs < 7200) {
    elements.forecastStorageWarning.hidden = false;
    elements.forecastStorageWarning.textContent = fillText;
    elements.forecastStorageWarning.dataset.kind = "warning";
  } else {
    elements.forecastStorageWarning.hidden = true;
  }
}

function ruleLabelFor(slug) {
  return rules.find((r) => r.slug === slug)?.label ?? slug;
}

// Design doc §8's dead-rule and overlap advisories — both computed
// server-side (`optic_scheduler::dead_rule_slugs`/`overlap_advisories`)
// against whatever's currently staged, so an edit's effect on these
// shows up here immediately too, same as the rest of the forecast.
function renderAdvisories(data) {
  const items = [];
  for (const slug of data.dead_rule_slugs) {
    items.push(
      `<p class="notice" data-kind="warning">"${escapeHtml(ruleLabelFor(slug))}" is enabled but produces no shots in this window — its trigger/constraints may never be simultaneously satisfiable.</p>`,
    );
  }
  for (const advisory of data.overlap_advisories) {
    const [labelA, labelB] = advisory.rule_slugs.map((slug) => escapeHtml(ruleLabelFor(slug)));
    const start = new Date(advisory.window_start).toLocaleString();
    const end = new Date(advisory.window_end).toLocaleTimeString();
    items.push(
      `<p class="notice" data-kind="warning">"${labelA}" and "${labelB}" are both active ${start}–${end}; combined ~${advisory.combined_shots} shots in that window.</p>`,
    );
  }
  elements.forecastAdvisories.innerHTML = items.join("");
}

function renderForecast(data) {
  elements.forecastCount.textContent = data.shots.length;
  elements.forecastWindow.textContent = `${data.horizon_hours}h`;
  if (data.shots.length < 2) {
    elements.forecastAvgInterval.textContent = "—";
  } else {
    const times = data.shots.map((s) => new Date(s.at).getTime());
    let totalGap = 0;
    for (let i = 1; i < times.length; i += 1) totalGap += times[i] - times[i - 1];
    const avgSecs = Math.round(totalGap / (times.length - 1) / 1000);
    elements.forecastAvgInterval.textContent =
      avgSecs < 120 ? `${avgSecs}s` : `${Math.round(avgSecs / 60)}m`;
  }
  renderStorageForecast(data);
  renderAdvisories(data);
  if (data.shots.length === 0) {
    elements.forecastBody.innerHTML =
      '<tr><td colspan="2" class="empty-state">No upcoming shots in the next 48 hours.</td></tr>';
    return;
  }
  const rows = data.shots
    .slice(0, 200)
    .map(
      (shot) =>
        `<tr><td>${new Date(shot.at).toLocaleString()}</td><td>${shot.rule_slugs.join(", ")}</td></tr>`,
    )
    .join("");
  const overflow =
    data.shots.length > 200
      ? `<tr><td colspan="2" class="empty-state">…and ${data.shots.length - 200} more.</td></tr>`
      : "";
  elements.forecastBody.innerHTML = rows + overflow;
}

elements.saveBtn.addEventListener("click", async () => {
  try {
    await api("/api/config/commit", { method: "POST" });
    isDirty = false;
    renderSaveDiscardButtons();
    showNotice("Schedule saved.", "success");
    await refreshRunStateOnly();
  } catch (error) {
    showNotice(`Failed to save schedule: ${error.message}`, "error");
  }
});

elements.discardBtn.addEventListener("click", async () => {
  try {
    await api("/api/config/discard", { method: "POST" });
    showNotice("Changes discarded.");
    await loadInitial();
  } catch (error) {
    showNotice(`Failed to discard changes: ${error.message}`, "error");
  }
});

elements.runToggleBtn.addEventListener("click", async () => {
  const action = elements.runToggleBtn.dataset.action;
  try {
    const response = await api(`/api/schedule/${action}`, { method: "POST" });
    renderRunState(await response.json());
  } catch (error) {
    showNotice(`Failed to ${action}: ${error.message}`, "error");
  }
});

// --- Scheduled exposure (read-only here; edited on the Dashboard) ---

function renderExposureMode(exposure) {
  const autoRamp = exposure?.mode === "AutoRamp";
  elements.exposureMode.textContent = autoRamp ? "Auto-ramp" : "Dashboard settings";
  elements.exposureMode.className = `pill ${autoRamp ? "good" : "neutral"}`;
}

function formatShutter(us) {
  if (us >= 1e6) {
    return `${(us / 1e6).toFixed(1)} s`;
  }
  return `1/${Math.round(1e6 / us)} s`;
}

function renderRampStatus(snapshot) {
  if (!snapshot) {
    elements.rampStatus.textContent = "—";
    return;
  }
  const parts = [new Date(snapshot.at).toLocaleTimeString()];
  if (snapshot.seed) {
    parts.push("seed (auto exposure/AWB)");
  }
  if (snapshot.exposure_us) {
    parts.push(formatShutter(snapshot.exposure_us));
  }
  if (snapshot.analogue_gain) {
    parts.push(`gain ${snapshot.analogue_gain.toFixed(2)}`);
  }
  if (snapshot.colour_gains) {
    parts.push(`WB ${snapshot.colour_gains.map((gain) => gain.toFixed(2)).join("/")}`);
  }
  parts.push(`target ${snapshot.target_bias_ev.toFixed(1)} EV`);
  if (snapshot.sun_elevation_deg !== null && snapshot.sun_elevation_deg !== undefined) {
    parts.push(`sun ${snapshot.sun_elevation_deg.toFixed(1)}°`);
  }
  parts.push(`max ${formatShutter(snapshot.max_shutter_us)}`);
  elements.rampStatus.textContent = parts.join(" · ");
}

updateTriggerFieldVisibility();
void loadInitial();
setInterval(refreshRunStateOnly, 5000);
