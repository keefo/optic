// Config page — Station (used by Solar/Lunar/Milky Way scheduler triggers
// and constraints) plus system Time & NTP status/control. Moved off the
// dashboard (2026-09-20) into its own dedicated page, same reasoning as
// the scheduler/capture-history pages: a real page with its own layout,
// not a card competing for space on the operator's main working view.

const elements = {
  stationLatitude: document.querySelector("#station-latitude"),
  stationLongitude: document.querySelector("#station-longitude"),
  stationElevation: document.querySelector("#station-elevation"),
  stationTimezone: document.querySelector("#station-timezone"),
  saveStation: document.querySelector("#save-station"),
  discardStation: document.querySelector("#discard-station"),
  stationNotice: document.querySelector("#station-notice"),
  celestialEmpty: document.querySelector("#celestial-empty"),
  celestialGroups: document.querySelector("#celestial-groups"),
  timeNow: document.querySelector("#time-now"),
  timeSyncCue: document.querySelector("#time-sync-cue"),
  timeTimezone: document.querySelector("#time-timezone"),
  timeNtpEnabled: document.querySelector("#time-ntp-enabled"),
  timeSynchronized: document.querySelector("#time-synchronized"),
  ntpSyncNow: document.querySelector("#ntp-sync-now"),
  timeNotice: document.querySelector("#time-notice"),
};

let scheduleRulesCache = [];
let stationFieldsInitialized = false;

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

function showStationNotice(message, kind = "normal") {
  elements.stationNotice.textContent = message;
  elements.stationNotice.dataset.kind = kind;
}

function showTimeNotice(message, kind = "normal") {
  elements.timeNotice.textContent = message;
  elements.timeNotice.dataset.kind = kind;
}

// Populated once from GET /api/timezones on load — the backend's own
// chrono-tz TZ_VARIANTS list, so the dropdown can never submit a name the
// backend wouldn't recognize (previously a free-text field: a typo'd zone
// silently fell back to UTC with no warning — see
// optic_scheduler.rs::Station::tz).
async function populateTimezoneOptions() {
  try {
    const response = await api("/api/timezones");
    const zones = await response.json();
    elements.stationTimezone.innerHTML =
      '<option value="">Not set</option>' +
      zones.map((zone) => `<option value="${zone}">${zone}</option>`).join("");
  } catch (error) {
    showStationNotice(`Failed to load timezone list: ${error.message}`, "error");
  }
}

function populateStationFields(station) {
  elements.stationLatitude.value = station?.latitude ?? "";
  elements.stationLongitude.value = station?.longitude ?? "";
  elements.stationElevation.value = station?.elevation_m ?? "";
  elements.stationTimezone.value = station?.timezone ?? "";
}

function escapeHtml(value) {
  const div = document.createElement("div");
  div.textContent = value;
  return div.innerHTML;
}

function celestialPreviewReady() {
  return (
    elements.stationLatitude.value.trim() !== "" && elements.stationLongitude.value.trim() !== ""
  );
}

// Formats a UTC instant in the Station's own selected timezone — not the
// viewer's browser timezone, which could be anywhere. Falls back to the
// browser's local zone only if no Station timezone is selected yet.
function formatCelestialTime(iso) {
  if (!iso) return "Not found in the next search window";
  const timezone = elements.stationTimezone.value || undefined;
  return new Intl.DateTimeFormat(undefined, {
    timeZone: timezone,
    weekday: "short",
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  }).format(new Date(iso));
}

const CELESTIAL_GROUP_ORDER = ["Sun", "Moon", "Milky Way"];

function renderCelestialPreview(data) {
  elements.celestialEmpty.hidden = true;
  elements.celestialGroups.hidden = false;
  const groups = CELESTIAL_GROUP_ORDER.map((group) => {
    const items = data.items.filter((item) => item.group === group);
    if (items.length === 0) return "";
    const rows = items
      .map(
        (item) =>
          `<div><dt>${escapeHtml(item.label)}</dt><dd>${formatCelestialTime(item.at)}</dd></div>`,
      )
      .join("");
    const extra =
      group === "Moon"
        ? `<div><dt>Illumination</dt><dd>${data.moon_illumination_pct.toFixed(0)}% (${data.moon_waxing ? "waxing" : "waning"})</dd></div>`
        : "";
    return `<div class="celestial-group"><p class="eyebrow">${group.toUpperCase()}</p><dl>${rows}${extra}</dl></div>`;
  });
  elements.celestialGroups.innerHTML = groups.join("");
}

async function refreshCelestialPreview() {
  if (!celestialPreviewReady()) {
    elements.celestialEmpty.hidden = false;
    elements.celestialEmpty.textContent =
      "Set latitude and longitude above to preview celestial times.";
    elements.celestialGroups.hidden = true;
    return;
  }
  const params = new URLSearchParams({
    latitude: elements.stationLatitude.value.trim(),
    longitude: elements.stationLongitude.value.trim(),
    elevation_m: elements.stationElevation.value.trim() || "0",
  });
  try {
    const response = await api(`/api/celestial-preview?${params}`);
    renderCelestialPreview(await response.json());
  } catch (error) {
    elements.celestialEmpty.hidden = false;
    elements.celestialEmpty.textContent = `Failed to load celestial preview: ${error.message}`;
    elements.celestialGroups.hidden = true;
  }
}

// Returns `{ station, ok }`: `station` is `null` (clear it) when every
// field is blank/unset, the parsed `Station` when every field is filled,
// or `ok: false` when only some fields are filled — a `Station` needs all
// three of lat/long/timezone to mean anything, so a partial fill is
// neither "set" nor "cleared."
function stationFromFields() {
  const lat = elements.stationLatitude.value.trim();
  const lon = elements.stationLongitude.value.trim();
  const tz = elements.stationTimezone.value;
  const anyFilled = lat !== "" || lon !== "" || tz !== "";
  const allFilled = lat !== "" && lon !== "" && tz !== "";
  if (!anyFilled) return { station: null, ok: true };
  if (!allFilled) return { station: null, ok: false };
  const elevation = elements.stationElevation.value.trim();
  return {
    station: {
      latitude: Number(lat),
      longitude: Number(lon),
      elevation_m: elevation === "" ? 0 : Number(elevation),
      timezone: tz,
    },
    ok: true,
  };
}

async function stageStation() {
  const { station, ok } = stationFromFields();
  if (!ok) {
    showStationNotice(
      "Fill in latitude, longitude, and timezone together, or clear all three to remove the station.",
      "error",
    );
    return;
  }
  try {
    await api("/api/schedule/preview", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ station, rules: scheduleRulesCache }),
    });
    showStationNotice("Staged — click Save station to apply.");
  } catch (error) {
    showStationNotice(`Failed to stage station: ${error.message}`, "error");
  } finally {
    refreshStatus();
  }
}

for (const field of [
  elements.stationLatitude,
  elements.stationLongitude,
  elements.stationElevation,
  elements.stationTimezone,
]) {
  field.addEventListener("change", stageStation);
  field.addEventListener("change", refreshCelestialPreview);
}

elements.saveStation.addEventListener("click", async () => {
  try {
    await api("/api/config/commit", { method: "POST" });
    // Also keep the Pi's system clock timezone in sync with the Station's
    // — the user's explicit choice, rather than treating these as two
    // independent settings (see /api/system/timezone in web.rs). Skipped
    // when the Station has no timezone set (e.g. it was just cleared).
    const timezone = elements.stationTimezone.value;
    if (timezone) {
      await api("/api/system/timezone", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ timezone }),
      });
      showStationNotice(`Station saved. System clock timezone set to ${timezone}.`, "success");
    } else {
      showStationNotice("Station saved.", "success");
    }
  } catch (error) {
    showStationNotice(`Failed to save station: ${error.message}`, "error");
  } finally {
    refreshStatus();
    refreshTimeSync();
  }
});

elements.discardStation.addEventListener("click", async () => {
  try {
    await api("/api/config/discard", { method: "POST" });
    showStationNotice("Changes discarded.");
  } catch (error) {
    showStationNotice(`Failed to discard changes: ${error.message}`, "error");
  } finally {
    // Force the next refreshStatus() to re-populate the fields from the
    // just-reverted server state, rather than leaving the discarded
    // in-progress edit sitting in the inputs.
    stationFieldsInitialized = false;
    refreshStatus();
  }
});

async function refreshStatus() {
  try {
    const response = await api("/api/status");
    const status = await response.json();
    scheduleRulesCache = status.config.schedule.rules || [];
    if (!stationFieldsInitialized) {
      populateStationFields(status.config.schedule.station);
      stationFieldsInitialized = true;
      refreshCelestialPreview();
    }
    elements.saveStation.disabled = !status.config_staged;
    elements.discardStation.hidden = !status.config_staged;
  } catch (error) {
    showStationNotice(`Failed to load config: ${error.message}`, "error");
  }
}

// `/api/system/status` is only polled every 15s (NTP status doesn't need
// per-second freshness), but the clock should visibly tick. The last
// poll's `now` is kept as an anchor and a separate 1s interval
// (`tickCurrentTime`) re-renders just the clock from that anchor plus
// elapsed client-side time, without re-fetching — same pattern as
// footer.js's ticking clock, see that file's comment for the full
// reasoning.
// Named distinctly from footer.js's identical-in-spirit
// lastSystemStatus/lastPollClientTime/estimatedNow — both scripts load
// as plain (non-module) <script> tags on this page and therefore share
// one global scope, so identical top-level `let`/`function` names across
// the two files would throw a SyntaxError and break both.
let clockSystemStatus = null;
let clockPollClientTime = null;

function estimatedSystemNow() {
  if (!clockSystemStatus) return new Date();
  const anchor = new Date(clockSystemStatus.now).getTime();
  return new Date(anchor + (Date.now() - clockPollClientTime));
}

// Set while a "Sync now" request waits for systemd-timesyncd's next
// successful reply: `baseline` is `last_synced_at` read just before the
// request, so any newer value proves a fresh sync happened.
let pendingNtpSync = null;
const NTP_SYNC_TIMEOUT_MS = 15000;

function formatSystemTime(date, options) {
  return new Intl.DateTimeFormat(undefined, {
    timeZone: clockSystemStatus?.time_sync?.timezone,
    ...options,
  }).format(date);
}

// "just now" / "N min ago" / "N h ago". Both ends are Pi timestamps
// (last_synced_at and the ticking estimate of the Pi's `now`), so a
// browser clock that is off doesn't skew it.
function formatSyncAge(lastSyncedAt) {
  const seconds = Math.max(0, (estimatedSystemNow() - new Date(lastSyncedAt)) / 1000);
  if (seconds < 60) return "just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min ago`;
  return `${Math.floor(seconds / 3600)} h ago`;
}

function renderSyncCue() {
  const cue = elements.timeSyncCue;
  const timeSync = clockSystemStatus?.time_sync;
  if (!timeSync) {
    cue.hidden = true;
    return;
  }
  cue.hidden = false;
  if (pendingNtpSync) {
    cue.textContent = "Syncing…";
    cue.className = "pill neutral";
    cue.title = "Waiting for a time server reply";
  } else if (!timeSync.synchronized) {
    cue.textContent = "Not synced";
    cue.className = "pill bad";
    cue.title = "The system clock is not synchronized to a time server";
  } else if (timeSync.last_synced_at) {
    cue.textContent = `✓ Synced ${formatSyncAge(timeSync.last_synced_at)}`;
    cue.className = "pill good";
    cue.title = `Last time server reply: ${formatSystemTime(new Date(timeSync.last_synced_at), {
      dateStyle: "medium",
      timeStyle: "medium",
    })}`;
  } else {
    cue.textContent = "✓ Synced";
    cue.className = "pill good";
    cue.title = "Synchronized to a time server";
  }
}

function tickCurrentTime() {
  if (!clockSystemStatus) return;
  elements.timeNow.textContent = formatSystemTime(estimatedSystemNow(), {
    dateStyle: "medium",
    timeStyle: "medium",
  });
  renderSyncCue();
}

// `now`/timezone-aware clock is always present (a plain server-side
// clock read, not gated behind timedatectl succeeding — see
// system_status.rs::SystemStatus::now); NTP status is gated behind
// `time_sync` being available (e.g. absent on a non-Linux dev target).
function renderTimeSync(system) {
  tickCurrentTime();

  const timeSync = system.time_sync;
  if (!timeSync) {
    elements.timeTimezone.textContent = "Unavailable";
    elements.timeNtpEnabled.textContent = "—";
    elements.timeNtpEnabled.className = "pill neutral";
    elements.timeSynchronized.textContent = "—";
    elements.timeSynchronized.className = "pill neutral";
    return;
  }
  elements.timeTimezone.textContent = timeSync.timezone;
  elements.timeNtpEnabled.textContent = timeSync.ntp_enabled ? "Yes" : "No";
  elements.timeNtpEnabled.className = `pill ${timeSync.ntp_enabled ? "good" : "bad"}`;
  elements.timeSynchronized.textContent = timeSync.synchronized ? "Yes" : "No";
  elements.timeSynchronized.className = `pill ${timeSync.synchronized ? "good" : "bad"}`;
}

async function refreshTimeSync() {
  try {
    const response = await api("/api/system/status");
    const data = await response.json();
    clockSystemStatus = data.system;
    clockPollClientTime = Date.now();
    renderTimeSync(data.system);
    checkPendingNtpSync();
  } catch (error) {
    showTimeNotice(`Failed to load time status: ${error.message}`, "error");
  }
}

function isNewerSync(lastSyncedAt, baseline) {
  if (!lastSyncedAt) return false;
  return baseline === null || new Date(lastSyncedAt) > new Date(baseline);
}

// Second state of "Sync now": settles to "Synchronized" once timesyncd
// reports a reply newer than the baseline, or to a warning after
// NTP_SYNC_TIMEOUT_MS. Never claims success from the request alone.
function checkPendingNtpSync() {
  if (!pendingNtpSync) return;
  const lastSyncedAt = clockSystemStatus?.time_sync?.last_synced_at ?? null;
  if (isNewerSync(lastSyncedAt, pendingNtpSync.baseline)) {
    finishNtpSync();
    const at = formatSystemTime(new Date(lastSyncedAt), { timeStyle: "medium" });
    showTimeNotice(`Synchronized with the time server at ${at}.`, "success");
  } else if (Date.now() > pendingNtpSync.deadline) {
    finishNtpSync();
    showTimeNotice(
      "Sync requested, but no time server reply yet. The clock keeps its last sync; check the Pi's network.",
      "warning",
    );
  }
}

function finishNtpSync() {
  clearInterval(pendingNtpSync.poll);
  pendingNtpSync = null;
  elements.ntpSyncNow.disabled = false;
  renderSyncCue();
}

elements.ntpSyncNow.addEventListener("click", async () => {
  elements.ntpSyncNow.disabled = true;
  showTimeNotice("Syncing…");
  try {
    // Fresh baseline, not the up-to-15s-old poll, so a routine sync that
    // happened meanwhile isn't mistaken for this one.
    await refreshTimeSync();
    const baseline = clockSystemStatus?.time_sync?.last_synced_at ?? null;
    await api("/api/system/ntp-sync", { method: "POST" });
    showTimeNotice("Sync requested — waiting for a time server reply…");
    pendingNtpSync = {
      baseline,
      deadline: Date.now() + NTP_SYNC_TIMEOUT_MS,
      poll: setInterval(refreshTimeSync, 1000),
    };
    renderSyncCue();
  } catch (error) {
    showTimeNotice(`Failed to sync: ${error.message}`, "error");
    elements.ntpSyncNow.disabled = false;
  }
});

void populateTimezoneOptions().then(refreshStatus);
void refreshTimeSync();
setInterval(refreshStatus, 3000);
setInterval(refreshTimeSync, 15000);
setInterval(tickCurrentTime, 1000);
