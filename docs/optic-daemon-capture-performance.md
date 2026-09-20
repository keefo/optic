# Investigation: Still-Capture Latency ("Capture & transfer" is slow)

## 1. Problem Statement

Clicking "Capture & transfer" in the dashboard takes **~5–5.6 seconds**
end-to-end, regardless of capture profile (Master Archive with DNG, or 2K
Binning JPEG-only measured the same). This is the full HTTP round-trip —
`api/capture`'s handler doesn't return until the native camera work is
done — so it's directly felt by the operator as "the button just hangs."

This document records the measurement methodology, the raw findings, the
root-cause conclusion, and open directions for actually fixing it. It is a
reference for future iteration, not a worklog — no fix has been
implemented yet as of this writing.

## 2. Methodology

### 2.1 Instrumentation added

`tracing::info!` calls emitting a consistent `"capture perf"` message with a
`stage` field and `elapsed_ms` (or `since_previous_ms`) were added at every
step of the capture path, so a single grep (`grep "capture perf"`) produces
a clean, ordered trace of one capture request:

- `src/web.rs::capture()` — `http_handler_total`: the full HTTP handler,
  from receiving the request to the actor reply. Used to confirm there's no
  hidden queueing/network overhead beyond the native camera work itself.
- `src/native_camera.rs`, inside the `NativeCommand::Capture` match arm:
  - `stop_existing_preview` — stopping any active live-preview stream
    before reconfiguring for still capture.
  - `capture_frame_total` — wraps the whole `capture_frame` call.
  - `publish_capture` — wraps the whole `publish_capture` call.
  - `total` — the sum of the above, tagged with `profile`/`save_dng`.
- Inside `capture_frame`:
  - `pipeline_start` — `start_pipeline`'s cost (stream config, buffer
    allocation, queuing the initial batch of requests).
  - `warmup_frame` (one log line **per frame** received during the warmup
    loop) — `frame_index`, `exposure_us` (read back from that frame's own
    completed-request metadata via `controls::ExposureTime` — the value
    libcamera actually used, not what was requested), and
    `since_previous_ms` (host-observed wall-clock gap since the previous
    frame arrived).
  - `warmup_and_capture` — the whole warmup-then-grab-the-real-frame loop.
  - `camera_stop` — the post-capture `camera.stop()`.
- Inside `publish_capture`:
  - `jpeg_encode`, `dng_decode_and_encode` (only when `save_dng`), and
    `disk_write` — each separately timed and tagged with byte counts where
    relevant.

All of this runs at `info` level, matching the deployed
`Environment=RUST_LOG=info`, so no log-level change is needed to see it —
**in principle**. See 2.2 for why that wasn't actually usable as-is.

### 2.2 Environment obstacle: journald has no persistent storage on this Pi

`journalctl --user -u optic-daemon.service` returns **nothing** for this
service, even immediately after triggering a capture. Confirmed via
`journalctl --user --disk-usage` → `No journal files were found. Archived
and active journals take up 0B in the file system.` Even attaching
`journalctl --user -u optic-daemon.service -f` (live follow) *while*
triggering a capture captured zero lines — so this isn't a storage-vs-query
issue, journald is not retaining or forwarding these messages at all on
this host.

**Workaround used for every measurement in this document:** temporarily
stop the systemd-managed service, run the exact same installed binary
manually with the same environment variables, redirecting stdout/stderr to
a plain file:

```bash
systemctl --user stop optic-daemon.service
OPTIC_BIND_ADDR=0.0.0.0:8000 \
OPTIC_CAPTURE_DIR=/mnt/capture \
OPTIC_SYNC_REMOTE_HOST=imacpro.local \
RUST_LOG=info \
LD_LIBRARY_PATH=/home/liam/.local/optic-sysroot/usr/lib/aarch64-linux-gnu \
nohup /home/liam/.local/bin/optic-daemon > /tmp/optic-perf.log 2>&1 &
disown
# ... trigger captures via curl against http://optic.local:8000 ...
# then: pkill -f "/home/liam/.local/bin/optic-daemon"
systemctl --user start optic-daemon.service   # restore normal operation
```

This is a real, if minor, gap in this deployment's observability — anyone
debugging this daemon live will hit the same wall. Worth fixing
independently of the capture-latency work (e.g. enabling persistent
journal storage, or an explicit log file with rotation), but out of scope
here.

**Update 2026-09-19:** persistent journal storage was enabled (see
`worklogs/2026-09-19-persistent-journal-crash-evidence.md`) — journald now
uses bounded persistent storage (`Storage=persistent`, 16 MiB cap) instead
of pure RAM-only volatile storage, so logs survive a reboot. This closes
the "logs disappear" half of this gap: the same lines that used to vanish
are now durably queryable via `journalctl -u user@1000.service` (the
system-scoped view of the user manager) or an unscoped `journalctl | grep
optic-daemon`. However, the exact invocation form documented above,
`journalctl --user -u optic-daemon.service`, **still** returns nothing —
that specific quirk is unrelated to volatile-vs-persistent storage and
remains unresolved. Use the system-scoped query form instead until that's
separately investigated.

## 3. Findings

### 3.1 Stage breakdown (single capture, each profile)

| Stage | Master Archive + DNG | 2K Binning (JPEG only) |
|---|---|---|
| Stop existing preview | 8 ms | 0 ms |
| Start still-capture pipeline | 70 ms | 49 ms |
| **Warmup + capture loop** | **5,167 ms** | **5,037 ms** |
| Stop camera | 4 ms | 3 ms |
| `capture_frame` total | 5,243 ms | 5,091 ms |
| JPEG encode | 121 ms (7.8 MB) | 20 ms (0.5 MB) |
| DNG decode+encode | 73 ms (24.7 MB) | — |
| Disk write | 12 ms | 0 ms |
| `publish_capture` total | 209 ms | 21 ms |
| **Grand total (= HTTP handler total)** | **5,461 ms** | **5,112 ms** |

Conclusion: encoding and disk I/O together are 20–210ms — noise. Pipeline
start/stop is tens of milliseconds — noise. **The warmup+capture loop is
>90% of total latency in both profiles**, and is essentially the same
regardless of resolution/DNG.

### 3.2 Does warmup only cost once? (consecutive-capture test)

Four `binning_2k` captures fired back-to-back, no delay between them:

| Shot | Warmup+capture | Total |
|---|---|---|
| 1 | 5,037 ms | 5,113 ms |
| 2 | 5,037 ms | 5,108 ms |
| 3 | 5,542 ms (jitter) | 5,620 ms |
| 4 | 5,037 ms | 5,109 ms |

Flat — no downward trend. This matches the code: `capture_frame` calls
`start_pipeline` (fresh reconfigure) at the start and `camera.stop()` (full
teardown) at the end of **every** call, so nothing persists between shots
for a "warm" state to exist. A burst of N captures costs ~5s × N, linearly.

### 3.3 Root cause: libcamera pacing, not sensor/exposure time

Per-frame trace during the warmup loop (`binning_2k`, `shutter_us: 0` i.e.
requested-auto, but note AE is fully disabled — see 3.3.1):

| Frame | `exposure_us` (actual, from completed-request metadata) | `since_previous_ms` |
|---|---|---|
| 1–11 | **762** (0.762 ms) | ~499–554 |

Every frame's *real, camera-reported* exposure time is under 1
millisecond. Yet frames only arrive roughly every 500ms — a ~650×
mismatch between what the sensor actually needs and how long libcamera
makes the caller wait between frames.

**This rules out the hardware/sensor as the bottleneck.** If frame delivery
were gated by actual exposure/readout time, the ~500ms figure would show up
in `exposure_us` too. It doesn't. Something in the libcamera stack is
pacing frame delivery to ~2 FPS independent of the real exposure need.

#### 3.3.1 Why exposure is 762µs even though `shutter_us: 0` ("auto") was requested

`apply_controls` (`src/native_camera.rs`) disables AE entirely
(`AeEnable(false)`, comment: "Permanent AEC Bypass: Pure Manual open-loop")
and sets `ExposureTime(settings.shutter_us as i32)` directly — i.e.
`shutter_us: 0` in the request becomes a **literal** manual exposure
request of 0. The 762µs observed is presumably the sensor driver's
minimum-clamp for that mode, not anything AE chose (AE is off). This also
means: **the ~500ms/frame pacing is not AE-convergence wait time either** —
there's no AE loop running at all in this code path.

#### 3.3.2 Where the pacing most likely comes from

For still captures, `apply_controls` is invoked with `fps: None`
(`streaming.then_some(profile.preview_spec().fps)` evaluates to `None` when
`streaming = false`), which means **`controls::FrameDurationLimits` is
never set for still captures**. The live-preview path, in contrast, always
sets it explicitly.

Working hypothesis (not yet confirmed by directly inspecting the active
`FrameDurationLimits`/sensor-mode libcamera selects when none is
requested): the Raspberry Pi libcamera pipeline handler (`rpi::pisp`)
applies its own default frame duration for the `StillCapture` role — likely
tied to whichever sensor mode it selects for that role (possibly the same
full-resolution mode regardless of the requested output size, which would
also explain why Master Archive and 2K Binning show identical per-frame
timing despite very different output resolutions — worth confirming
directly, see §4).

## 4. Open Questions / Next Steps for Optimization

Ordered roughly by expected effort-to-payoff:

1. **Set an explicit fast `FrameDurationLimits` for still captures too.**
   The mechanism to do this already exists (`apply_controls`'s `fps`
   parameter, currently hardcoded to `None` for `streaming = false`). Since
   real exposure is sub-millisecond, requesting e.g. a 30–50ms frame
   duration for the warmup loop specifically could plausibly cut the
   ~5 second warmup down to a few hundred milliseconds. This is the
   highest-leverage, most concretely-supported-by-data next step.
   **Not yet tried** — needs a real on-hardware test after the change,
   same as everything else in this doc.
2. **Re-examine why `CAPTURE_WARMUP_FRAMES = 8` is needed at all.** The
   original rationale for a warmup period is almost always AE/AWB
   convergence — but AE is fully disabled in this code path (§3.3.1), and
   AWB (`AwbEnable(true)`) is the only thing still auto-converging. It's
   worth checking whether AWB actually needs 8 frames to settle, or whether
   this constant was inherited from a design that assumed AE was active.
   If frame duration is fixed first (item 1), even a full 8-frame wait at
   a fast frame duration becomes cheap, so this is lower priority than
   item 1, but combining both would compound the improvement.
3. **Confirm the sensor-mode-selection hypothesis in §3.3.2 directly**,
   rather than leaving it as inference — e.g. log the stream configuration
   libcamera actually reports for the `StillCapture` role (pixel format,
   and ideally the selected sensor mode / native frame duration range) and
   compare it against the `ViewFinder` role's. This would confirm or rule
   out "still capture always uses the full-resolution sensor mode
   regardless of requested output size."
4. **Re-measure after any change using the same methodology** (§2) —
   in particular, re-run the exact per-frame `warmup_frame` trace to
   confirm `since_previous_ms` actually drops, not just the aggregate
   total (a regression in warmup frame *count* could hide a regression in
   per-frame cost, or vice versa).
5. Independently, consider fixing the journald observability gap (§2.2) so
   future investigations don't need the stop-the-service-and-run-manually
   workaround.

## 5. Raw Trace Data (for reference)

<details>
<summary>Master Archive + DNG, single capture</summary>

```
stage="stop_existing_preview" elapsed_ms=8
stage="pipeline_start" elapsed_ms=70
stage="warmup_and_capture" frames=9 elapsed_ms=5167
stage="camera_stop" elapsed_ms=4
stage="capture_frame_total" elapsed_ms=5243
stage="jpeg_encode" bytes=7783245 elapsed_ms=121
stage="dng_decode_and_encode" bytes=24661360 elapsed_ms=73
stage="disk_write" elapsed_ms=12
stage="publish_capture" elapsed_ms=209
profile=MasterArchive save_dng=true stage="total" elapsed_ms=5461
stage="http_handler_total" elapsed_ms=5461
```

</details>

<details>
<summary>2K Binning, JPEG only, four consecutive captures</summary>

```
# Shot 1
stage="stop_existing_preview" elapsed_ms=4
stage="pipeline_start" elapsed_ms=46
stage="warmup_and_capture" frames=9 elapsed_ms=5037
stage="camera_stop" elapsed_ms=3
stage="capture_frame_total" elapsed_ms=5088
stage="jpeg_encode" bytes=527284 elapsed_ms=21
stage="disk_write" elapsed_ms=0
stage="publish_capture" elapsed_ms=21
profile=Binning2k save_dng=false stage="total" elapsed_ms=5113
stage="http_handler_total" elapsed_ms=5113

# Shot 2
stage="stop_existing_preview" elapsed_ms=0
stage="pipeline_start" elapsed_ms=45
stage="warmup_and_capture" frames=9 elapsed_ms=5037
stage="camera_stop" elapsed_ms=3
stage="capture_frame_total" elapsed_ms=5087
stage="jpeg_encode" bytes=528227 elapsed_ms=21
stage="publish_capture" elapsed_ms=21
profile=Binning2k save_dng=false stage="total" elapsed_ms=5108
stage="http_handler_total" elapsed_ms=5108

# Shot 3
stage="stop_existing_preview" elapsed_ms=4
stage="pipeline_start" elapsed_ms=47
stage="warmup_and_capture" frames=9 elapsed_ms=5542
stage="camera_stop" elapsed_ms=3
stage="capture_frame_total" elapsed_ms=5594
stage="jpeg_encode" bytes=528453 elapsed_ms=21
stage="publish_capture" elapsed_ms=21
profile=Binning2k save_dng=false stage="total" elapsed_ms=5620
stage="http_handler_total" elapsed_ms=5620

# Shot 4
stage="stop_existing_preview" elapsed_ms=0
stage="pipeline_start" elapsed_ms=46
stage="warmup_and_capture" frames=9 elapsed_ms=5037
stage="camera_stop" elapsed_ms=3
stage="capture_frame_total" elapsed_ms=5087
stage="jpeg_encode" bytes=528459 elapsed_ms=21
stage="publish_capture" elapsed_ms=21
profile=Binning2k save_dng=false stage="total" elapsed_ms=5109
stage="http_handler_total" elapsed_ms=5109
```

</details>

<details>
<summary>Per-frame exposure/timing trace, 2K Binning</summary>

```
stage="stop_existing_preview" elapsed_ms=4
stage="pipeline_start" elapsed_ms=46
stage="warmup_frame" frame_index=1  exposure_us=762 since_previous_ms=554
stage="warmup_frame" frame_index=2  exposure_us=762 since_previous_ms=504
stage="warmup_frame" frame_index=3  exposure_us=762 since_previous_ms=499
stage="warmup_frame" frame_index=4  exposure_us=762 since_previous_ms=499
stage="warmup_frame" frame_index=5  exposure_us=762 since_previous_ms=499
stage="warmup_frame" frame_index=6  exposure_us=762 since_previous_ms=499
stage="warmup_frame" frame_index=7  exposure_us=762 since_previous_ms=500
stage="warmup_frame" frame_index=8  exposure_us=762 since_previous_ms=499
stage="warmup_frame" frame_index=9  exposure_us=762 since_previous_ms=500
stage="warmup_frame" frame_index=10 exposure_us=762 since_previous_ms=499
stage="warmup_frame" frame_index=11 exposure_us=762 since_previous_ms=499
stage="warmup_and_capture" frames=9 elapsed_ms=5562
stage="camera_stop" elapsed_ms=3
stage="capture_frame_total" elapsed_ms=5613
stage="jpeg_encode" bytes=536235 elapsed_ms=21
stage="publish_capture" elapsed_ms=21
profile=Binning2k save_dng=false stage="total" elapsed_ms=5639
stage="http_handler_total" elapsed_ms=5639
```

Note: this run needed 11 frames to reach `target` (`request_count +
CAPTURE_WARMUP_FRAMES`), not the 9 the `warmup_and_capture` summary line's
hardcoded `frames=` field claims — that field is `CAPTURE_WARMUP_FRAMES + 1`
and doesn't reflect the actual `request_count`, which can vary with however
many buffers got allocated. Minor inaccuracy in the instrumentation itself,
worth fixing if this logging is kept long-term; use the count of
`warmup_frame` lines for the real number, not the summary's `frames` field.

</details>

## 6. Instrumentation Status

All logging described in §2.1 is live in `src/native_camera.rs` and
`src/web.rs` as of `optic-daemon` 0.1.15, deployed and verified on
`optic.local`. It's left in place (not behind a feature flag) since it's
cheap (`tracing::info!` calls with primitive fields) and directly useful
for verifying any future change against this document's baseline numbers.
