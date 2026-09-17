"use strict";

const elements = {
  preview: document.querySelector("#preview"),
  previewFrame: document.querySelector(".preview-frame"),
  placeholder: document.querySelector("#preview-placeholder"),
  notice: document.querySelector("#notice"),
  streamState: document.querySelector("#stream-state"),
  daemonStatus: document.querySelector("#daemon-status"),
  cameraStatus: document.querySelector("#camera-status"),
  testShot: document.querySelector("#test-shot"),
  capture: document.querySelector("#capture"),
  reset: document.querySelector("#reset"),
  saveDng: document.querySelector("#save-dng"),
  dngHelp: document.querySelector("#dng-help"),
  firstVisibleLatency: document.querySelector("#first-visible-latency"),
  medianVisibleLatency: document.querySelector("#median-visible-latency"),
  medianSampleCount: document.querySelector("#median-sample-count"),
  settledLatency: document.querySelector("#settled-latency"),
  measurementSample: document.querySelector("#measurement-sample"),
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
  master_archive: { label: "Master Archive", width: 4056, height: 3040, previewWidth: 4056, previewHeight: 3040, previewFps: 2 },
  dci_4k: { label: "4K DCI Widescreen", width: 4056, height: 2160, previewWidth: 1352, previewHeight: 720, previewFps: 8 },
  binning_2k: { label: "2K Binning", width: 2028, height: 1520, previewWidth: 1014, previewHeight: 760, previewFps: 8 },
};

let objectUrl = null;
let livePreview = false;
let previewGeneration = 0;
let reconfigureTimer = null;
let reconfigureRunning = false;
let reconfigurePending = false;
let activePreviewProfile = null;
let previewStarting = false;
let captureRunning = false;
let pageActive = true;
let controlRevision = 0;
let mjpegAbortController = null;
let mjpegGeneration = 0;
let measurementRevision = 0;
let lastFrameMetadata = null;
const controlChanges = new Map();
const activeMeasurements = new Map();
const firstVisibleSamples = [];
const MAX_MEASUREMENT_SAMPLES = 10;
const SETTLE_TIMEOUT_MS = 15000;

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

function updateProfile() {
  const profile = selectedProfile();
  setPreviewAspect(profile);
  if (profile === "master_archive") {
    elements.saveDng.checked = true;
    elements.saveDng.disabled = true;
    elements.dngHelp.textContent = "Required for Master Archive";
  } else if (profile === "dci_4k") {
    elements.saveDng.checked = false;
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
  document.querySelector("#shutter-value").value = values.shutter_us === 0 ? "Auto" : values.shutter_us;
}

function showNotice(message, kind = "normal") {
  elements.notice.textContent = message;
  elements.notice.dataset.kind = kind;
}

function showPreview(source, state) {
  stopMjpegPreview();
  if (objectUrl && objectUrl !== source) {
    URL.revokeObjectURL(objectUrl);
    objectUrl = null;
  }
  elements.preview.src = source;
  elements.preview.hidden = false;
  elements.placeholder.hidden = true;
  elements.streamState.textContent = state;
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
    if (buffer[index] === 13 && buffer[index + 1] === 10 && buffer[index + 2] === 13 && buffer[index + 3] === 10) {
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
  elements.settledLatency.textContent = needsAe || needsAwb ? "Measuring…" : "N/A";
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
  beginMeasurement(labels[control.id] || "Camera control", aeControls.has(control.id), control.id === "awb");
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
      measurement.observedChange = relevantMetadataChanged(measurement.baseline, metadata, measurement);
    }
    const aeSettled = !measurement.needsAe || aeState === "converged" || aeState === "idle" || aeState === "unavailable";
    const awbSettled = !measurement.needsAwb || awbState === "converged" || awbState === "locked" || awbState === "unavailable";
    if (measurement.observedChange && aeSettled && awbSettled
      && relevantMetadataStable(measurement.previous, metadata, measurement)) {
      measurement.stableFrames += 1;
    } else {
      measurement.stableFrames = 0;
    }
    measurement.previous = cloneMetadata(metadata);

    if (measurement.stableFrames >= 3) {
      if (revision === measurementRevision) {
        elements.settledLatency.textContent = formatLatency(paintedAt - measurement.startedAt);
      }
      activeMeasurements.delete(revision);
    } else if (!metadataAvailable) {
      if (revision === measurementRevision) elements.settledLatency.textContent = "Unavailable";
      activeMeasurements.delete(revision);
    } else if (paintedAt - measurement.startedAt >= SETTLE_TIMEOUT_MS) {
      if (revision === measurementRevision) {
        elements.settledLatency.textContent = measurement.observedChange ? ">15000 ms" : "No metadata change";
      }
      activeMeasurements.delete(revision);
    }
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
  const aeAvailable = !measurement.needsAe || (metadata.exposureUs !== null && metadata.analogueGain !== null);
  const awbAvailable = !measurement.needsAwb || metadata.colourGains !== null;
  return aeAvailable && awbAvailable;
}

function relevantMetadataChanged(baseline, current, measurement) {
  if (!baseline) return true;
  const aeChanged = measurement.needsAe && (
    !approximatelyEqual(baseline.exposureUs, current.exposureUs, 50, 0.005)
    || !approximatelyEqual(baseline.analogueGain, current.analogueGain, 0.005, 0.005)
  );
  const awbChanged = measurement.needsAwb && !arraysApproximatelyEqual(
    baseline.colourGains,
    current.colourGains,
    0.005,
    0.005,
  );
  return aeChanged || awbChanged || (!measurement.needsAe && !measurement.needsAwb);
}

function relevantMetadataStable(previous, current, measurement) {
  if (!previous) return false;
  const aeStable = !measurement.needsAe || (
    approximatelyEqual(previous.exposureUs, current.exposureUs, 50, 0.005)
    && approximatelyEqual(previous.analogueGain, current.analogueGain, 0.005, 0.005)
  );
  const awbStable = !measurement.needsAwb || arraysApproximatelyEqual(
    previous.colourGains,
    current.colourGains,
    0.005,
    0.005,
  );
  return aeStable && awbStable;
}

function approximatelyEqual(left, right, absoluteTolerance, relativeTolerance) {
  if (left === null || right === null) return false;
  return Math.abs(left - right) <= Math.max(absoluteTolerance, Math.abs(left) * relativeTolerance);
}

function arraysApproximatelyEqual(left, right, absoluteTolerance, relativeTolerance) {
  if (!left || !right || left.length !== right.length) return false;
  return left.every((value, index) => approximatelyEqual(value, right[index], absoluteTolerance, relativeTolerance));
}

function median(values) {
  const sorted = [...values].sort((left, right) => left - right);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0 ? (sorted[middle - 1] + sorted[middle]) / 2 : sorted[middle];
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
      activePreviewProfile = accepted.profile;
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
        elements.settledLatency.textContent = "Failed";
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
    const response = await api(streamAlreadyRunning ? "/api/stream/reconfigure" : "/api/stream/start", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(request),
    });
    const accepted = await response.json();
    markMeasurementSent(requestedRevision);
    if (!pageActive || generation !== previewGeneration) {
      navigator.sendBeacon("/api/stream/stop");
      return;
    }
    livePreview = true;
    activePreviewProfile = accepted.profile;
    previewGeneration += 1;
    setPreviewAspect(accepted.profile);
    startMjpegPreview(previewLabel(accepted.profile, accepted.settings));
    if (controlRevision !== requestedRevision || reconfigurePending) schedulePreviewUpdate();
    if (announce) {
      showNotice(`${profile.label} preview is live. Adjust focus and aperture on the lens.`, "success");
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
  hidePreview("Still capture · preview resumes automatically");
}

async function nativeTestShot() {
  const profileName = selectedProfile();
  const profile = profiles[profileName];
  setBusy(true);
  prepareForStillCapture();
  showNotice(`Capturing a ${profile.width} × ${profile.height} ${profile.label} test image…`);
  try {
    const response = await api("/api/test-shot", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ settings: settings(), profile: profileName }),
    });
    objectUrl = URL.createObjectURL(await response.blob());
    showPreview(objectUrl, `Test shot · ${profile.width} × ${profile.height}`);
    showNotice(`${profile.label} test shot complete. Inspect it at full size for focus.`, "success");
  } catch (error) {
    showNotice(error.message, "error");
  } finally {
    captureRunning = false;
    await ensurePreview(null, false);
    setBusy(false);
    refreshStatus();
  }
}

async function captureAndTransfer() {
  const profileName = selectedProfile();
  const profile = profiles[profileName];
  const saveDng = elements.saveDng.checked;
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
    showNotice(`${result.files.length} file${result.files.length === 1 ? "" : "s"} queued for ${profile.label} (${formatBytes(result.bytes)}).`, "success");
  } catch (error) {
    showNotice(error.message, "error");
  } finally {
    captureRunning = false;
    await ensurePreview(null, false);
    setBusy(false);
    refreshStatus();
  }
}

function setBusy(busy) {
  for (const button of [elements.testShot, elements.capture]) {
    button.disabled = busy;
  }
}

async function refreshStatus() {
  try {
    const response = await api("/api/status");
    const status = await response.json();
    elements.daemonStatus.textContent = `Daemon ${status.version}`;
    elements.daemonStatus.className = "pill good";
    elements.cameraStatus.textContent = status.camera.detected
      ? status.camera.streaming ? "Camera streaming" : status.camera.busy ? "Camera busy" : "Camera ready"
      : "Camera unavailable";
    elements.cameraStatus.className = status.camera.detected ? "pill good" : "pill bad";
    document.querySelector("#version").textContent = status.version;
    document.querySelector("#uptime").textContent = formatDuration(status.uptime_seconds);
    document.querySelector("#queue").textContent = `${status.capture_stage.queued_files} files · ${formatBytes(status.capture_stage.queued_bytes)}`;
    document.querySelector("#sensor").textContent = status.camera.sensor || "Not detected";
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
  } catch (error) {
    elements.daemonStatus.textContent = "Daemon offline";
    elements.daemonStatus.className = "pill bad";
  }
}

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1024 ** 2).toFixed(1)} MiB`;
}

function formatDuration(seconds) {
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
}

elements.testShot.addEventListener("click", nativeTestShot);
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
  });
});
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