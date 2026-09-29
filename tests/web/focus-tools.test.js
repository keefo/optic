"use strict";

// Unit tests for the pure functions in src/web/focus-tools.js.
// Run: node --test tests/web/*.test.js

const test = require("node:test");
const assert = require("node:assert/strict");
const focus = require("../../src/web/focus-tools.js");

function solid(width, height, [r, g, b]) {
  const data = new Uint8ClampedArray(width * height * 4);
  for (let index = 0; index < data.length; index += 4) {
    data[index] = r;
    data[index + 1] = g;
    data[index + 2] = b;
    data[index + 3] = 255;
  }
  return { data, width, height };
}

// Left half `low`, right half `high` (grey levels): a vertical step edge.
function step(width, height, low, high) {
  const image = solid(width, height, [low, low, low]);
  for (let y = 0; y < height; y += 1) {
    for (let x = width / 2; x < width; x += 1) {
      const index = (y * width + x) * 4;
      image.data[index] = high;
      image.data[index + 1] = high;
      image.data[index + 2] = high;
    }
  }
  return image;
}

function boxBlur(gray, width, height, radius) {
  const out = new Float32Array(gray.length);
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      let sum = 0;
      let count = 0;
      for (let dx = -radius; dx <= radius; dx += 1) {
        const sx = Math.min(width - 1, Math.max(0, x + dx));
        sum += gray[y * width + sx];
        count += 1;
      }
      out[y * width + x] = sum / count;
    }
  }
  return out;
}

test("histogram: all black is 100% shadow clip, 0% highlight", () => {
  const histogram = focus.computeHistogram(solid(4, 3, [0, 0, 0]));
  assert.equal(histogram.total, 12);
  assert.equal(histogram.luma[0], 12);
  assert.equal(histogram.shadowClip, 1);
  assert.equal(histogram.highlightClip, 0);
});

test("histogram: all white is 100% highlight clip, 0% shadow", () => {
  const histogram = focus.computeHistogram(solid(4, 3, [255, 255, 255]));
  assert.equal(histogram.luma[255], 12);
  assert.equal(histogram.highlightClip, 1);
  assert.equal(histogram.shadowClip, 0);
});

test("histogram: one blown channel counts as highlight clip", () => {
  const histogram = focus.computeHistogram(solid(2, 2, [255, 10, 10]));
  assert.equal(histogram.highlightClip, 1);
  assert.equal(histogram.r[255], 4);
  assert.equal(histogram.g[10], 4);
});

test("histogram: two-value image lands in the right bins", () => {
  const histogram = focus.computeHistogram(step(4, 2, 40, 200));
  assert.equal(histogram.luma[40], 4);
  assert.equal(histogram.luma[200], 4);
  assert.equal(histogram.shadowClip, 0);
  assert.equal(histogram.highlightClip, 0);
});

test("histogram: Rec. 709 luma weights for pure R, G, B", () => {
  assert.equal(focus.computeHistogram(solid(1, 1, [255, 0, 0])).luma[54], 1);
  assert.equal(focus.computeHistogram(solid(1, 1, [0, 255, 0])).luma[182], 1);
  assert.equal(focus.computeHistogram(solid(1, 1, [0, 0, 255])).luma[18], 1);
});

test("sobel: vertical step edge has magnitude only at the edge columns", () => {
  const width = 8;
  const height = 6;
  const gray = focus.toGray(step(width, height, 0, 100));
  const magnitude = focus.sobelMagnitude(gray, width, height);
  for (let y = 1; y < height - 1; y += 1) {
    for (let x = 1; x < width - 1; x += 1) {
      const value = magnitude[y * width + x];
      if (x === 3 || x === 4) assert.ok(value > 300, `edge at (${x},${y}) = ${value}`);
      else assert.equal(value, 0, `flat at (${x},${y})`);
    }
  }
});

test("peaking threshold: monotonic, clamped, and keeps only edge pixels", () => {
  assert.equal(focus.peakingThreshold(0), 220);
  assert.equal(focus.peakingThreshold(1), 40);
  assert.equal(focus.peakingThreshold(5), 40);
  assert.equal(focus.peakingThreshold(-1), 220);
  assert.equal(focus.peakingThreshold("junk"), 220);
  assert.ok(focus.peakingThreshold(0.25) > focus.peakingThreshold(0.75));

  const width = 8;
  const height = 6;
  const magnitude = focus.sobelMagnitude(focus.toGray(step(width, height, 0, 100)), width, height);
  const threshold = focus.peakingThreshold(0.5);
  for (let x = 1; x < width - 1; x += 1) {
    const kept = magnitude[2 * width + x] >= threshold;
    assert.equal(kept, x === 3 || x === 4, `column ${x}`);
  }
});

test("sharpness: a sharp step scores higher than the same step blurred", () => {
  const width = 32;
  const height = 16;
  const gray = focus.toGray(step(width, height, 30, 220));
  const sharp = focus.sharpnessScore(gray, width, height);
  const blurred = focus.sharpnessScore(boxBlur(gray, width, height, 3), width, height);
  assert.ok(sharp > blurred * 2, `sharp=${sharp} blurred=${blurred}`);
  assert.equal(focus.sharpnessScore(focus.toGray(solid(8, 8, [90, 90, 90])), 8, 8), 0);
  assert.equal(focus.sharpnessScore(new Float32Array(4), 2, 2), 0);
});

test("loupeRect: centred, and clamped at every corner", () => {
  assert.deepEqual(focus.loupeRect(500, 400, 160, 1000, 800), { x: 420, y: 320, width: 160, height: 160 });
  assert.deepEqual(focus.loupeRect(0, 0, 160, 1000, 800), { x: 0, y: 0, width: 160, height: 160 });
  assert.deepEqual(focus.loupeRect(1000, 0, 160, 1000, 800), { x: 840, y: 0, width: 160, height: 160 });
  assert.deepEqual(focus.loupeRect(0, 800, 160, 1000, 800), { x: 0, y: 640, width: 160, height: 160 });
  assert.deepEqual(focus.loupeRect(1000, 800, 160, 1000, 800), { x: 840, y: 640, width: 160, height: 160 });
});

test("loupeRect: shrinks to a frame smaller than the crop", () => {
  assert.deepEqual(focus.loupeRect(50, 30, 160, 100, 60), { x: 0, y: 0, width: 100, height: 60 });
});

test("clientToImagePoint: letterboxed top/bottom (wide image in tall box)", () => {
  // 400x200 image in a 400x400 box: drawn at scale 1, 100 px bars above and below.
  const rect = { left: 10, top: 20, width: 400, height: 400 };
  assert.deepEqual(focus.clientToImagePoint(10, 120, rect, 400, 200), { x: 0, y: 0 });
  assert.deepEqual(focus.clientToImagePoint(410, 320, rect, 400, 200), { x: 400, y: 200 });
  assert.equal(focus.clientToImagePoint(200, 50, rect, 400, 200), null);
});

test("clientToImagePoint: pillarboxed left/right and scaled", () => {
  // 4056x3040 image in an 800x400 box: scale = 400/3040, drawn width ~533.7.
  const rect = { left: 0, top: 0, width: 800, height: 400 };
  const scale = 400 / 3040;
  const offsetX = (800 - 4056 * scale) / 2;
  const centre = focus.clientToImagePoint(400, 200, rect, 4056, 3040);
  assert.ok(Math.abs(centre.x - 2028) < 1e-6 && Math.abs(centre.y - 1520) < 1e-6);
  assert.equal(focus.clientToImagePoint(offsetX - 1, 200, rect, 4056, 3040), null);
  assert.equal(focus.clientToImagePoint(10, 10, { left: 0, top: 0, width: 0, height: 0 }, 4056, 3040), null);
});

test("clientToImagePoint: clamp pins drags outside the image to its edges", () => {
  // 400x200 image in a 400x400 box: 100 px bars above and below.
  const rect = { left: 10, top: 20, width: 400, height: 400 };
  // In the top bar, and far outside the element: pinned to the image edges.
  assert.deepEqual(focus.clientToImagePoint(200, 50, rect, 400, 200, true), { x: 190, y: 0 });
  assert.deepEqual(focus.clientToImagePoint(-500, 900, rect, 400, 200, true), { x: 0, y: 200 });
  // Inside points are unchanged by clamping.
  assert.deepEqual(focus.clientToImagePoint(110, 170, rect, 400, 200, true), { x: 100, y: 50 });
  // Without clamp the default is still null outside the image.
  assert.equal(focus.clientToImagePoint(200, 50, rect, 400, 200), null);
  // An unsized element still gives null, even with clamp.
  assert.equal(focus.clientToImagePoint(1, 1, { left: 0, top: 0, width: 0, height: 0 }, 400, 200, true), null);
});

test("clipping marks BT.601 luma >= 250, the daemon's clipped threshold", () => {
  assert.equal(focus.clippedMask(solid(4, 4, [250, 250, 250])).fraction, 1);
  assert.equal(focus.clippedMask(solid(4, 4, [249, 249, 249])).fraction, 0);
  // Saturated colour is not clipped luma: pure red is Y = 76.
  assert.equal(focus.clippedMask(solid(4, 4, [255, 0, 0])).fraction, 0);
  // Warm lamp light either side of the threshold: Y = 248.7 and 250.4.
  assert.equal(focus.clippedMask(solid(4, 4, [255, 255, 200])).fraction, 0);
  assert.equal(focus.clippedMask(solid(4, 4, [255, 255, 215])).fraction, 1);
});

test("clipping reports which pixels and what fraction", () => {
  const { mask, fraction } = focus.clippedMask(step(8, 2, 40, 255));
  assert.equal(fraction, 0.5);
  assert.deepEqual(Array.from(mask.slice(0, 8)), [0, 0, 0, 0, 1, 1, 1, 1]);
});

test("frameWidth/frameHeight read a canvas as well as an image", () => {
  // The preview became a <canvas> with the iOS flicker fix. A canvas has
  // width/height and no naturalWidth, so reading naturalWidth directly
  // disabled every focus tool (2026-09-29 regression).
  const canvas = { width: 1352, height: 1014 };
  assert.equal(focus.frameWidth(canvas), 1352);
  assert.equal(focus.frameHeight(canvas), 1014);

  const img = { naturalWidth: 4056, naturalHeight: 3040, width: 800, height: 600 };
  assert.equal(focus.frameWidth(img), 4056, "an image's intrinsic size wins over its layout size");
  assert.equal(focus.frameHeight(img), 3040);

  // A frame source that is not ready yet reports 0, which callers treat as
  // "skip this frame" rather than dividing by undefined.
  assert.equal(focus.frameWidth({}), 0);
  assert.equal(focus.frameHeight({}), 0);
  assert.equal(focus.frameWidth({ naturalWidth: 0, width: 0 }), 0);
});

