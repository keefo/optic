// Focus tools for the live preview: histogram, focus peaking, and a loupe
// with a sharpness score. Everything runs in the browser on frames app.js
// has already decoded into #preview (same-origin blob URLs, so canvases are
// not tainted). Design: docs/optic-daemon-focus-tools.md.
//
// The pure functions at the top take `{ data, width, height }` RGBA pixel
// data and are exported for Node tests (tests/web/focus-tools.test.js).

(() => {
  const LUMA_R = 0.2126;
  const LUMA_G = 0.7152;
  const LUMA_B = 0.0722;
  const SHADOW_CLIP_LUMA = 2;
  const HIGHLIGHT_CLIP_CHANNEL = 254;
  // The daemon's own definition of a clipped sample: BT.601 luma >= 250
  // (`CLIPPED_LUMA`, docs/optic-daemon-exposure-ramping.md §5.1/§5.7).
  const CLIPPED_LUMA = 250;

  // Histogram of R, G, B and Rec. 709 luma (256 bins each), plus the
  // fraction of pixels crushed to black (luma <= 2) or with any channel
  // blown out (>= 254).
  function computeHistogram(image) {
    const { data, width, height } = image;
    const r = new Uint32Array(256);
    const g = new Uint32Array(256);
    const b = new Uint32Array(256);
    const luma = new Uint32Array(256);
    let shadow = 0;
    let highlight = 0;
    const total = width * height;
    for (let index = 0; index < total * 4; index += 4) {
      const red = data[index];
      const green = data[index + 1];
      const blue = data[index + 2];
      r[red] += 1;
      g[green] += 1;
      b[blue] += 1;
      const y = Math.round(LUMA_R * red + LUMA_G * green + LUMA_B * blue);
      luma[y] += 1;
      if (y <= SHADOW_CLIP_LUMA) shadow += 1;
      if (
        red >= HIGHLIGHT_CLIP_CHANNEL ||
        green >= HIGHLIGHT_CLIP_CHANNEL ||
        blue >= HIGHLIGHT_CLIP_CHANNEL
      ) {
        highlight += 1;
      }
    }
    return {
      r,
      g,
      b,
      luma,
      total,
      shadowClip: total ? shadow / total : 0,
      highlightClip: total ? highlight / total : 0,
    };
  }

  function toGray(image) {
    const { data, width, height } = image;
    const gray = new Float32Array(width * height);
    for (let pixel = 0, index = 0; pixel < gray.length; pixel += 1, index += 4) {
      gray[pixel] = LUMA_R * data[index] + LUMA_G * data[index + 1] + LUMA_B * data[index + 2];
    }
    return gray;
  }

  // Sobel gradient magnitude; the one-pixel border is left at 0.
  function sobelMagnitude(gray, width, height) {
    const magnitude = new Float32Array(width * height);
    for (let y = 1; y < height - 1; y += 1) {
      for (let x = 1; x < width - 1; x += 1) {
        const i = y * width + x;
        const topLeft = gray[i - width - 1];
        const top = gray[i - width];
        const topRight = gray[i - width + 1];
        const left = gray[i - 1];
        const right = gray[i + 1];
        const bottomLeft = gray[i + width - 1];
        const bottom = gray[i + width];
        const bottomRight = gray[i + width + 1];
        const gx = topRight + 2 * right + bottomRight - topLeft - 2 * left - bottomLeft;
        const gy = bottomLeft + 2 * bottom + bottomRight - topLeft - 2 * top - topRight;
        magnitude[i] = Math.sqrt(gx * gx + gy * gy);
      }
    }
    return magnitude;
  }

  // Absolute Sobel magnitude above which a pixel is highlighted. An absolute
  // threshold (not a percentile) is deliberate: a defocused frame must light
  // up less, so the highlighted area grows as focus improves.
  // sensitivity 0 -> 220 (only the crispest edges), 1 -> 40.
  function peakingThreshold(sensitivity) {
    const clamped = Math.min(1, Math.max(0, Number(sensitivity) || 0));
    return 220 - 180 * clamped;
  }

  // Tenengrad focus measure: mean squared Sobel magnitude over the interior.
  // Only comparable between frames of the same crop and scene, which is
  // exactly how the loupe uses it (turn the ring until it peaks).
  function sharpnessScore(gray, width, height) {
    if (width < 3 || height < 3) return 0;
    const magnitude = sobelMagnitude(gray, width, height);
    let sum = 0;
    for (let y = 1; y < height - 1; y += 1) {
      for (let x = 1; x < width - 1; x += 1) {
        const value = magnitude[y * width + x];
        sum += value * value;
      }
    }
    return sum / ((width - 2) * (height - 2));
  }

  // A size x size crop centred on (centerX, centerY), clamped inside the
  // frame. Shrinks to the frame when the frame is smaller than the crop.
  function loupeRect(centerX, centerY, size, frameWidth, frameHeight) {
    const width = Math.min(size, frameWidth);
    const height = Math.min(size, frameHeight);
    const x = Math.round(Math.min(Math.max(centerX - width / 2, 0), frameWidth - width));
    const y = Math.round(Math.min(Math.max(centerY - height / 2, 0), frameHeight - height));
    return { x, y, width, height };
  }

  // Maps a click to image pixels for an element drawn with
  // `object-fit: contain` (letterboxed). Returns null outside the image,
  // or with `clamp` pins the point to the nearest image edge (for drags).
  function clientToImagePoint(clientX, clientY, rect, naturalWidth, naturalHeight, clamp = false) {
    if (!naturalWidth || !naturalHeight || !rect.width || !rect.height) return null;
    const scale = Math.min(rect.width / naturalWidth, rect.height / naturalHeight);
    const offsetX = (rect.width - naturalWidth * scale) / 2;
    const offsetY = (rect.height - naturalHeight * scale) / 2;
    const x = (clientX - rect.left - offsetX) / scale;
    const y = (clientY - rect.top - offsetY) / scale;
    if (clamp) {
      return {
        x: Math.min(Math.max(x, 0), naturalWidth),
        y: Math.min(Math.max(y, 0), naturalHeight),
      };
    }
    if (x < 0 || y < 0 || x > naturalWidth || y > naturalHeight) return null;
    return { x, y };
  }

  // Which pixels the highlight guard counts as clipped: BT.601 luma (the Y
  // the daemon meters) >= CLIPPED_LUMA. Returns a 0/1 mask and the fraction.
  function clippedMask(image) {
    const { data, width, height } = image;
    const total = width * height;
    const mask = new Uint8Array(total);
    let clipped = 0;
    for (let pixel = 0, index = 0; pixel < total; pixel += 1, index += 4) {
      const y = 0.299 * data[index] + 0.587 * data[index + 1] + 0.114 * data[index + 2];
      if (Math.round(y) >= CLIPPED_LUMA) {
        mask[pixel] = 1;
        clipped += 1;
      }
    }
    return { mask, fraction: total ? clipped / total : 0 };
  }

  const pure = {
    computeHistogram,
    clippedMask,
    toGray,
    sobelMagnitude,
    peakingThreshold,
    sharpnessScore,
    loupeRect,
    clientToImagePoint,
  };

  if (typeof module !== "undefined" && module.exports) {
    module.exports = pure;
  }
  if (typeof document === "undefined") return;

  // ---- Browser controller -------------------------------------------------

  const STORAGE_KEY = "optic.focusTools";
  const WORK_LONG_EDGE = 960;
  const LOUPE_SOURCE = 160;
  const LOUPE_ZOOM = 2;
  const SENSOR_WIDTH = 4056;

  // The preview is a <canvas> (app.js draws decoded frames into it); these
  // also accept an <img>, whose intrinsic size is naturalWidth/Height.
  const frameWidth = (el) => el.naturalWidth || el.width || 0;
  const frameHeight = (el) => el.naturalHeight || el.height || 0;

  const $ = (selector) => document.querySelector(selector);
  const ui = {
    histogramToggle: $("#focus-histogram-toggle"),
    peakingToggle: $("#focus-peaking-toggle"),
    clippingToggle: $("#focus-clipping-toggle"),
    loupeToggle: $("#focus-loupe-toggle"),
    sensitivityField: $("#peaking-sensitivity-field"),
    sensitivity: $("#peaking-sensitivity"),
    overlay: $("#peaking-overlay"),
    histogramPanel: $("#histogram-panel"),
    histogramCanvas: $("#histogram-canvas"),
    histogramClip: $("#histogram-clip"),
    loupePanel: $("#loupe-panel"),
    loupeCanvas: $("#loupe-canvas"),
    loupeScore: $("#loupe-score"),
    loupeBest: $("#loupe-best"),
    loupeReset: $("#loupe-reset"),
    loupeScale: $("#loupe-scale"),
    preview: $("#preview"),
  };
  if (!ui.histogramToggle || !ui.preview) return;

  const state = {
    histogram: false,
    peaking: false,
    clipping: false,
    loupe: false,
    sensitivity: 0.5,
    point: null, // normalized { x, y } in 0..1; null = frame centre
    best: 0,
  };

  const work = document.createElement("canvas");
  const workContext = work.getContext("2d", { willReadFrequently: true });
  const crop = document.createElement("canvas");
  const cropContext = crop.getContext("2d", { willReadFrequently: true });

  function loadState() {
    try {
      const saved = JSON.parse(localStorage.getItem(STORAGE_KEY) || "{}");
      state.histogram = saved.histogram === true;
      state.peaking = saved.peaking === true;
      state.clipping = saved.clipping === true;
      state.loupe = saved.loupe === true;
      if (Number.isFinite(saved.sensitivity)) state.sensitivity = saved.sensitivity;
    } catch (_) {
      // Storage unavailable (private window, blocked site data): defaults.
    }
  }

  function saveState() {
    try {
      localStorage.setItem(
        STORAGE_KEY,
        JSON.stringify({
          histogram: state.histogram,
          peaking: state.peaking,
          clipping: state.clipping,
          loupe: state.loupe,
          sensitivity: state.sensitivity,
        }),
      );
    } catch (_) {
      // Per-viewer convenience only.
    }
  }

  function anyEnabled() {
    return state.histogram || state.peaking || state.clipping || state.loupe;
  }

  function syncControls() {
    ui.histogramToggle.setAttribute("aria-pressed", String(state.histogram));
    ui.peakingToggle.setAttribute("aria-pressed", String(state.peaking));
    ui.clippingToggle?.setAttribute("aria-pressed", String(state.clipping));
    ui.loupeToggle.setAttribute("aria-pressed", String(state.loupe));
    ui.sensitivityField.hidden = !state.peaking;
    ui.sensitivity.value = String(Math.round(state.sensitivity * 100));
    ui.histogramPanel.hidden = !state.histogram;
    ui.loupePanel.hidden = !state.loupe;
    ui.overlay.hidden = !(state.peaking || state.clipping || state.loupe);
    ui.preview.classList.toggle("loupe-target", state.loupe);
    if (!state.histogram) ui.histogramClip.textContent = "";
    if (!state.loupe) resetBest();
  }

  function resetBest() {
    state.best = 0;
    ui.loupeBest.textContent = "–";
  }

  function toggle(key) {
    state[key] = !state[key];
    saveState();
    syncControls();
    if (anyEnabled() && frameWidth(ui.preview)) onFrame(ui.preview);
  }

  function drawHistogram(histogram) {
    const canvas = ui.histogramCanvas;
    const context = canvas.getContext("2d");
    const { width, height } = canvas;
    context.clearRect(0, 0, width, height);
    // Scale to the tallest interior bin so a clipped 0/255 spike does not
    // flatten the rest of the curve.
    let peak = 1;
    for (let bin = 1; bin < 255; bin += 1) peak = Math.max(peak, histogram.luma[bin]);
    const barWidth = width / 256;
    context.fillStyle = "rgba(237, 247, 244, 0.35)";
    for (let bin = 0; bin < 256; bin += 1) {
      const barHeight = Math.min(1, histogram.luma[bin] / peak) * height;
      context.fillRect(bin * barWidth, height - barHeight, Math.ceil(barWidth), barHeight);
    }
    const channels = [
      [histogram.r, "rgba(255, 107, 107, 0.9)"],
      [histogram.g, "rgba(92, 225, 167, 0.9)"],
      [histogram.b, "rgba(110, 160, 255, 0.9)"],
    ];
    context.lineWidth = 1;
    for (const [bins, colour] of channels) {
      context.strokeStyle = colour;
      context.beginPath();
      for (let bin = 0; bin < 256; bin += 1) {
        const y = height - Math.min(1, bins[bin] / peak) * height;
        if (bin === 0) context.moveTo(0, y);
        else context.lineTo(bin * barWidth, y);
      }
      context.stroke();
    }
    const percent = (fraction) => `${(fraction * 100).toFixed(fraction < 0.1 ? 1 : 0)}%`;
    ui.histogramClip.textContent = `Shadows clipped ${percent(histogram.shadowClip)} · Highlights clipped ${percent(histogram.highlightClip)}`;
  }

  function focusPoint(naturalWidth, naturalHeight) {
    const point = state.point || { x: 0.5, y: 0.5 };
    return { x: point.x * naturalWidth, y: point.y * naturalHeight };
  }

  function drawOverlay(image, width, height, scale) {
    const overlay = ui.overlay;
    if (overlay.width !== width || overlay.height !== height) {
      overlay.width = width;
      overlay.height = height;
    }
    const context = overlay.getContext("2d");
    context.clearRect(0, 0, width, height);
    if (state.peaking || state.clipping) {
      // One ImageData for both: putImageData replaces, it doesn't blend.
      const mask = context.createImageData(width, height);
      if (state.peaking) {
        const gray = toGray(image);
        const magnitude = sobelMagnitude(gray, width, height);
        const threshold = peakingThreshold(state.sensitivity);
        for (let pixel = 0, index = 0; pixel < magnitude.length; pixel += 1, index += 4) {
          if (magnitude[pixel] >= threshold) {
            mask.data[index] = 255;
            mask.data[index + 1] = 40;
            mask.data[index + 2] = 90;
            mask.data[index + 3] = 255;
          }
        }
      }
      if (state.clipping) {
        // Zebra stripes, red and dark, so they show on a white blow-out.
        const clipped = clippedMask(image).mask;
        for (let pixel = 0, index = 0; pixel < clipped.length; pixel += 1, index += 4) {
          if (!clipped[pixel]) continue;
          const x = pixel % width;
          const y = (pixel - x) / width;
          const stripe = ((x + y) >> 2) & 1;
          mask.data[index] = stripe ? 235 : 20;
          mask.data[index + 1] = stripe ? 30 : 20;
          mask.data[index + 2] = stripe ? 30 : 20;
          mask.data[index + 3] = stripe ? 255 : 170;
        }
      }
      context.putImageData(mask, 0, 0);
    }
    if (state.loupe) {
      const naturalWidth = width / scale;
      const naturalHeight = height / scale;
      const centre = focusPoint(naturalWidth, naturalHeight);
      const rect = loupeRect(centre.x, centre.y, LOUPE_SOURCE, naturalWidth, naturalHeight);
      context.strokeStyle = "rgba(92, 225, 167, 0.95)";
      context.lineWidth = Math.max(1, width / 480);
      context.strokeRect(rect.x * scale, rect.y * scale, rect.width * scale, rect.height * scale);
    }
  }

  function drawLoupe(img) {
    const naturalWidth = frameWidth(img);
    const naturalHeight = frameHeight(img);
    const centre = focusPoint(naturalWidth, naturalHeight);
    const rect = loupeRect(centre.x, centre.y, LOUPE_SOURCE, naturalWidth, naturalHeight);

    // Score the crop at source resolution, then show it magnified.
    crop.width = rect.width;
    crop.height = rect.height;
    cropContext.drawImage(
      img,
      rect.x,
      rect.y,
      rect.width,
      rect.height,
      0,
      0,
      rect.width,
      rect.height,
    );
    const pixels = cropContext.getImageData(0, 0, rect.width, rect.height);
    const score = sharpnessScore(toGray(pixels), rect.width, rect.height);
    state.best = Math.max(state.best, score);

    const canvas = ui.loupeCanvas;
    canvas.width = rect.width * LOUPE_ZOOM;
    canvas.height = rect.height * LOUPE_ZOOM;
    const context = canvas.getContext("2d");
    context.imageSmoothingEnabled = false;
    context.drawImage(crop, 0, 0, canvas.width, canvas.height);

    ui.loupeScore.textContent = `${Math.round(score)}${state.best ? ` · ${Math.round((score / state.best) * 100)}% of best` : ""}`;
    ui.loupeBest.textContent = String(Math.round(state.best));
    ui.loupeScale.textContent =
      naturalWidth >= SENSOR_WIDTH
        ? `1:1 sensor pixels, shown ${LOUPE_ZOOM}×`
        : `Preview pixels (${naturalWidth} × ${naturalHeight}), shown ${LOUPE_ZOOM}×. For 1:1 sensor pixels use Master Archive with downsampling off.`;
  }

  function onFrame(img) {
    if (!anyEnabled() || !img.naturalWidth || !img.naturalHeight) return;
    const scale = Math.min(1, WORK_LONG_EDGE / Math.max(img.naturalWidth, img.naturalHeight));
    const width = Math.max(1, Math.round(img.naturalWidth * scale));
    const height = Math.max(1, Math.round(img.naturalHeight * scale));
    try {
      if (state.histogram || state.peaking || state.clipping) {
        if (work.width !== width || work.height !== height) {
          work.width = width;
          work.height = height;
        }
        workContext.drawImage(img, 0, 0, width, height);
        const image = workContext.getImageData(0, 0, width, height);
        if (state.histogram) drawHistogram(computeHistogram(image));
        if (state.peaking || state.clipping || state.loupe) {
          drawOverlay(image, width, height, scale);
        }
      } else if (state.loupe) {
        drawOverlay({ data: new Uint8ClampedArray(0), width, height }, width, height, scale);
      }
      if (state.loupe) drawLoupe(img);
    } catch (error) {
      // Never let a tool break the preview; report once in the console.
      console.warn("Focus tools skipped a frame:", error);
    }
  }

  function clear() {
    const context = ui.overlay.getContext("2d");
    context.clearRect(0, 0, ui.overlay.width, ui.overlay.height);
    ui.histogramCanvas
      .getContext("2d")
      .clearRect(0, 0, ui.histogramCanvas.width, ui.histogramCanvas.height);
    ui.histogramClip.textContent = "";
    ui.loupeScore.textContent = "–";
  }

  ui.histogramToggle.addEventListener("click", () => toggle("histogram"));
  ui.peakingToggle.addEventListener("click", () => toggle("peaking"));
  ui.clippingToggle?.addEventListener("click", () => toggle("clipping"));
  ui.loupeToggle.addEventListener("click", () => toggle("loupe"));
  ui.sensitivity.addEventListener("input", () => {
    state.sensitivity = Number(ui.sensitivity.value) / 100;
    saveState();
  });
  ui.loupeReset.addEventListener("click", resetBest);
  // Press to place the loupe, drag to move it. Redraws are coalesced to one
  // per animation frame so a drag over a full-resolution frame stays smooth.
  let dragPointer = null;
  let redrawPending = false;

  function moveLoupeTo(event, clamp) {
    const naturalWidth = frameWidth(ui.preview);
    const naturalHeight = frameHeight(ui.preview);
    const point = clientToImagePoint(
      event.clientX,
      event.clientY,
      ui.preview.getBoundingClientRect(),
      naturalWidth,
      naturalHeight,
      clamp,
    );
    if (!point) return false;
    state.point = { x: point.x / naturalWidth, y: point.y / naturalHeight };
    resetBest();
    if (!redrawPending) {
      redrawPending = true;
      requestAnimationFrame(() => {
        redrawPending = false;
        onFrame(ui.preview);
      });
    }
    return true;
  }

  function endDrag(event) {
    if (event.pointerId !== dragPointer) return;
    dragPointer = null;
    if (ui.preview.hasPointerCapture(event.pointerId)) {
      ui.preview.releasePointerCapture(event.pointerId);
    }
  }

  ui.preview.addEventListener("pointerdown", (event) => {
    if (!state.loupe || event.button !== 0) return;
    if (!moveLoupeTo(event, false)) return;
    event.preventDefault();
    dragPointer = event.pointerId;
    ui.preview.setPointerCapture(event.pointerId);
  });
  ui.preview.addEventListener("pointermove", (event) => {
    if (event.pointerId === dragPointer && state.loupe) moveLoupeTo(event, true);
  });
  ui.preview.addEventListener("pointerup", endDrag);
  ui.preview.addEventListener("pointercancel", endDrag);
  // Stop the browser's own image drag (ghost image) from hijacking the drag.
  ui.preview.addEventListener("dragstart", (event) => {
    if (state.loupe) event.preventDefault();
  });

  loadState();
  syncControls();

  window.OpticFocus = { ...pure, onFrame, clear };
})();
