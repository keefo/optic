// Capture History page — queries GET /api/captures with filters, paginated.
// Standalone page (not embedded in the dashboard or scheduler), per the
// design doc's original deferral of "a dashboard panel for querying this
// history" (docs/optic-daemon-capture-log.md §6).

const PROFILE_LABELS = {
  master_archive: "Master Archive",
  dci_4k: "4K DCI Widescreen",
  binning_2k: "2K Binning",
};

const elements = {
  filterQuickRange: document.querySelector("#filter-quick-range"),
  filterSource: document.querySelector("#filter-source"),
  filterProfile: document.querySelector("#filter-profile"),
  filterSuccess: document.querySelector("#filter-success"),
  filterRuleSlug: document.querySelector("#filter-rule-slug"),
  filterSince: document.querySelector("#filter-since"),
  filterUntil: document.querySelector("#filter-until"),
  applyBtn: document.querySelector("#apply-filters"),
  resetBtn: document.querySelector("#reset-filters"),
  notice: document.querySelector("#filters-notice"),
  resultsSummary: document.querySelector("#results-summary"),
  resultsBody: document.querySelector("#results-body"),
  prevBtn: document.querySelector("#prev-page"),
  nextBtn: document.querySelector("#next-page"),
  pageIndicator: document.querySelector("#page-indicator"),
};

const LIMIT = 50;
let offset = 0;
let lastTotal = 0;

function showNotice(message, kind = "normal") {
  elements.notice.textContent = message;
  elements.notice.dataset.kind = kind;
}

function escapeHtml(value) {
  const div = document.createElement("div");
  div.textContent = value;
  return div.innerHTML;
}

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KiB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MiB`;
  return `${(bytes / 1024 ** 3).toFixed(1)} GiB`;
}

function formatDuration(ms) {
  return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;
}

// datetime-local values have no timezone marker, so `new Date(value)`
// parses them as local time — matching what the picker's field shows the
// user, and consistent with how the rest of this UI renders timestamps
// via toLocaleString().
function localInputToUnixMs(value) {
  if (!value) return undefined;
  const parsed = new Date(value).getTime();
  return Number.isNaN(parsed) ? undefined : parsed;
}

// Inverse of localInputToUnixMs: renders a Date as a datetime-local value
// in local time (`toISOString` would render UTC, which would silently
// shift the field's displayed value away from what the quick-range
// selection actually means).
function formatForDatetimeLocal(date) {
  const pad = (n) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function buildQuery() {
  const params = new URLSearchParams();
  if (elements.filterSource.value) params.set("source", elements.filterSource.value);
  if (elements.filterProfile.value) params.set("profile", elements.filterProfile.value);
  if (elements.filterSuccess.value) params.set("success", elements.filterSuccess.value);
  if (elements.filterRuleSlug.value.trim()) {
    params.set("rule_slug", elements.filterRuleSlug.value.trim());
  }
  const since = localInputToUnixMs(elements.filterSince.value);
  if (since !== undefined) params.set("since", String(since));
  const until = localInputToUnixMs(elements.filterUntil.value);
  if (until !== undefined) params.set("until", String(until));
  params.set("limit", String(LIMIT));
  params.set("offset", String(offset));
  return params;
}

async function load() {
  elements.resultsBody.innerHTML =
    '<tr><td colspan="7" class="empty-state">Loading&hellip;</td></tr>';
  try {
    const response = await fetch(`/api/captures?${buildQuery()}`);
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
    const data = await response.json();
    lastTotal = data.total;
    render(data);
    showNotice("Ready.");
  } catch (error) {
    elements.resultsBody.innerHTML = `<tr><td colspan="7" class="empty-state">Failed to load: ${escapeHtml(error.message)}</td></tr>`;
    showNotice(`Failed to load capture history: ${error.message}`, "error");
  }
}

function render(data) {
  const { entries, total, limit } = data;

  if (total === 0) {
    elements.resultsSummary.textContent = "No captures match these filters.";
  } else {
    const from = offset + 1;
    const to = Math.min(offset + entries.length, total);
    elements.resultsSummary.textContent = `${from}-${to} of ${total}`;
  }
  elements.pageIndicator.textContent =
    total === 0 ? "—" : `Page ${Math.floor(offset / limit) + 1} of ${Math.ceil(total / limit)}`;
  elements.prevBtn.disabled = offset === 0;
  elements.nextBtn.disabled = offset + entries.length >= total;

  if (entries.length === 0) {
    elements.resultsBody.innerHTML =
      '<tr><td colspan="7" class="empty-state">No captures match these filters.</td></tr>';
    return;
  }

  elements.resultsBody.innerHTML = entries
    .map((entry) => {
      const when = new Date(entry.completed_at_unix_ms).toLocaleString();
      const source = entry.source === "scheduler" ? "Scheduler" : "Web UI";
      const rules = entry.triggered_by.length ? escapeHtml(entry.triggered_by.join(", ")) : "—";
      const profile = PROFILE_LABELS[entry.profile] ?? escapeHtml(entry.profile);
      const outcome = entry.success
        ? '<span class="pill good">Success</span>'
        : `<span class="pill bad" title="${escapeHtml(entry.error ?? "")}">Failed</span>`;
      const duration = formatDuration(entry.duration_ms);
      const bytes = entry.success ? formatBytes(entry.bytes) : "—";
      return `<tr><td>${when}</td><td>${source}</td><td>${rules}</td><td>${profile}</td><td>${outcome}</td><td>${duration}</td><td>${bytes}</td></tr>`;
    })
    .join("");
}

elements.applyBtn.addEventListener("click", () => {
  offset = 0;
  void load();
});

elements.resetBtn.addEventListener("click", () => {
  elements.filterQuickRange.value = "";
  elements.filterSource.value = "";
  elements.filterProfile.value = "";
  elements.filterSuccess.value = "";
  elements.filterRuleSlug.value = "";
  elements.filterSince.value = "";
  elements.filterUntil.value = "";
  offset = 0;
  void load();
});

// Quick range fills "Captured after" (and clears "Captured before", since
// "last N hours" means "through now") and applies immediately — the whole
// point of a quick filter is not needing a separate Apply click.
elements.filterQuickRange.addEventListener("change", () => {
  const hours = elements.filterQuickRange.value;
  if (!hours) return;
  elements.filterSince.value = formatForDatetimeLocal(
    new Date(Date.now() - Number(hours) * 3600 * 1000),
  );
  elements.filterUntil.value = "";
  offset = 0;
  void load();
});

// Editing either date field by hand no longer matches whatever quick
// range was last picked, so drop back to "Custom / Any" rather than
// leave a stale, misleading selection showing.
elements.filterSince.addEventListener("input", () => {
  elements.filterQuickRange.value = "";
});
elements.filterUntil.addEventListener("input", () => {
  elements.filterQuickRange.value = "";
});

elements.prevBtn.addEventListener("click", () => {
  offset = Math.max(0, offset - LIMIT);
  void load();
});

elements.nextBtn.addEventListener("click", () => {
  if (offset + LIMIT < lastTotal) {
    offset += LIMIT;
    void load();
  }
});

void load();

// System events (GET /api/events): boots, reboots, shutdowns, and daemon
// starts/stops — docs/optic-daemon-system-events.md. Independent of the
// capture filters above.
const eventsElements = {
  body: document.querySelector("#events-body"),
  refresh: document.querySelector("#refresh-events"),
};

const PREVIOUS_BOOT_ENDINGS = {
  reboot: "Previous boot ended with a reboot.",
  shutdown: "Previous boot ended with a shutdown.",
  daemon_stopped: "The daemon was stopped earlier; how the Pi went down was not recorded.",
  unexpected: "Previous boot ended unexpectedly (power loss, crash, or watchdog reset).",
};

function formatDowntime(ms) {
  const seconds = Math.max(0, Math.round(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) {
    return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
  }
  return `${Math.floor(seconds / 86400)}d ${Math.floor((seconds % 86400) / 3600)}h`;
}

// Returns [label, pillKind, details]; details is already HTML-escaped.
function describeEvent(event) {
  const detail = event.detail ?? {};
  switch (event.kind) {
    case "boot": {
      const previous = detail.previous_boot;
      if (!previous) return ["Pi booted", "neutral", "First boot recorded."];
      const ending = PREVIOUS_BOOT_ENDINGS[previous.ended] ?? escapeHtml(previous.ended);
      const downtime = formatDowntime(event.occurred_at_unix_ms - previous.last_event_at_unix_ms);
      return [
        "Pi booted",
        previous.ended === "unexpected" ? "bad" : "neutral",
        `${ending} Down for about ${downtime}.`,
      ];
    }
    case "reboot":
      return ["Pi rebooting", "neutral", escapeHtml(detail.target ?? "")];
    case "shutdown":
      return ["Pi shutting down", "neutral", escapeHtml(detail.target ?? "")];
    case "daemon_start":
      return ["Daemon started", "good", `Version ${escapeHtml(detail.version ?? "?")}`];
    case "daemon_stop":
      return ["Daemon stopped", "neutral", ""];
    case "reboot_requested":
      return ["Reboot requested", "neutral", "From the dashboard"];
    case "shutdown_requested":
      return ["Shutdown requested", "neutral", "From the dashboard"];
    case "daemon_restart_requested":
      return ["Daemon restart requested", "neutral", "From the dashboard"];
    case "request_failed":
      return [
        "Request failed",
        "bad",
        `${escapeHtml(detail.action ?? "?")}: ${escapeHtml(detail.error ?? "")}`,
      ];
    default:
      return [escapeHtml(event.kind), "neutral", ""];
  }
}

async function loadEvents() {
  eventsElements.body.innerHTML =
    '<tr><td colspan="3" class="empty-state">Loading&hellip;</td></tr>';
  try {
    const response = await fetch("/api/events?limit=50");
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
    const { events } = await response.json();
    if (events.length === 0) {
      eventsElements.body.innerHTML =
        '<tr><td colspan="3" class="empty-state">No system events recorded yet.</td></tr>';
      return;
    }
    eventsElements.body.innerHTML = events
      .map((event) => {
        const when = new Date(event.occurred_at_unix_ms).toLocaleString();
        const [label, kind, details] = describeEvent(event);
        return `<tr><td>${when}</td><td><span class="pill ${kind}">${label}</span></td><td>${details}</td></tr>`;
      })
      .join("");
  } catch (error) {
    eventsElements.body.innerHTML = `<tr><td colspan="3" class="empty-state">Failed to load system events: ${escapeHtml(error.message)}</td></tr>`;
  }
}

eventsElements.refresh.addEventListener("click", () => void loadEvents());
void loadEvents();
