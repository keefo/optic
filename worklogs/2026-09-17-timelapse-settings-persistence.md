# Persistent Timelapse Settings Worklog

**Date:** 2026-09-17  
**Objective:** Store and persist the timelapse capture profile, camera orientation transforms, denoise settings, analogue gain, and shutter settings so they survive daemon reboots and browser refreshes natively on the Raspberry Pi 5.

## DSLR-Class Staged Sandbox Configuration Design

To protect active, scheduled timelapse captures from accidental real-time browser mutations, we implement a **Staged Sandbox Configuration Environment**:

1. **Production Config:** `/mnt/capture/config.json` holds the active, validated timelapse rules.
2. **Preview Config:** `/mnt/capture/preview_config.json` holds temporary UI modifications currently being calibrated on the live viewfinder.
3. **Save (Commit):** Overwrites `config.json` from `preview_config.json`.
4. **Discard:** Reverts `preview_config.json` back to `config.json`.
5. **Teardown Cleanup:** Safely deletes `preview_config.json` once the web browser session closes (preview stops).

## Acceptance Criteria

1. On boot, status API queries read from `preview_config.json` if present; otherwise, they check `config.json` or fall back to defaults, creating a temporary workspace.
2. Tweak commands (`/api/stream/reconfigure`) strictly write to `preview_config.json`.
3. Provide endpoints `/api/config/commit` and `/api/config/discard` to apply or roll back changes.
4. Delete `preview_config.json` cleanly under `/api/stream/stop`.

## Test Plan

- **Test A:** Verify compilation passes natively on Pi.
- **Test B:** Change EV/Gain, verify `/mnt/capture/preview_config.json` is mutated, but `/mnt/capture/config.json` remains untouched.
- **Test C:** Click "Save" and verify file promotion (`preview_config.json` overwritten onto `config.json`).
- **Test D:** Click "Discard" or close the page and check directory cleanup.

