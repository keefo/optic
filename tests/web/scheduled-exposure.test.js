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

const guardPlan = (extra = {}) => ({
  seeding: false,
  shutter_us: 3000000,
  gain: 1,
  sun_elevation_deg: -25,
  highlight_active: true,
  highlight_ev: -0.5,
  highlight_target_ev: -1.41,
  preview: { shutter_us: 1130000, gain: 1 },
  preview_unguarded: { shutter_us: 4227272, gain: 1.13 },
  ...extra,
});

test("the preview runs the guard target, or the unguarded frame while comparing", () => {
  assert.equal(ramp.previewSource(null), null);
  assert.deepEqual(ramp.previewSource({ seeding: true }), { seeding: true });
  assert.deepEqual(ramp.previewSource(guardPlan()), {
    seeding: false,
    shutter_us: 1130000,
    gain: 1,
  });
  assert.deepEqual(ramp.previewSource(guardPlan(), true), {
    seeding: false,
    shutter_us: 4227272,
    gain: 1.13,
  });
  // An older daemon without preview plans: the next frame, as before.
  const old = { seeding: false, shutter_us: 3000000, gain: 1 };
  assert.equal(ramp.previewSource(old), old);
  assert.equal(ramp.previewSource(old, true), old);
});

test("the guard line reports active, inactive and off states", () => {
  assert.equal(ramp.describeGuardLine(null, 1), "");
  // A daemon without the guard fields shows nothing.
  assert.equal(ramp.describeGuardLine({ seeding: false }, 1), "");
  assert.equal(
    ramp.describeGuardLine(guardPlan(), 1, 0.0265),
    "Highlight guard · budget 1.00% · clipped now 2.65% · preview −1.4 EV · timelapse −0.5 EV",
  );
  assert.equal(
    ramp.describeGuardLine(guardPlan(), 0.5),
    "Highlight guard · budget 0.50% · preview −1.4 EV · timelapse −0.5 EV",
  );
  assert.equal(
    ramp.describeGuardLine(guardPlan({ highlight_active: false, sun_elevation_deg: 12.34 }), 1, 0.05),
    "Highlight guard inactive (sun 12.3°); it acts only with the sun below the horizon · clipped now 5.00%",
  );
  assert.equal(
    ramp.describeGuardLine(guardPlan({ highlight_active: false, sun_elevation_deg: null }), 1),
    "Highlight guard inactive (no station set); it acts only with the sun below the horizon",
  );
  assert.equal(
    ramp.describeGuardLine(guardPlan({ highlight_active: false }), 0, 0.123),
    "Highlight guard off (budget 0%) · clipped now 12.3%",
  );
});

test("ramp defaults mirror RampSettings::default()", () => {
  assert.equal(ramp.RAMP_DEFAULTS.clip_budget_percent, 1);
});

test("the preview is re-requested only for a real change", () => {
  const source = (shutter_us, gain = 1) => ({ seeding: false, shutter_us, gain });
  const seeding = { seeding: true };
  // First request, and switching Scheduled exposure off or on.
  assert.equal(ramp.previewNeedsUpdate(null, source(600000)), true);
  assert.equal(ramp.previewNeedsUpdate(source(600000), null), true);
  assert.equal(ramp.previewNeedsUpdate(null, null), false);
  // Seeding starts or ends.
  assert.equal(ramp.previewNeedsUpdate(seeding, source(600000)), true);
  assert.equal(ramp.previewNeedsUpdate(source(600000), seeding), true);
  assert.equal(ramp.previewNeedsUpdate(seeding, { seeding: true }), false);
  // Measured jitter on 2026-09-23: gain 5.22-6.87 around 118,745 µs. A step
  // inside 1/6 EV is left alone; a larger one goes through.
  assert.equal(ramp.previewNeedsUpdate(source(118745, 6.28), source(118745, 6.48)), false);
  assert.equal(ramp.previewNeedsUpdate(source(650000), source(600000)), false); // 0.12 EV
  assert.equal(ramp.previewNeedsUpdate(source(700000), source(600000)), true); // 0.22 EV
  assert.equal(ramp.previewNeedsUpdate(source(118745, 5.22), source(118745, 6.87)), true);
  assert.equal(ramp.previewNeedsUpdate(source(1000000), source(500000)), true);
});
