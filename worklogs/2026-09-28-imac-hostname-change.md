# Dated Worklog: 2026-09-28 - Capture Sync Broke When the Mac Was Renamed (`imacpro` → `imac`)

Status: **repo changes implemented and locally checked**; Pi `known_hosts` pin
and deploy pending user approval.

## Objective

`optic_sync` stopped transferring. `/api/status` reported:

```
connectivity: backoff, queued_files: 2, queued_bytes: 6127124,
last_error: scheduler-master-archive-everyhour-1790625601924.jpg:
  receiver rejected transfer: ssh: Could not resolve hostname
  imacpro.local: Name or service not known
```

## Diagnosis (observed, 2026-09-28)

- On the Pi: `getent hosts imacpro.local` fails; `getent hosts imac.local`
  returns `192.168.0.231`. avahi-daemon is enabled and active, and
  `nsswitch.conf` still has `mdns4_minimal`, so mDNS itself is healthy.
- On the Mac: `scutil --get LocalHostName` → `imac` (ComputerName and
  HostName too). The Mac was renamed, so `imacpro.local` no longer exists.
- The receiver is up: `192.168.0.231:2222` accepts connections.
- The pinned host key for `[imacpro.local]:2222` **matches** the live key at
  `imac.local:2222`, so this is only a name change, not a new machine or a
  re-keyed receiver.
- `/mnt/capture` held the 2 queued files and was 3% full; nothing lost, but
  the queue is RAM-backed, so a power loss would have dropped them.

## Approach

Move every reference to `imac.local` (user decision: keep the new name).

## Acceptance Criteria

1. No `imacpro` references remain outside historical worklogs.
2. The Pi's `known_hosts` pins `[imac.local]:2222` with the same key.
3. After deploy, `/api/status` shows sync `connectivity` out of backoff,
   `queued_files: 0` and `last_error: null`, and the queued frames appear on
   the Mac.
4. `verify.sh --phase 6` passes with the new host.

## Test Plan (before implementation)

1. `bash -n verify.sh scripts/setup-phase-06-*.sh`.
2. `rg -c imacpro` outside `worklogs/` returns nothing.
3. `cargo test --locked` (no Rust change; guards against accidental edits).
4. On the Pi, after the pin and deploy: `ssh` dry-run to the new name, then
   `POST /api/sync/retry-now` and watch the queue drain.
5. `ssh liam@optic.local 'bash -s -- --phase 6' < verify.sh` → no FAIL.

## Implementation Summary

- `systemd/optic-daemon.service`: `OPTIC_SYNC_REMOTE_HOST=imac.local`.
- `verify.sh`: `EXPECTED_CAPTURE_HOST="imac.local"` (and the comment).
- `scripts/setup-phase-06-pi-ram-transfer.sh`: `REMOTE_HOST="imac.local"`.
- `scripts/setup-phase-06-imac-receiver.sh`: `EXPECTED_HOSTNAME="imac"`.
- `setup.md`, `docs/optic-daemon-capture-performance.md`,
  `docs/pi-services-audit.md`: references updated; `setup.md` §6 gains a note
  that the receiver's name is the Mac's `LocalHostName`, what breaks when it
  changes, and how to check it.
- `Cargo.toml`/`Cargo.lock`: `0.1.33` → `0.1.34` (the unit file changes, so
  this needs a full deploy, not `--assets`).

## Remaining Limitations / Follow-up

- Beszel's `HUB_URL` still points at `192.168.0.202`, but the Mac is now
  `192.168.0.231` and the hub is not running at all. The Pi's agent has been
  retrying every 10 s and filling the journal. Out of scope here; needs its
  own fix (and the DHCP reservation `setup.md` already recommends).
- The underlying fragility remains: the station depends on an mDNS name that
  changes whenever the Mac is renamed. A DHCP-reserved IP, or the Tailscale
  name once the Pi joins the tailnet, would end this class of outage.
