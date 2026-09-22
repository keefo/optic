const elements = {
  preview: document.querySelector("#preview"),
  previewFrame: document.querySelector(".preview-frame"),
  placeholder: document.querySelector("#preview-placeholder"),
  notice: document.querySelector("#notice"),
  streamState: document.querySelector("#stream-state"),
  daemonStatus: document.querySelector("#daemon-status"),
  cameraStatus: document.querySelector("#camera-status"),
  capture: document.querySelector("#capture"),
  reset: document.querySelector("#reset"),
  saveDng: document.querySelector("#save-dng"),
  dngHelp: document.querySelector("#dng-help"),
  firstVisibleLatency: document.querySelector("#first-visible-latency"),
  medianVisibleLatency: document.querySelector("#median-visible-latency"),
  medianSampleCount: document.querySelector("#median-sample-count"),
  captureLatency: document.querySelector("#capture-latency"),
  captureLatencyDetail: document.querySelector("#capture-latency-detail"),
  measurementSample: document.querySelector("#measurement-sample"),
  discardConfig: document.querySelector("#discard-config"),
  saveConfig: document.querySelector("#save-config"),
  syncConnectivity: document.querySelector("#sync-connectivity"),
  syncQueued: document.querySelector("#sync-queued"),
  syncTransferred: document.querySelector("#sync-transferred"),
  syncConnectivityRaw: document.querySelector("#sync-connectivity-raw"),
  syncBackoff: document.querySelector("#sync-backoff"),
  syncNextRetry: document.querySelector("#sync-next-retry"),
  syncNextScan: document.querySelector("#sync-next-scan"),
  syncLastError: document.querySelector("#sync-last-error"),
  syncToggle: document.querySelector("#sync-toggle-btn"),
  syncRetryNow: document.querySelector("#sync-retry-now"),
  schedulerRunState: document.querySelector("#scheduler-run-state"),
  schedulerNextCapture: document.querySelector("#scheduler-next-capture"),
  schedulerRuleCount: document.querySelector("#scheduler-rule-count"),
};

const defaults = {
  rotation: 0,
  horizontal_flip: false,
  vertical_flip: false,
  awb: "auto",
  metering: "centre",
  exposure: "normal",
  ev: 0,
  gain: 0,
  shutter_us: 0,
  denoise: "auto",
};

const profiles = {
  master_archive: {
    label: "Master Archive",
    width: 4056,
    height: 3040,
    previewWidth: 4056,
    previewHeight: 3040,
    previewFps: 2,
  },
  dci_4k: {
    label: "4K DCI Widescreen",
    width: 4056,
    height: 2160,
    previewWidth: 1352,
    previewHeight: 720,
    previewFps: 8,
  },
  binning_2k: {
    label: "2K Binning",
    width: 2028,
    height: 1520,
    previewWidth: 1014,
    previewHeight: 760,
    previewFps: 8,
  },
};

let objectUrl = null;
let livePreview = false;
// Fill in default placeholders if we deleted elements to support pure manual focus
if (!document.querySelector("#ev")) {
  const hiddenForm = document.createElement("div");
  hiddenForm.style.display = "none";
  hiddenForm.innerHTML = `
      <input id="ev" type="hidden" value="0.0">
      <input id="ev-value" type="hidden" value="0.0">
      <select id="metering">
        <option value="centre" selected>Centre</option>
      </select>
      <select id="exposure">
        <option value="normal" selected>Normal</option>
      </select>
    `;
  document.body.appendChild(hiddenForm);
}

let previewGeneration = 0;
let reconfigureTimer = null;
let reconfigureRunning = false;
let reconfigurePending = false;
let previewStarting = false;
let captureRunning = false;
let pageActive = true;
let controlRevision = 0;
let mjpegAbortController = null;
let mjpegGeneration = 0;
let measurementRevision = 0;
let lastFrameMetadata = null;
const controlChanges = new Map();
let cameraFieldsInitialized = false;
const activeMeasurements = new Map();
const firstVisibleSamples = [];
// Capture latency samples per profile + DNG choice: their costs differ too
// much (≈0.7 s vs ≈2.5 s) for one shared median to mean anything.
const captureSamples = new Map();
const MAX_MEASUREMENT_SAMPLES = 10;
const SETTLE_TIMEOUT_MS = 15000;
const POST_CAPTURE_FREEZE_MS = 3000;

function settings() {
  return {
    rotation: Number(document.querySelector("#rotation").value),
    horizontal_flip: document.querySelector("#hflip").checked,
    vertical_flip: document.querySelector("#vflip").checked,
    awb: document.querySelector("#awb").value,
    metering: document.querySelector("#metering").value,
    exposure: document.querySelector("#exposure").value,
    ev: Number(document.querySelector("#ev").value),
    gain: Number(document.querySelector("#gain").value),
    shutter_us: Number(document.querySelector("#shutter").value),
    denoise: document.querySelector("#denoise").value,
  };
}

function selectedProfile() {
  return document.querySelector('input[name="capture-profile"]:checked').value;
}

function streamRequest(profileName = selectedProfile()) {
  return { settings: settings(), profile: profileName, control_revision: controlRevision };
}

function optionLabel(controlId, value) {
  const control = document.querySelector(`#${controlId}`);
  const option = Array.from(control.options).find((candidate) => candidate.value === value);
  return option ? option.text : value;
}

function previewLabel(profileName, values) {
  const profile = profiles[profileName];
  const ev = `${values.ev >= 0 ? "+" : ""}${values.ev.toFixed(1)}`;
  const gain = values.gain === 0 ? "Auto" : `${values.gain.toFixed(1)}×`;
  return `Live · ${profile.previewWidth} × ${profile.previewHeight} · ${profile.previewFps} FPS · White balance ${optionLabel("awb", values.awb)} · Metering ${optionLabel("metering", values.metering)} · Exposure mode ${optionLabel("exposure", values.exposure)} · Denoise ${optionLabel("denoise", values.denoise)} · EV ${ev} · Analogue gain ${gain}`;
}

function setPreviewAspect(profileName) {
  const profile = profiles[profileName];
  elements.previewFrame.style.aspectRatio = `${profile.previewWidth} / ${profile.previewHeight}`;
}

// Only forces `checked` for Binning2k (the one profile with a hard policy
// — `validate_raw_policy` rejects a companion DNG for it outright).
// MasterArchive/Dci4k leave `checked` exactly as it was: `save_dng` is a
// persistent setting like rotation/gain, not reset on every profile
// switch — it's staged via `stageSaveDng` (see below) and read back from
// real server config on load (see `applyServerConfig`), same as every
// other camera setting.
function updateProfile() {
  const profile = selectedProfile();
  setPreviewAspect(profile);
  if (profile === "master_archive") {
    elements.saveDng.disabled = false;
    elements.dngHelp.textContent = "Recommended for Master Archive";
  } else if (profile === "dci_4k") {
    elements.saveDng.disabled = false;
    elements.dngHelp.textContent = "Optional for 4K DCI";
  } else {
    elements.saveDng.checked = false;
    elements.saveDng.disabled = true;
    elements.dngHelp.textContent = "Disabled for efficient 2K capture";
  }
  schedulePreviewUpdate();
}

function applySettings(values) {
  document.querySelector("#rotation").value = values.rotation;
  document.querySelector("#hflip").checked = values.horizontal_flip;
  document.querySelector("#vflip").checked = values.vertical_flip;
  document.querySelector("#awb").value = values.awb;
  document.querySelector("#metering").value = values.metering;
  document.querySelector("#exposure").value = values.exposure;
  document.querySelector("#ev").value = values.ev;
  document.querySelector("#gain").value = values.gain;
  document.querySelector("#shutter").value = values.shutter_us;
  document.querySelector("#denoise").value = values.denoise;
  updateOutputs();
}

function updateOutputs() {
  const values = settings();
  document.querySelector("#ev-value").value = values.ev.toFixed(1);
  document.querySelector("#gain-value").value = values.gain === 0 ? "Auto" : values.gain.toFixed(1);
  document.querySelector("#shutter-value").value =
    values.shutter_us === 0 ? "Auto" : values.shutter_us;
}

function showNotice(message, kind = "normal") {
  elements.notice.textContent = message;
  elements.notice.dataset.kind = kind;
}

// Applies the server's current committed-or-staged profile/settings to the
// form fields. Only called once per page load (see `cameraFieldsInitialized`
// in `refreshStatus`) or right after a discard — never on every status
// poll, which would otherwise fight an in-progress edit the user is
// actively dragging a slider on.
function applyServerConfig(config) {
  applySettings(config.settings);
  for (const btn of document.querySelectorAll('input[name="capture-profile"]')) {
    btn.checked = btn.value === config.profile;
  }
  // Set before updateProfile() so Binning2k's forced-off policy (inside
  // updateProfile()) still wins if that's the committed/staged profile.
  elements.saveDng.checked = config.save_dng;
  updateProfile();
}

// Stages the DNG checkbox as a real, persistent, saveable setting — see
// `POST /api/config/save-dng` in web.rs for why this is its own endpoint
// rather than riding along on the live-preview reconfigure request.
async function stageSaveDng() {
  try {
    await api("/api/config/save-dng", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ save_dng: elements.saveDng.checked }),
    });
  } catch (error) {
    showNotice(`Failed to stage DNG preference: ${error.message}`, "error");
  } finally {
    refreshStatus();
  }
}

async function fnCommitConfig() {
  setBusy(true);
  try {
    const response = await api("/api/config/commit", { method: "POST" });
    const result = await response.json();
    showNotice(result.message, "success");
  } catch (error) {
    showNotice(error.message, "error");
  } finally {
    setBusy(false);
    refreshStatus();
  }
}

async function fnDiscardConfig() {
  setBusy(true);
  try {
    const response = await api("/api/config/discard", { method: "POST" });
    const result = await response.json();
    showNotice(result.message, "success");
  } catch (error) {
    showNotice(error.message, "error");
  } finally {
    setBusy(false);
    // Force the next refreshStatus() to re-populate camera fields from the
    // just-reverted server state.
    cameraFieldsInitialized = false;
    refreshStatus();
  }
}

function hidePreview(state = "Starting automatically") {
  stopMjpegPreview();
  if (objectUrl) {
    URL.revokeObjectURL(objectUrl);
    objectUrl = null;
  }
  elements.preview.removeAttribute("src");
  elements.preview.hidden = true;
  elements.placeholder.hidden = false;
  elements.streamState.textContent = state;
}

function stopMjpegPreview() {
  mjpegGeneration += 1;
  if (mjpegAbortController) {
    mjpegAbortController.abort();
    mjpegAbortController = null;
  }
}

// Stops the live stream but leaves the last rendered frame on screen
// (rather than blanking to the placeholder) so a still capture reads as
// "the view froze for a moment" instead of "the preview went black".
function freezePreviewForCapture(state) {
  stopMjpegPreview();
  elements.streamState.textContent = state;
}

function startMjpegPreview(state) {
  stopMjpegPreview();
  const generation = mjpegGeneration;
  const controller = new AbortController();
  mjpegAbortController = controller;
  elements.preview.hidden = false;
  elements.placeholder.hidden = true;
  elements.streamState.textContent = state;
  void consumeMjpeg(generation, controller.signal);
}

async function consumeMjpeg(generation, signal) {
  try {
    const response = await api(`/api/stream/mjpeg?t=${Date.now()}`, { signal });
    if (!response.body) throw new Error("Streaming response body is unavailable");
    const reader = response.body.getReader();
    let buffer = new Uint8Array(0);

    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      const combined = new Uint8Array(buffer.length + value.length);
      combined.set(buffer);
      combined.set(value, buffer.length);
      buffer = combined;

      while (true) {
        const headerEnd = findHeaderEnd(buffer);
        if (headerEnd < 0) break;
        const headerText = new TextDecoder().decode(buffer.subarray(0, headerEnd));
        const headers = parsePartHeaders(headerText);
        const length = Number(headers["content-length"]);
        if (!Number.isSafeInteger(length) || length <= 0) {
          throw new Error("MJPEG frame has an invalid content length");
        }
        const imageStart = headerEnd + 4;
        const partEnd = imageStart + length + 2;
        if (buffer.length < partEnd) break;
        const jpeg = buffer.slice(imageStart, imageStart + length);
        buffer = buffer.slice(partEnd);
        await renderMjpegFrame(jpeg, headers, generation);
      }
    }
  } catch (error) {
    if (error.name !== "AbortError" && generation === mjpegGeneration && livePreview) {
      elements.streamState.textContent = "Live · stream interrupted";
      showNotice(`Preview stream interrupted: ${error.message}`, "error");
    }
  }
}

function findHeaderEnd(buffer) {
  for (let index = 0; index <= buffer.length - 4; index += 1) {
    if (
      buffer[index] === 13 &&
      buffer[index + 1] === 10 &&
      buffer[index + 2] === 13 &&
      buffer[index + 3] === 10
    ) {
      return index;
    }
  }
  return -1;
}

function parsePartHeaders(text) {
  const headers = {};
  for (const line of text.split("\r\n").slice(1)) {
    const separator = line.indexOf(":");
    if (separator > 0) {
      headers[line.slice(0, separator).trim().toLowerCase()] = line.slice(separator + 1).trim();
    }
  }
  return headers;
}

async function renderMjpegFrame(jpeg, headers, generation) {
  if (generation !== mjpegGeneration) return;
  const nextUrl = URL.createObjectURL(new Blob([jpeg], { type: "image/jpeg" }));
  const previousUrl = objectUrl;
  objectUrl = nextUrl;
  elements.preview.src = nextUrl;
  try {
    await elements.preview.decode();
  } catch (_) {
    if (objectUrl === nextUrl) objectUrl = null;
    URL.revokeObjectURL(nextUrl);
    return;
  }
  if (generation !== mjpegGeneration || objectUrl !== nextUrl) {
    URL.revokeObjectURL(nextUrl);
    return;
  }
  const paintedAt = await new Promise((resolve) => {
    requestAnimationFrame(() => requestAnimationFrame(resolve));
  });
  if (previousUrl) URL.revokeObjectURL(previousUrl);
  recordRenderedFrame(headers, paintedAt);
}

function beginMeasurement(label, needsAe, needsAwb) {
  const revision = controlRevision;
  controlChanges.set(revision, {
    revision,
    label,
    needsAe,
    needsAwb,
    startedAt: performance.now(),
    firstVisibleAt: null,
    baseline: cloneMetadata(lastFrameMetadata),
    previous: null,
    observedChange: lastFrameMetadata === null,
    stableFrames: 0,
  });
  measurementRevision = revision;
  elements.firstVisibleLatency.textContent = "Measuring…";
  elements.measurementSample.textContent = `${label} · revision ${revision}`;
}

function beginControlMeasurement(control) {
  const labels = {
    awb: "White balance",
    metering: "Metering",
    exposure: "Exposure mode",
    ev: "EV compensation",
    gain: "Analogue gain",
    shutter: "Shutter",
    denoise: "Denoise",
    rotation: "Rotation",
    hflip: "Horizontal flip",
    vflip: "Vertical flip",
  };
  const aeControls = new Set(["metering", "exposure", "ev", "gain", "shutter"]);
  beginMeasurement(
    labels[control.id] || "Camera control",
    aeControls.has(control.id),
    control.id === "awb",
  );
}

function markMeasurementSent(revision) {
  const measurement = controlChanges.get(revision);
  if (measurement && !activeMeasurements.has(revision)) {
    activeMeasurements.set(revision, measurement);
  }
  for (const pendingRevision of controlChanges.keys()) {
    if (pendingRevision <= revision) controlChanges.delete(pendingRevision);
  }
}

function recordRenderedFrame(headers, paintedAt) {
  const frameRevision = Number(headers["x-optic-control-revision"]);
  if (!Number.isSafeInteger(frameRevision)) return;
  const aeState = headers["x-optic-ae-state"] || "unavailable";
  const awbState = headers["x-optic-awb-state"] || "unavailable";
  const metadata = frameMetadata(headers);
  lastFrameMetadata = metadata;

  for (const [revision, measurement] of activeMeasurements) {
    if (frameRevision < revision) continue;
    if (measurement.firstVisibleAt === null) {
      measurement.firstVisibleAt = paintedAt;
      const latency = paintedAt - measurement.startedAt;
      firstVisibleSamples.push(latency);
      if (firstVisibleSamples.length > MAX_MEASUREMENT_SAMPLES) firstVisibleSamples.shift();
      if (revision === measurementRevision) {
        elements.firstVisibleLatency.textContent = formatLatency(latency);
        elements.medianVisibleLatency.textContent = formatLatency(median(firstVisibleSamples));
        elements.medianSampleCount.textContent = `Last ${firstVisibleSamples.length} measurement${firstVisibleSamples.length === 1 ? "" : "s"}`;
        elements.measurementSample.textContent = `${measurement.label} · revision ${revision} · frame ${headers["x-optic-sequence"] || "—"}`;
      }
    }

    if (!measurement.needsAe && !measurement.needsAwb) {
      activeMeasurements.delete(revision);
      continue;
    }

    const metadataAvailable = relevantMetadataAvailable(metadata, measurement);
    if (metadataAvailable && !measurement.observedChange) {
      measurement.observedChange = relevantMetadataChanged(
        measurement.baseline,
        metadata,
        measurement,
      );
    }
    const aeSettled =
      !measurement.needsAe ||
      aeState === "converged" ||
      aeState === "idle" ||
      aeState === "unavailable";
    const awbSettled =
      !measurement.needsAwb ||
      awbState === "converged" ||
      awbState === "locked" ||
      awbState === "unavailable";
    if (
      measurement.observedChange &&
      aeSettled &&
      awbSettled &&
      relevantMetadataStable(measurement.previous, metadata, measurement)
    ) {
      measurement.stableFrames += 1;
    } else {
      measurement.stableFrames = 0;
    }
    measurement.previous = cloneMetadata(metadata);

    const settled =
      measurement.stableFrames >= 3 ||
      !metadataAvailable ||
      paintedAt - measurement.startedAt >= SETTLE_TIMEOUT_MS;
    if (settled) activeMeasurements.delete(revision);
  }
}

function frameMetadata(headers) {
  const exposureUs = Number(headers["x-optic-exposure-us"]);
  const analogueGain = Number(headers["x-optic-analogue-gain"]);
  const gains = (headers["x-optic-colour-gains"] || "").split(",").map(Number);
  return {
    exposureUs: Number.isFinite(exposureUs) ? exposureUs : null,
    analogueGain: Number.isFinite(analogueGain) ? analogueGain : null,
    colourGains: gains.length === 2 && gains.every(Number.isFinite) ? gains : null,
  };
}

function cloneMetadata(metadata) {
  if (!metadata) return null;
  return {
    exposureUs: metadata.exposureUs,
    analogueGain: metadata.analogueGain,
    colourGains: metadata.colourGains ? [...metadata.colourGains] : null,
  };
}

function relevantMetadataAvailable(metadata, measurement) {
  if (!metadata) return false;
  const aeAvailable =
    !measurement.needsAe || (metadata.exposureUs !== null && metadata.analogueGain !== null);
  const awbAvailable = !measurement.needsAwb || metadata.colourGains !== null;
  return aeAvailable && awbAvailable;
}

function relevantMetadataChanged(baseline, current, measurement) {
  if (!baseline) return true;
  const aeChanged =
    measurement.needsAe &&
    (!approximatelyEqual(baseline.exposureUs, current.exposureUs, 50, 0.005) ||
      !approximatelyEqual(baseline.analogueGain, current.analogueGain, 0.005, 0.005));
  const awbChanged =
    measurement.needsAwb &&
    !arraysApproximatelyEqual(baseline.colourGains, current.colourGains, 0.005, 0.005);
  return aeChanged || awbChanged || (!measurement.needsAe && !measurement.needsAwb);
}

function relevantMetadataStable(previous, current, measurement) {
  if (!previous) return false;
  const aeStable =
    !measurement.needsAe ||
    (approximatelyEqual(previous.exposureUs, current.exposureUs, 50, 0.005) &&
      approximatelyEqual(previous.analogueGain, current.analogueGain, 0.005, 0.005));
  const awbStable =
    !measurement.needsAwb ||
    arraysApproximatelyEqual(previous.colourGains, current.colourGains, 0.005, 0.005);
  return aeStable && awbStable;
}

function approximatelyEqual(left, right, absoluteTolerance, relativeTolerance) {
  if (left === null || right === null) return false;
  return Math.abs(left - right) <= Math.max(absoluteTolerance, Math.abs(left) * relativeTolerance);
}

function arraysApproximatelyEqual(left, right, absoluteTolerance, relativeTolerance) {
  if (!left || !right || left.length !== right.length) return false;
  return left.every((value, index) =>
    approximatelyEqual(value, right[index], absoluteTolerance, relativeTolerance),
  );
}

function median(values) {
  const sorted = [...values].sort((left, right) => left - right);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0 ? (sorted[middle - 1] + sorted[middle]) / 2 : sorted[middle];
}

function recordCaptureLatency(key, label, latency) {
  const samples = captureSamples.get(key) || [];
  samples.push(latency);
  if (samples.length > MAX_MEASUREMENT_SAMPLES) samples.shift();
  captureSamples.set(key, samples);
  elements.captureLatency.textContent = formatLatency(latency);
  elements.captureLatencyDetail.textContent = `${label} · median ${formatLatency(median(samples))} of ${samples.length}`;
}

// The button stays disabled after the capture itself: the frozen-still hold
// plus the preview restart. Shown so that wait isn't mistaken for capture time.
function traceButtonReady(captureDoneAt, holdDoneAt, readyAt) {
  const hold = holdDoneAt - captureDoneAt;
  const preview = readyAt - holdDoneAt;
  const trace = `button ready +${formatLatency(readyAt - captureDoneAt)} (hold ${formatLatency(hold)}, preview ${formatLatency(preview)})`;
  elements.captureLatencyDetail.textContent += ` · ${trace}`;
  console.info(`capture perf: ${trace}`);
}

function formatLatency(milliseconds) {
  return `${Math.round(milliseconds)} ms`;
}

function cancelPreviewUpdate() {
  if (reconfigureTimer !== null) {
    clearTimeout(reconfigureTimer);
    reconfigureTimer = null;
  }
  reconfigurePending = false;
}

function schedulePreviewUpdate() {
  reconfigurePending = true;
  if (!livePreview) return;
  if (reconfigureTimer !== null) clearTimeout(reconfigureTimer);
  reconfigureTimer = setTimeout(updateLivePreview, 350);
}

async function updateLivePreview() {
  reconfigureTimer = null;
  if (!livePreview || reconfigureRunning || !reconfigurePending) return;

  reconfigurePending = false;
  reconfigureRunning = true;
  const generation = previewGeneration;
  const profileName = selectedProfile();
  const profile = profiles[profileName];
  const requestedRevision = controlRevision;
  const request = streamRequest(profileName);
  markMeasurementSent(requestedRevision);
  elements.streamState.textContent = "Live · updating configuration";
  showNotice(`Applying ${profile.label} preview…`);

  try {
    const response = await api("/api/stream/reconfigure", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(request),
    });
    const accepted = await response.json();
    if (livePreview && generation === previewGeneration) {
      setPreviewAspect(accepted.profile);
      startMjpegPreview(previewLabel(accepted.profile, accepted.settings));
      showNotice(`${profile.label} preview updated.`, "success");
    }
    if (controlRevision !== requestedRevision) reconfigurePending = true;
  } catch (error) {
    activeMeasurements.delete(requestedRevision);
    if (livePreview && generation === previewGeneration) {
      elements.streamState.textContent = "Live · update failed";
      if (requestedRevision === measurementRevision) {
        elements.firstVisibleLatency.textContent = "Failed";
      }
      showNotice(`Could not update preview: ${error.message}`, "error");
    }
  } finally {
    reconfigureRunning = false;
    if (livePreview && reconfigurePending) {
      reconfigureTimer = setTimeout(updateLivePreview, 0);
    }
    refreshStatus();
  }
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

async function ensurePreview(streamAlreadyRunning = null, announce = true) {
  if (!pageActive || livePreview || previewStarting || captureRunning) return;

  const generation = previewGeneration;
  const profileName = selectedProfile();
  const profile = profiles[profileName];
  previewStarting = true;
  setBusy(true);
  cancelPreviewUpdate();
  const requestedRevision = controlRevision;
  const request = streamRequest(profileName);
  elements.streamState.textContent = "Starting automatically";
  if (announce) showNotice(`Starting ${profile.label} preview…`);
  try {
    if (streamAlreadyRunning === null) {
      const response = await api("/api/status");
      const status = await response.json();
      streamAlreadyRunning = status.camera.streaming;
    }
    if (!pageActive || generation !== previewGeneration) return;
    const response = await api(
      streamAlreadyRunning ? "/api/stream/reconfigure" : "/api/stream/start",
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(request),
      },
    );
    const accepted = await response.json();
    markMeasurementSent(requestedRevision);
    if (!pageActive || generation !== previewGeneration) {
      navigator.sendBeacon("/api/stream/stop");
      return;
    }
    livePreview = true;
    previewGeneration += 1;
    setPreviewAspect(accepted.profile);
    startMjpegPreview(previewLabel(accepted.profile, accepted.settings));
    if (controlRevision !== requestedRevision || reconfigurePending) schedulePreviewUpdate();
    if (announce) {
      showNotice(
        `${profile.label} preview is live. Adjust focus and aperture on the lens.`,
        "success",
      );
    }
  } catch (error) {
    hidePreview("Automatic preview unavailable");
    showNotice(`Could not start preview: ${error.message}`, "error");
  } finally {
    previewStarting = false;
    setBusy(false);
  }
}

function prepareForStillCapture() {
  captureRunning = true;
  livePreview = false;
  previewGeneration += 1;
  cancelPreviewUpdate();
  freezePreviewForCapture("Still capture · preview resumes automatically");
}

async function captureAndTransfer() {
  const profileName = selectedProfile();
  const profile = profiles[profileName];
  const saveDng = elements.saveDng.checked;
  const captureKey = `${profileName}:${saveDng}`;
  const captureLabel = saveDng ? `${profile.label} + DNG` : profile.label;
  const captureStartedAt = performance.now();
  elements.captureLatency.textContent = "Measuring…";
  elements.captureLatencyDetail.textContent = captureLabel;
  setBusy(true);
  prepareForStillCapture();
  showNotice(`Capturing ${profile.label} to the verified RAM queue…`);
  try {
    const response = await api("/api/capture", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        settings: settings(),
        profile: profileName,
        save_dng: saveDng,
      }),
    });
    const result = await response.json();
    recordCaptureLatency(captureKey, captureLabel, performance.now() - captureStartedAt);
    showNotice(
      `${result.files.length} file${result.files.length === 1 ? "" : "s"} queued for ${profile.label} (${formatBytes(result.bytes)}).`,
      "success",
    );
  } catch (error) {
    elements.captureLatency.textContent = "Failed";
    elements.captureLatencyDetail.textContent = `${captureLabel} · not counted`;
    showNotice(error.message, "error");
  } finally {
    // Hold the frozen frame for a moment so the capture reads as a
    // deliberate still, rather than flickering straight back to live the
    // instant the request completes. captureRunning stays true through the
    // wait so refreshStatus()'s own auto-resume logic doesn't race this.
    const captureDoneAt = performance.now();
    await new Promise((resolve) => setTimeout(resolve, POST_CAPTURE_FREEZE_MS));
    const holdDoneAt = performance.now();
    captureRunning = false;
    await ensurePreview(null, false);
    setBusy(false);
    traceButtonReady(captureDoneAt, holdDoneAt, performance.now());
    refreshStatus();
  }
}

function setBusy(busy) {
  elements.capture.disabled = busy;
}

async function refreshStatus() {
  try {
    const response = await api("/api/status");
    const status = await response.json();
    elements.daemonStatus.textContent = `Daemon ${status.version}`;
    elements.daemonStatus.className = "pill good";
    elements.cameraStatus.textContent = status.camera.detected
      ? status.camera.streaming
        ? "Camera streaming"
        : status.camera.busy
          ? "Camera busy"
          : "Camera ready"
      : "Camera unavailable";
    elements.cameraStatus.className = status.camera.detected ? "pill good" : "pill bad";
    document.querySelector("#version").textContent = status.version;
    document.querySelector("#uptime").textContent = formatDuration(status.uptime_seconds);
    document.querySelector("#queue").textContent =
      `${status.capture_stage.queued_files} files · ${formatBytes(status.capture_stage.queued_bytes)}`;
    document.querySelector("#sensor").textContent = status.camera.sensor || "Not detected";
    renderSync(status.sync);
    renderSchedulerSummary(status.schedule, status.config.schedule.rules);
    if (!cameraFieldsInitialized) {
      applyServerConfig(status.config);
      cameraFieldsInitialized = true;
    }
    elements.saveConfig.disabled = !status.config_staged;
    elements.discardConfig.hidden = !status.config_staged;
    if (!pageActive || captureRunning || reconfigureRunning) return;
    if (!status.camera.streaming && livePreview) {
      livePreview = false;
      previewGeneration += 1;
      cancelPreviewUpdate();
      hidePreview("Restarting automatically");
      showNotice("Preview ended unexpectedly. Restarting…");
    }
    if (!livePreview && !previewStarting) {
      void ensurePreview(status.camera.streaming);
    }
  } catch {
    elements.daemonStatus.textContent = "Daemon offline";
    elements.daemonStatus.className = "pill bad";
  }
}

function renderSync(sync) {
  const label = !sync.enabled
    ? "Disabled"
    : sync.paused
      ? "Paused"
      : sync.connectivity === "syncing"
        ? "Syncing"
        : sync.connectivity === "backoff"
          ? "Retrying"
          : "Idle";
  const tone =
    !sync.enabled || sync.paused ? "neutral" : sync.connectivity === "backoff" ? "bad" : "good";
  elements.syncConnectivity.textContent = label;
  elements.syncConnectivity.className = `pill ${tone}`;

  elements.syncQueued.textContent = `${sync.queued_files} file${sync.queued_files === 1 ? "" : "s"} · ${formatBytes(sync.queued_bytes)}`;
  elements.syncTransferred.textContent = `${sync.transferred_files} file${sync.transferred_files === 1 ? "" : "s"} · ${formatBytes(sync.transferred_bytes)}`;
  elements.syncConnectivityRaw.textContent = sync.connectivity;
  elements.syncBackoff.textContent = sync.backoff_secs != null ? `${sync.backoff_secs}s` : "—";
  elements.syncNextRetry.textContent =
    sync.next_retry_in_secs != null ? `${sync.next_retry_in_secs}s` : "—";
  elements.syncNextScan.textContent =
    sync.next_scan_in_secs != null ? `${sync.next_scan_in_secs}s` : "—";
  elements.syncLastError.textContent = sync.last_error || "—";

  // One button, not two — label/target action flip with current state,
  // same treatment as the scheduler's run-control toggle.
  elements.syncToggle.textContent = sync.paused ? "Resume" : "Pause";
  elements.syncToggle.className = sync.paused ? "good" : "";
  elements.syncToggle.dataset.action = sync.paused ? "resume" : "pause";
  elements.syncToggle.disabled = !sync.enabled;
  elements.syncRetryNow.disabled = !sync.enabled || sync.connectivity !== "backoff";
}

function renderSchedulerSummary(schedule, rules) {
  const running = schedule.run_state === "Running";
  elements.schedulerRunState.textContent = running ? "Running" : "Paused";
  elements.schedulerRunState.className = `pill ${running ? "good" : "neutral"}`;
  elements.schedulerNextCapture.textContent = schedule.next_capture_at
    ? `${new Date(schedule.next_capture_at).toLocaleTimeString()} (${schedule.next_capture_rules.join(", ")})`
    : running
      ? "None scheduled"
      : "—";
  const count = rules?.length ?? 0;
  elements.schedulerRuleCount.textContent = `${count} rule${count === 1 ? "" : "s"}`;
}

async function syncAction(path) {
  try {
    await api(path, { method: "POST" });
  } catch (error) {
    showNotice(`Sync action failed: ${error.message}`, "error");
  } finally {
    refreshStatus();
  }
}

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KiB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MiB`;
  return `${(bytes / 1024 ** 3).toFixed(1)} GiB`;
}

function formatDuration(seconds) {
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
}

elements.capture.addEventListener("click", captureAndTransfer);
elements.reset.addEventListener("click", () => {
  applySettings(defaults);
  controlRevision += 1;
  beginMeasurement("Reset defaults", true, true);
  if (livePreview) {
    schedulePreviewUpdate();
  } else {
    showNotice("Camera controls reset to automatic defaults.");
  }
});
document.querySelectorAll(".control-grid input, .control-grid select").forEach((control) => {
  control.addEventListener("input", () => {
    controlRevision += 1;
    beginControlMeasurement(control);
    updateOutputs();
    schedulePreviewUpdate();
  });
});
document.querySelectorAll('input[name="capture-profile"]').forEach((control) => {
  control.addEventListener("change", () => {
    controlRevision += 1;
    beginMeasurement("Capture profile", false, false);
    updateProfile();
    // Binning2k's forced-off policy inside updateProfile() may just have
    // changed `checked` out from under a previously-staged value — keep
    // the server in sync with whatever the checkbox now actually shows.
    void stageSaveDng();
  });
});
elements.saveDng.addEventListener("change", stageSaveDng);
elements.preview.addEventListener("error", () => {
  if (livePreview) {
    showNotice("Waiting for camera frames…");
  }
});
window.addEventListener("pagehide", () => {
  pageActive = false;
  livePreview = false;
  previewGeneration += 1;
  cancelPreviewUpdate();
  navigator.sendBeacon("/api/stream/stop");
});
window.addEventListener("pageshow", (event) => {
  if (!event.persisted) return;
  pageActive = true;
  hidePreview();
  void ensurePreview();
});
applySettings(defaults);
updateProfile();
hidePreview();
refreshStatus();
setInterval(refreshStatus, 3000);
elements.discardConfig.addEventListener("click", fnDiscardConfig);
elements.saveConfig.addEventListener("click", fnCommitConfig);
elements.syncToggle.addEventListener("click", () =>
  syncAction(`/api/sync/${elements.syncToggle.dataset.action}`),
);
elements.syncRetryNow.addEventListener("click", () => syncAction("/api/sync/retry-now"));
