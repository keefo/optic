# Dated Worklog: 2026-09-19 - Small Persistent Journal for Crash/Hang Evidence

Status: **implemented and verified on real hardware**, with one caveat: a
full reboot (the strongest possible proof of cross-reboot survival) was not
performed as part of this change, since CLAUDE.md gates reboots on explicit
user approval and one wasn't sought for a dedicated verification reboot.
Everything short of that was verified directly (see Validation).

## Objective

The user's Pi became unresponsive and required a manual reboot today. While
diagnosing it, `journalctl --list-boots` showed only the current boot — every
previous boot's logs were gone. Root cause: `scripts/setup-phase-01-journaling.sh`
installs `/etc/systemd/journald.conf.d/60-optic-volatile.conf` with
`Storage=volatile` (RAM-only, capped at 32 MiB), deliberately chosen to
reduce SD card write wear. This is also a previously-known, explicitly
flagged gap — `docs/optic-daemon-capture-performance.md` §2.2 documented
that `journalctl` returns nothing for this daemon and called fixing it
"worth fixing independently... but out of scope" at the time.

Net effect: any freeze, crash, or unexpected reboot leaves **zero** log
evidence once the box restarts. The user asked for this fixed so a future
incident is diagnosable, explicitly accepting a small increase in SD writes
in exchange.

This is a logging/config change only — not a fix for whatever caused
today's hang itself (which remains unconfirmed; see chat: the user
disconnected the camera ribbon cable while investigating, which is a
plausible but unproven mechanism for a hang, since live CSI/I2C
disconnection is not hot-pluggable and can wedge a kernel thread).

## Acceptance Criteria

- A crash/freeze/power-cycle leaves at least some pre-incident log lines
  queryable via `journalctl --list-boots` / `journalctl -b -1` after the
  next reboot.
- The persistent journal footprint on the SD card stays small and bounded
  (target: capped at 16 MiB), not unbounded growth.
- No change to log verbosity, `RUST_LOG`, or the optic-daemon service
  itself — this is a journald storage-location change only.
- The existing idempotent setup script remains the single source of truth
  for this host configuration (edit and re-run it, don't hand-edit the
  drop-in on the Pi).

## Test Plan (written before implementation)

1. Update `scripts/setup-phase-01-journaling.sh`'s desired config from
   `Storage=volatile` to `Storage=persistent` with `SystemMaxUse=16M` and
   `RuntimeMaxUse=16M` (bounds the /run fallback tier too), and update the
   script's own post-apply verification (`journal_storage`/`journal_limit`
   checks) to match.
2. Run the script's `--dry-run` mode first on the Pi to confirm it reports
   the expected change.
3. Run it for real (it self-elevates via `sudo`), which restarts
   `systemd-journald`.
4. Verify effective config via `systemd-analyze cat-config systemd/journald.conf`
   shows `Storage=persistent`, `SystemMaxUse=16M`, `RuntimeMaxUse=16M`.
5. Verify `/var/log/journal/<machine-id>/` actually contains journal files
   after the restart (not just an empty directory, which was the pre-fix
   state despite the directory already existing).
6. Verify `journalctl --user -u optic-daemon.service --since -2min` now
   returns real lines (closes the exact gap `optic-daemon-capture-performance.md`
   §2.2 documented).
7. Reboot is the only way to fully prove cross-reboot survival; given
   CLAUDE.md's reboot-approval gate, ask the user before doing a
   verification reboot rather than assuming it's covered by the earlier
   journald-change approval.

## Implementation Summary

- `scripts/setup-phase-01-journaling.sh`: `desired_content()` changed from
  `Storage=volatile` / `RuntimeMaxUse=32M` to `Storage=persistent`,
  `SystemMaxUse=16M`, `RuntimeMaxUse=16M`. Post-apply verification updated
  to match, and **strengthened**: it previously only checked the
  `systemd-analyze cat-config` text output; it now also asserts
  `/var/log/journal/<machine-id>/system.journal` actually exists, because
  (see Findings) a config check alone proved insufficient to catch a real
  gap in this exact change.
- Added an explicit `journalctl --flush` call between the `systemctl
  restart systemd-journald.service` and the verification step, with a
  comment explaining why (see Findings). Kept the existing drop-in
  filename (`60-optic-volatile.conf`) unchanged — renaming it risked a
  worse bug (see Findings) and CLAUDE.md's change principles call for
  focused changes over unrelated renames.
- Did not touch `optic-daemon.service`, `RUST_LOG`, or any Rust code —
  this is a host-level journald storage change only.

## Findings While Implementing

- **A vendor-shipped drop-in was already fighting the one this project
  installs.** `systemd-analyze cat-config systemd/journald.conf` revealed
  `/usr/lib/systemd/journald.conf.d/40-rpi-volatile-storage.conf` —
  Raspberry Pi OS's own default `Storage=volatile`, shipped independently
  of this project's `60-optic-volatile.conf`. Filename sort order (`40-`
  before `60-`) means this project's file still wins for the `Storage=`
  key (last value read wins), so the intended config **is** effective —
  but this explains why a from-scratch Pi image would already be
  volatile-only even without this project's own script, and why simply
  deleting this project's drop-in would not restore persistent logging
  (the vendor default is volatile too).
- **Restarting `systemd-journald` does not migrate an already-buffered
  runtime journal into persistent storage.** After applying the new config
  and restarting the service, `/var/log/journal/<machine-id>/` remained
  empty and `journalctl --header` still reported the active file as
  `/run/log/journal/...` — i.e., config said `persistent` but nothing was
  actually being written there. This migration normally happens once,
  automatically, via `systemd-journal-flush.service` at boot; it is not
  re-triggered by a live service restart. Root-caused by manually running
  `journalctl --flush` and confirming `/var/log/journal/<machine-id>/system.journal`
  appeared immediately afterward. This means the script's original
  post-apply check (comparing `systemd-analyze cat-config` text only)
  would have reported success while the change had **no actual effect**
  until the next full reboot — caught only because this worklog's test
  plan required checking for real journal files, not just config text, per
  CLAUDE.md's "never infer a pass from source inspection alone." Fixed by
  adding the explicit flush to the script and hardening its verification
  to check for the real file.
- `systemd-journal-flush.service` is a `static` unit (confirmed via
  `systemctl is-enabled`), meaning it's pulled in automatically by boot
  ordering rather than needing to be manually enabled — so a normal future
  reboot will perform this same migration on its own; today's manual
  `--flush` was only needed because the config changed on a live system
  outside of a boot.
- **`journalctl --user -u optic-daemon.service` still returns nothing**,
  even after the fix — but the same log lines ARE present and now
  persistent, queryable via plain `journalctl -u user@1000.service` (the
  system-scoped view of the user manager) or an unscoped `journalctl |
  grep optic-daemon`. This is the exact `--user`-invocation quirk already
  documented in `docs/optic-daemon-capture-performance.md` §2.2 as a
  separate, pre-existing issue — unrelated to Storage=volatile vs
  persistent, and out of scope for tonight. The underlying objective
  (evidence survives a reboot) is met via the working query form.

## Validation

- `bash -n scripts/setup-phase-01-journaling.sh` — syntax check, clean.
- `--dry-run` against the live Pi reported exactly the expected change
  before anything was modified.
- Applied for real (self-elevated via `sudo`): old drop-in backed up to
  `/var/backups/optic-hardening/60-optic-volatile.conf.<timestamp>.bak`,
  new content installed, `systemd-journald` restarted, script's own
  post-apply check passed.
- Found the flush gap (above) by checking `/var/log/journal/<machine-id>/`
  directly rather than trusting the config-text check alone; fixed the
  script; **re-ran the updated script end-to-end a second time** to prove
  idempotency and that the new flush + file-existence check actually
  catches success correctly — passed cleanly, `system.journal` and
  `user-1000.journal` both present under `/var/log/journal/<machine-id>/`.
- Confirmed `journalctl --list-boots` still shows only the current boot
  (expected — no reboot has happened since applying the fix), but
  confirmed the actual boot-start log lines from earlier today (20:36:20,
  before any of tonight's changes) are now durably stored on the SD card
  and queryable (`journalctl -u user@1000.service` / grep), not just
  in-memory.
- `journalctl --disk-usage` after the fix: ~4.9M — small, well under the
  16M cap.
- **Not performed:** an actual reboot to prove survival end-to-end. The
  static `systemd-journal-flush.service` mechanism explains why this
  should work automatically, and the manual `--flush` proved the
  persistent files are real and being written to — but this is inference
  from how the mechanism works, not a direct observation of a reboot, per
  CLAUDE.md's distinction between "tested" and "hardware-validated." Ask
  before doing a dedicated verification reboot.

## Remaining Limitations / Follow-up

- Does not explain or fix whatever caused today's original hang — only
  ensures the *next* one leaves evidence.
- `journalctl --user -u optic-daemon.service` remains broken as a query
  form (pre-existing, documented separately); use the system-scoped query
  instead until/unless that's separately investigated.
- Cross-reboot survival is inferred from the `systemd-journal-flush.service`
  boot-time mechanism, not directly observed via an actual reboot in this
  session.
- `docs/optic-daemon-capture-performance.md` §2.2 updated to reflect this
  fix and the remaining `--user` quirk (see that file).
