// Shared system-health summary + control footer, included identically on
// every page (index.html, scheduler.html, capture-history.html,
// config.html) via its own <script src="/footer.js" defer> tag alongside
// each page's own script. This one small widget is genuinely identical
// everywhere, unlike Station/Scheduler/capture-history logic which stays
// page-owned per this codebase's established no-shared-module
// convention — a second <script> tag (the same pattern styles.css
// already uses via <link>) is the pragmatic exception for something
// used verbatim four times over duplicating it four times instead.

const footerElements = {
  summary: document.querySelector("#footer-summary"),
  restartDaemon: document.querySelector("#footer-restart-daemon"),
  reboot: document.querySelector("#footer-reboot"),
};

async function footerApi(path, options = {}) {
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

function formatFooterDuration(seconds) {
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  return `${hours}h ${minutes}m`;
}

// `/api/system/status` is only polled every 5s (temp/memory/disk don't
// need per-second freshness), but the clock should visibly tick. Rather
// than polling every second just for that, the last poll's `now` is kept
// as an anchor (`lastSystemStatus`/`lastPollClientTime`) and a separate
// 1s interval re-renders the whole summary from that anchor plus elapsed
// client-side time — accurate to within the local clock's own drift over
// at most 5s, imperceptible for a display like this, and self-corrects
// on every real poll. 5s (not the more typical 15s+ polling interval
// elsewhere in this app) is deliberate here specifically because this
// endpoint is cheap — ~15ms of subprocess/disk work measured live on
// the real Pi, against a near-idle 4-core box — not because the data
// actually needs sub-15s freshness.
let lastSystemStatus = null;
let lastPollClientTime = null;

function estimatedNow() {
  if (!lastSystemStatus) return new Date();
  const anchor = new Date(lastSystemStatus.now).getTime();
  return new Date(anchor + (Date.now() - lastPollClientTime));
}

// Formats the estimated current instant in the *system's* own timezone
// (from `time_sync.timezone`), not the viewer's browser timezone — the
// point is showing what the Pi itself thinks the time is, same reasoning
// as the Config page's celestial-time formatting. Falls back to the
// browser's local zone only when `time_sync` is unavailable (e.g. a
// non-Linux dev target, where `timedatectl` doesn't exist).
function formatSystemNow(system) {
  const timezone = system.time_sync?.timezone;
  return new Intl.DateTimeFormat(undefined, {
    timeZone: timezone,
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
    second: "2-digit",
    timeZoneName: "short",
  }).format(estimatedNow());
}

function renderFooterSummary() {
  if (!lastSystemStatus) return;
  const system = lastSystemStatus;
  const temp = system.cpu_temp_celsius != null ? `${system.cpu_temp_celsius.toFixed(1)}°C` : "—";
  const memoryPct =
    system.memory.total_bytes > 0
      ? Math.round(
          ((system.memory.total_bytes - system.memory.available_bytes) /
            system.memory.total_bytes) *
            100,
        )
      : null;
  const root = system.disks.find((disk) => disk.label === "root");
  const diskPct =
    root && root.total_bytes > 0
      ? Math.round(((root.total_bytes - root.available_bytes) / root.total_bytes) * 100)
      : null;
  const elapsedSincePoll = Math.round((Date.now() - lastPollClientTime) / 1000);
  footerElements.summary.textContent =
    `${formatSystemNow(system)} · CPU ${temp} · Mem ${memoryPct ?? "—"}% · ` +
    `Disk ${diskPct ?? "—"}% · Up ${formatFooterDuration(system.uptime_seconds + elapsedSincePoll)}`;
}

async function refreshFooterStatus() {
  try {
    const response = await footerApi("/api/system/status");
    const { system } = await response.json();
    lastSystemStatus = system;
    lastPollClientTime = Date.now();
    renderFooterSummary();
  } catch (error) {
    lastSystemStatus = null;
    footerElements.summary.textContent = `System status unavailable: ${error.message}`;
  }
}

async function footerSystemAction(path, confirmMessage) {
  if (!window.confirm(confirmMessage)) return;
  try {
    await footerApi(path, { method: "POST" });
    footerElements.summary.textContent = "Command sent…";
  } catch (error) {
    footerElements.summary.textContent = `System action failed: ${error.message}`;
  } finally {
    setTimeout(refreshFooterStatus, 1000);
  }
}

footerElements.restartDaemon.addEventListener("click", () =>
  footerSystemAction(
    "/api/system/restart-daemon",
    "Restart the optic-daemon service? The page will briefly disconnect.",
  ),
);
footerElements.reboot.addEventListener("click", () =>
  footerSystemAction(
    "/api/system/reboot",
    "Reboot the Raspberry Pi? This takes it offline for about a minute.",
  ),
);

void refreshFooterStatus();
setInterval(refreshFooterStatus, 5000);
setInterval(renderFooterSummary, 1000);
