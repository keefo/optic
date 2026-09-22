"use strict";

// Unit tests for the pure functions in src/web/scheduled-exposure.js.
// Run: node --test tests/web/*.test.js

const test = require("node:test");
const assert = require("node:assert/strict");
const ramp = require("../../src/web/scheduled-exposure.js");

const manualPlan = (shutter_us, gain) => ({ seeding: false, shutter_us, gain });

test("no plan means no preview override", () => {
  assert.equal(ramp.previewEquivalent(null, 8), null);
});

test("a seeding plan previews with auto exposure", () => {
  assert.deepEqual(ramp.previewEquivalent({ seeding: true }, 8), {
    shutter_us: 0,
    gain: 0,
    shortfall_ev: 0,
  });
});

test("a short shutter passes through unchanged", () => {
  const preview = ramp.previewEquivalent(manualPlan(10000, 1), 8);
  assert.equal(preview.shutter_us, 10000);
  assert.equal(preview.gain, 1);
  assert.equal(preview.shortfall_ev, 0);
});

test("a long shutter is clamped to the preview frame and moved into gain", () => {
  // 8 FPS -> 125 ms frame, 95% of it usable.
  const preview = ramp.previewEquivalent(manualPlan(500000, 2), 8);
  assert.equal(preview.shutter_us, 118750);
  // Same total exposure: 500000 x 2 = 118750 x 8.42.
  assert.equal(preview.gain, 8.42);
  assert.equal(preview.shortfall_ev, 0);
});

test("beyond 16x gain the preview reports how much darker it is", () => {
  const preview = ramp.previewEquivalent(manualPlan(4000000, 1), 8);
  assert.equal(preview.shutter_us, 118750);
  assert.equal(preview.gain, 16);
  // 4e6 / 118750 = 33.7x needed -> log2(33.7 / 16) = 1.07 EV short.
  assert.ok(Math.abs(preview.shortfall_ev - 1.074) < 0.01, preview.shortfall_ev);
});

test("a slower preview allows a longer shutter", () => {
  const preview = ramp.previewEquivalent(manualPlan(4000000, 1), 2);
  assert.equal(preview.shutter_us, 475000);
  assert.ok(Math.abs(preview.gain - 8.42) < 0.01);
  assert.equal(preview.shortfall_ev, 0);
});

test("night look and night drop mirror each other", () => {
  assert.equal(ramp.nightLookFromDrop(2), 2);
  assert.equal(ramp.nightLookFromDrop(0), 4);
  assert.equal(ramp.nightLookFromDrop(4), 0);
  // An Advanced-only drop beyond the slider range clamps the slider.
  assert.equal(ramp.nightLookFromDrop(6), 0);
  for (const look of [0, 0.5, 2, 3.5, 4]) {
    assert.equal(ramp.nightLookFromDrop(ramp.dropFromNightLook(look)), look);
  }
});

test("shutter and EV formatting", () => {
  assert.equal(ramp.formatShutter(1000), "1/1000 s");
  assert.equal(ramp.formatShutter(66654), "1/15 s");
  assert.equal(ramp.formatShutter(250000), "250 ms");
  assert.equal(ramp.formatShutter(3843022), "3.8 s");
  assert.equal(ramp.formatEv(-2), "−2.0 EV");
  assert.equal(ramp.formatEv(0.04), "0.0 EV");
  assert.equal(ramp.formatEv(1.25), "+1.3 EV");
});

test("plan descriptions for the locked fields", () => {
  assert.equal(ramp.describePlan(null), null);
  // Seeding without a running preview: nothing measured yet.
  assert.deepEqual(ramp.describePlan({ seeding: true }), {
    shutter: "Auto",
    gain: "Auto",
  });
  // White balance is not part of the ramp, so it is not described here.
  assert.deepEqual(ramp.describePlan(manualPlan(4226589, 1.1454139)), {
    shutter: "4.2 s",
    gain: "1.15×",
  });
});

test("while seeding, the locked fields show the preview's live auto exposure", () => {
  const preview = { exposureUs: 66654, analogueGain: 15.515152 };
  assert.deepEqual(ramp.describePlan({ seeding: true }, preview), {
    shutter: "1/15 s",
    gain: "15.5×",
  });
  // Partial or missing metadata falls back per field.
  assert.deepEqual(
    ramp.describePlan({ seeding: true }, { exposureUs: 8000, analogueGain: null }),
    { shutter: "1/125 s", gain: "Auto" },
  );
  assert.equal(ramp.describePlan({ seeding: true }, { exposureUs: 0 }).shutter, "Auto");
});

test("a ramped plan ignores the preview's (override) values", () => {
  const preview = { exposureUs: 118750, analogueGain: 16 };
  assert.equal(ramp.describePlan(manualPlan(3843022, 1), preview).shutter, "3.8 s");
  assert.equal(ramp.describePlan(manualPlan(3843022, 1), preview).gain, "1.00×");
});
