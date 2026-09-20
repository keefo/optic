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
//   not a runtime check). Today there's exactly one field:
//   constraints: {time_window: {days, start:"HH:MM:SS", end:"HH:MM:SS"} | null}
//   Future Phase 2+ constraint types add more named fields here, not more
//   array entries.

const elements = {
  runState: document.querySelector("#run-state"),
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
  windowEnabled: document.querySelector("#rule-window-enabled"),
  windowFields: document.querySelector("#window-fields"),
  windowStart: document.querySelector("#rule-window-start"),
  windowEnd: document.querySelector("#rule-window-end"),
  windowDays: document.querySelector("#rule-window-days"),
  windowWeekdayPicker: document.querySelector("#window-weekday-picker"),
  cancelBtn: document.querySelector("#rule-form-cancel"),
  saveBtn: document.querySelector("#save-rules"),
  discardBtn: document.querySelector("#discard-rules"),
  forecastCount: document.querySelector("#forecast-count"),
  forecastAvgInterval: document.querySelector("#forecast-avg-interval"),
  forecastWindow: document.querySelector("#forecast-window"),
  forecastBody: document.querySelector("#forecast-body"),
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
  elements.runState.textContent = running ? "Running" : "Paused";
  elements.runState.className = `pill ${running ? "good" : "neutral"}`;
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
  return trigger.kind;
}

function describeConstraints(constraints) {
  const windowConstraint = constraints?.time_window;
  if (!windowConstraint) return "";
  const start = windowConstraint.start.slice(0, 5);
  const end = windowConstraint.end.slice(0, 5);
  const days =
    windowConstraint.days.kind === "Weekdays"
      ? windowConstraint.days.value.join(", ")
      : "every day";
  return ` · window ${start}–${end} (${days})`;
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
  const isInterval = elements.triggerKind.value === "Interval";
  elements.intervalFields.hidden = !isInterval;
  elements.recurringTimeFields.hidden = isInterval;
  elements.weekdayPicker.hidden = isInterval || elements.recurringDays.value !== "Weekdays";

  // The time-window constraint is independent of trigger type — a Fixed
  // Interval rule has no "days" concept of its own, so restricting it to
  // specific weekdays only works through this window, not through the
  // trigger. Shown/hidden the same way regardless of which trigger type
  // is selected above.
  const windowOn = elements.windowEnabled.checked;
  elements.windowFields.hidden = !windowOn;
  elements.windowWeekdayPicker.hidden = !windowOn || elements.windowDays.value !== "Weekdays";
}

function openRuleForm(rule) {
  editingRuleId = rule?.id ?? null;
  elements.ruleFormTitle.textContent = rule ? "Edit rule" : "Add rule";
  elements.ruleLabel.value = rule?.label ?? "";
  elements.ruleSlug.value = rule?.slug ?? "";
  elements.ruleSlug.dataset.userEdited = rule ? "true" : "";

  const trigger = rule?.trigger ?? { kind: "Interval", every_secs: 300, align_to_wall_clock: true };
  elements.triggerKind.value = trigger.kind;
  if (trigger.kind === "Interval") {
    elements.intervalSecs.value = trigger.every_secs;
    elements.intervalAlign.checked = trigger.align_to_wall_clock;
  } else {
    elements.recurringTime.value = trigger.time.slice(0, 5);
    elements.recurringDays.value = trigger.days.kind === "Weekdays" ? "Weekdays" : "Every";
    const checked = trigger.days.kind === "Weekdays" ? trigger.days.value : [];
    for (const box of elements.weekdayPicker.querySelectorAll('input[type="checkbox"]')) {
      box.checked = checked.includes(box.value);
    }
  }

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
  } else {
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

updateTriggerFieldVisibility();
void loadInitial();
setInterval(refreshRunStateOnly, 5000);
