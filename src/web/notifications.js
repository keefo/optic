// Config page — Notifications (ntfy) card: channel, daily digest and
// heartbeat settings, stored in config.json's `notifications` section
// (docs/optic-daemon-digest-heartbeat.md §5–6). Its own script, loaded
// only by config.html, beside config.js.
//
// The ntfy topic and token are write-only: GET /api/notifications only says
// whether they are set, masked as first 4 + **** + last 4, and the page
// shows that mask as the field's value (see secretField).

const notif = {
  source: document.querySelector("#notif-source"),
  enabled: document.querySelector("#notif-enabled"),
  station: document.querySelector("#notif-station"),
  server: document.querySelector("#notif-server"),
  topic: document.querySelector("#notif-topic"),
  token: document.querySelector("#notif-token"),
  generateTopic: document.querySelector("#notif-generate-topic"),
  digestEnabled: document.querySelector("#notif-digest-enabled"),
  digestTime: document.querySelector("#notif-digest-time"),
  digestStatus: document.querySelector("#notif-digest-status"),
  hbEnabled: document.querySelector("#notif-hb-enabled"),
  hbInterval: document.querySelector("#notif-hb-interval"),
  hbDelay: document.querySelector("#notif-hb-delay"),
  hbStatus: document.querySelector("#notif-hb-status"),
  save: document.querySelector("#notif-save"),
  test: document.querySelector("#notif-test"),
  digestNow: document.querySelector("#notif-digest-now"),
  notice: document.querySelector("#notif-notice"),
};

// The last saved view; the heartbeat sequence ID is not editable here and is
// sent back unchanged.
let notifSaved = null;
let notifDirty = false;

// Only shown when something needs attention; nothing for normal saved settings.
const SOURCE_LABELS = {
  config: "",
  alerts_file: "Using the old alerts.json (it could not be imported)",
  none: "Not set up yet",
};

async function notifApi(path, options = {}) {
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
  return response.json();
}

function showNotifNotice(message, kind = "normal") {
  notif.notice.textContent = message;
  notif.notice.dataset.kind = kind;
}

// On/Off pill switches (Notifications, Digest, Heartbeat).
function setToggle(button, on) {
  button.setAttribute("aria-checked", String(on));
  button.textContent = on ? "On" : "Off";
}

function isToggleOn(button) {
  return button.getAttribute("aria-checked") === "true";
}

// The form's values as last loaded or saved; Save is enabled only while
// the form differs from it, so undoing a change disables Save again.
let notifSavedForm = null;

function notifFormState() {
  return JSON.stringify([
    isToggleOn(notif.enabled),
    notif.station.value,
    notif.server.value,
    notif.topic.value,
    notif.token.value,
    isToggleOn(notif.digestEnabled),
    notif.digestTime.value,
    isToggleOn(notif.hbEnabled),
    notif.hbInterval.value,
    notif.hbDelay.value,
  ]);
}

function updateNotifDirty() {
  notifDirty = notifFormState() !== notifSavedForm;
  notif.save.disabled = !notifDirty;
}

function renderNotifSettings(view) {
  notifSaved = view;
  notif.source.textContent = SOURCE_LABELS[view.source] ?? "";
  setToggle(notif.enabled, view.enabled);
  notif.station.value = view.station_name || "";
  notif.server.value = view.server || "";
  // Saved secrets are shown masked (opti****bd45) as the field's value:
  // left as is = keep, cleared = remove, anything else = replace.
  notif.topic.value = view.topic_set ? view.topic_hint : "";
  notif.topic.placeholder = "optic-… (required to turn notifications on)";
  notif.token.value = view.token_set ? view.token_hint : "";
  notif.token.placeholder = "optional";
  setToggle(notif.digestEnabled, view.digest.enabled);
  notif.digestTime.value = view.digest.send_at;
  setToggle(notif.hbEnabled, view.heartbeat.enabled);
  notif.hbInterval.value = Math.round(view.heartbeat.interval_secs / 60);
  notif.hbDelay.value = Math.round(view.heartbeat.alert_after_secs / 60);
  notifSavedForm = notifFormState();
  updateNotifDirty();
  if (view.error) showNotifNotice(view.error, "warning");
}

function formatNotifTime(iso, timezone) {
  if (!iso) return "—";
  return new Intl.DateTimeFormat(undefined, {
    timeZone: timezone || undefined,
    weekday: "short",
    hour: "numeric",
    minute: "2-digit",
    timeZoneName: "short",
  }).format(new Date(iso));
}

const HEARTBEAT_STATES = {
  disabled: "off",
  dry_run: "notifications off; not checking in",
  ok: "checking in",
  withheld: "withheld",
  failing: "check-in failing",
};

function renderNotifStatus(alerts) {
  const digest = alerts.digest;
  const timezone = digest?.timezone;
  if (digest) {
    notif.digestStatus.textContent = digest.enabled
      ? `Next digest ${formatNotifTime(digest.next_due_at, timezone)}` +
        (digest.last_sent_at
          ? ` · last sent ${formatNotifTime(digest.last_sent_at, timezone)}`
          : "") +
        (digest.pending ? " · waiting to be delivered" : "") +
        (digest.last_error ? ` · last error: ${digest.last_error}` : "")
      : "Digest is off.";
  }
  const heartbeat = alerts.heartbeat;
  if (heartbeat) {
    const parts = [`Status: ${HEARTBEAT_STATES[heartbeat.state] || heartbeat.state}`];
    if (heartbeat.withheld_reason) parts.push(heartbeat.withheld_reason);
    if (heartbeat.last_checkin_at) {
      parts.push(`last check-in ${formatNotifTime(heartbeat.last_checkin_at, timezone)}`);
    }
    if (heartbeat.next_checkin_at) {
      parts.push(`next ${formatNotifTime(heartbeat.next_checkin_at, timezone)}`);
    }
    if (heartbeat.last_error) parts.push(`last error: ${heartbeat.last_error}`);
    notif.hbStatus.textContent = parts.join(" · ");
  }
}

async function refreshNotifStatus() {
  try {
    renderNotifStatus(await notifApi("/api/alerts"));
  } catch (_) {
    // The status lines are informational; the footer shows alert errors.
  }
}

async function loadNotifSettings() {
  try {
    renderNotifSettings(await notifApi("/api/notifications"));
  } catch (error) {
    showNotifNotice(`Notification settings unavailable: ${error.message}`, "error");
  }
}

// 24 random characters from an unambiguous alphabet: ~120 bits, far beyond
// guessing, and within ntfy's topic rules (A-Z a-z 0-9 - _).
function generateTopic() {
  const alphabet = "abcdefghijkmnpqrstuvwxyz23456789";
  const bytes = new Uint8Array(24);
  crypto.getRandomValues(bytes);
  return `optic-${Array.from(bytes, (byte) => alphabet[byte % alphabet.length]).join("")}`;
}

// What to send for a masked secret field: unchanged = keep (empty value),
// cleared = remove, otherwise the new value. A partly edited mask is an
// error: the rest of the secret is unknown to the page.
function secretField(input, saved, hint, name) {
  const value = input.value.trim();
  if (saved && value === hint) return { value: "", clear: false };
  if (saved && value === "") return { value: "", clear: true };
  if (value.includes("*")) {
    throw new Error(`type the full new ${name}, or clear the field to remove it`);
  }
  return { value, clear: false };
}

function notifPayload() {
  const minutes = (input) => Math.round(Number(input.value) * 60);
  const topic = secretField(notif.topic, notifSaved?.topic_set, notifSaved?.topic_hint, "topic");
  const token = secretField(notif.token, notifSaved?.token_set, notifSaved?.token_hint, "token");
  return {
    enabled: isToggleOn(notif.enabled),
    station_name: notif.station.value,
    server: notif.server.value,
    topic: topic.value,
    clear_topic: topic.clear,
    token: token.value,
    clear_token: token.clear,
    digest: {
      enabled: isToggleOn(notif.digestEnabled),
      send_at: notif.digestTime.value,
    },
    heartbeat: {
      enabled: isToggleOn(notif.hbEnabled),
      interval_secs: minutes(notif.hbInterval),
      alert_after_secs: minutes(notif.hbDelay),
      sequence_id: notifSaved?.heartbeat.sequence_id || "optic-heartbeat",
    },
  };
}

async function saveNotifSettings() {
  notif.save.disabled = true;
  showNotifNotice("Saving…");
  try {
    const view = await notifApi("/api/notifications", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(notifPayload()),
    });
    renderNotifSettings(view);
    showNotifNotice("Saved. Changes apply immediately.", "success");
    setTimeout(refreshNotifStatus, 1500);
  } catch (error) {
    notif.save.disabled = false;
    showNotifNotice(`Not saved: ${error.message}`, "error");
  }
}

async function notifManualSend(path, button, sentMessage) {
  if (notifDirty) {
    showNotifNotice("Save your changes first; this uses the saved settings.", "warning");
    return;
  }
  button.disabled = true;
  showNotifNotice("Sending…");
  try {
    await notifApi(path, { method: "POST" });
    showNotifNotice(sentMessage, "success");
  } catch (error) {
    showNotifNotice(`Not sent: ${error.message}`, "error");
  } finally {
    button.disabled = false;
  }
}

for (const input of [
  notif.station,
  notif.server,
  notif.topic,
  notif.token,
  notif.digestTime,
  notif.hbInterval,
  notif.hbDelay,
]) {
  input.addEventListener("input", updateNotifDirty);
  input.addEventListener("change", updateNotifDirty);
}

for (const toggle of [notif.enabled, notif.digestEnabled, notif.hbEnabled]) {
  toggle.addEventListener("click", () => {
    setToggle(toggle, !isToggleOn(toggle));
    updateNotifDirty();
  });
}

notif.generateTopic.addEventListener("click", () => {
  notif.topic.value = generateTopic();
  updateNotifDirty();
  showNotifNotice(
    "New topic generated. Subscribe to it in the ntfy app now; it is not shown again after saving.",
    "warning",
  );
});
notif.save.addEventListener("click", saveNotifSettings);
notif.test.addEventListener("click", () =>
  notifManualSend(
    "/api/notifications/test",
    notif.test,
    "Test notification sent. Check your phone.",
  ),
);
notif.digestNow.addEventListener("click", () =>
  notifManualSend(
    "/api/notifications/digest-now",
    notif.digestNow,
    "Digest of the last 24 h sent. Check your phone.",
  ),
);

void loadNotifSettings();
void refreshNotifStatus();
setInterval(refreshNotifStatus, 15000);
