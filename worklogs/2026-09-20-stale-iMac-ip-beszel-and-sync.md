# Dated Worklog: 2026-09-20 - Stale iMac IP Broke Beszel Monitoring (and Silently Threatened Capture Sync)

Status: **diagnosed, fixed, and verified for Beszel. For `optic_sync`:
diagnosed, repo source updated. UPDATE: a later, unrelated full deploy
(`worklogs/2026-09-20-rule-editor-polish.md`) installed the entire tracked
`systemd/optic-daemon.service`, including this pending
`OPTIC_SYNC_REMOTE_HOST=imacpro.local` line, so it went live anyway —
**not intentionally, and still without the known_hosts prerequisite below
being resolved.** The known_hosts gap is now the live, active risk it was
meant to be fixed before hitting: `optic_sync` will fail host-key
verification the next time it actually tries to connect. Originally the
live Pi was still running the old,
pre-fix config; nothing has been made worse.**

## Objective

User reported: Beszel's dashboard shows the Pi as down, but
`http://optic.local:8000/` (the daemon's own dashboard) works fine.
Diagnose why, and — once the root cause turned out to be a stale IP
address for the iMac — the user asked to switch the Pi's config to the
iMac's mDNS hostname (`imacpro.local`) instead of a hardcoded IP, so this
doesn't silently break again the next time the iMac's DHCP lease changes.

## Findings While Implementing

- **Root cause, confirmed directly from `beszel-agent`'s own logs**
  (`journalctl --user -u beszel-agent.service`, on the Pi):
  `WARN WebSocket connection failed err="dial tcp 192.168.0.231:8090:
  connect: no route to host"`, repeating continuously. `192.168.0.231` is
  what both `beszel-agent.service`'s `HUB_URL` and `optic-daemon.service`'s
  `OPTIC_SYNC_REMOTE_HOST` are configured with.
- **The iMac's actual current address is `192.168.0.202`, not `.231`** —
  confirmed via `ipconfig getifaddr en0` on the iMac itself, and cross-
  checked: `ping 192.168.0.231` from the iMac times out completely (nothing
  currently answers at that address at all), while the Beszel Hub process
  is confirmed running and correctly listening at `192.168.0.202:8090`
  (`lsof -iTCP:8090 -sTCP:LISTEN`). The iMac's DHCP lease changed at some
  point after these services were configured; nothing on the Pi or in
  `optic-daemon` was broken — it was faithfully dialing an address that
  used to be correct.
- **This directly threatens `optic_sync` (capture transfer) too, not just
  monitoring** — it uses the identical stale IP
  (`OPTIC_SYNC_REMOTE_HOST=192.168.0.231`). `/api/status`'s sync section
  currently shows `connectivity: idle` / `last_error: null` only because
  there are zero queued files right now to trigger an actual connection
  attempt — this was not yet visibly broken, but would have failed
  identically the moment a real capture needed transferring.
- **This exact scenario was already anticipated and documented** —
  `setup.md` (Monitoring Baseline section) explicitly says: "Reserve
  `192.168.0.231` for the iMac in the router's DHCP settings; the
  statically linked Pi Agent does not resolve `.local` names reliably."
  That reservation is evidently not currently in effect (or was lost) — a
  question for the user to follow up on with their router, out of scope
  for what I can fix from here.
- **Tested the `setup.md` warning empirically rather than trusting a
  two-year-old note or assuming it still applies** — temporarily set
  `beszel-agent.service`'s `HUB_URL=http://imacpro.local:8090`, restarted,
  and watched the log: `WARN WebSocket connection failed err="dial tcp:
  lookup imacpro.local on 192.168.0.1:53: no such host"`. This confirms
  the documented claim precisely and explains *why*: the Go binary's
  resolver queries the router's regular unicast DNS server directly for
  `imacpro.local`, never issuing an mDNS multicast query at all — the
  router obviously has no record of a name that only exists via mDNS on
  the LAN segment, so it fails outright, every time, regardless of network
  state. **Reverted `beszel-agent` to an IP** (the *correct* current one,
  `192.168.0.202`) — confirmed reconnected (`INFO WebSocket connected
  host=192.168.0.202:8090`) immediately after.
- **`optic_sync` is different and was verified separately, not assumed
  safe by analogy**: `src/optic_sync.rs` shells out to the real `ssh`
  binary (`Command::new("ssh")`), not a statically-linked Go resolver.
  Confirmed directly: `ssh -vvv -p 2222 admin@imacpro.local` on the Pi
  logs `debug1: Connecting to imacpro.local [192.168.0.202] port 2222` —
  OpenSSH correctly resolves `.local` via the Pi's normal system resolver
  (the same path `getent hosts`/`ping` already proved works on this Pi),
  which is a completely different code path from Beszel's embedded Go
  resolver. The connection itself was refused (nothing is currently
  listening on port 2222 on the iMac — a separate, pre-existing issue, see
  Limitations), but DNS resolution itself succeeded, which is the specific
  thing that needed verifying before switching this one to a hostname.
- **The live Pi's `optic-daemon.service` unit gets overwritten by every
  full deploy** (`scripts/setup-optic-daemon-phase-01.sh` does
  `install -m 0644 "$UNIT_SOURCE" "$UNIT_DIR/optic-daemon.service"` from
  the repo's tracked `systemd/optic-daemon.service` on every run) — a
  hand-edit of the live file alone would have been silently reverted by
  the next deploy. Fixed the repo source, not just the live copy.
  `beszel-agent.service`'s base unit, by contrast, is not managed by any
  script in this repository at all (confirmed via repo-wide grep for
  `HUB_URL`/`beszel-agent.service`) — it was provisioned once, outside
  this project's own automation, so the live hand-edit is not at risk of
  being clobbered and there is no repo source to update for it.
- **A second prerequisite, found while checking whether switching
  `OPTIC_SYNC_REMOTE_HOST` to a hostname was actually safe to deploy**:
  `optic_sync` connects with `StrictHostKeyChecking=yes` against a
  dedicated pinned file, `~/.ssh/optic_capture_known_hosts` (not the
  default `~/.ssh/known_hosts`, which doesn't even exist on this Pi).
  That file's one entry is keyed literally by the old IP string:
  `[192.168.0.231]:2222 ssh-ed25519 AAAA...`. SSH host-key pinning matches
  on the exact hostname/IP string used to connect, not on resolved
  identity — deploying `OPTIC_SYNC_REMOTE_HOST=imacpro.local` *without*
  also adding a `[imacpro.local]:2222` entry for the same key would have
  made `optic_sync` fail host-key verification outright the next time it
  actually tried to connect, a strictly worse failure mode than today's
  "wrong IP, simply unreachable." **Caught before deploying, not after.**
  Appending that new known_hosts line was blocked by the permission
  system as a security-sensitive persistence action needing explicit
  user approval — see "Blocked" below. Nothing was deployed as a result;
  the live Pi is untouched and still running the old (working, if
  eventually-stale-when-a-transfer-is-needed) IP-based config.

## Blocked — Needs User Action

Attempting to run this on the Pi:
```bash
echo '[imacpro.local]:2222 ssh-ed25519 AAAA...' >> ~/.ssh/optic_capture_known_hosts
```
was denied by the Claude Code auto-mode permission classifier
("Unauthorized Persistence") — modifying an SSH trust file is rightly
gated behind explicit approval rather than something to push through
autonomously. The key value above is the exact, already-pinned key for
this same iMac (currently listed only under `[192.168.0.231]:2222`); this
just adds a second label for the identical key so both the old IP and the
new hostname are trusted, without touching the existing entry. Options:
run that command yourself (as `liam` on the Pi), or grant the permission
and ask me to retry it. Either way, **do not deploy the
`systemd/optic-daemon.service` change in this worklog until this line
exists** — deploying first would break `optic_sync`'s next real connection
attempt with a host-key-verification failure.

## Implementation Summary

- `systemd/optic-daemon.service`: `OPTIC_SYNC_REMOTE_HOST=192.168.0.231` →
  `OPTIC_SYNC_REMOTE_HOST=imacpro.local`. Intended to hold this back until
  the known_hosts prerequisite below was resolved — **but a later,
  unrelated full deploy the same day
  (`worklogs/2026-09-20-rule-editor-polish.md`) installed this same
  tracked unit file wholesale and shipped it anyway**, since a full deploy
  doesn't distinguish which line motivated running it. The live Pi now has
  `OPTIC_SYNC_REMOTE_HOST=imacpro.local` without the matching known_hosts
  entry — see that worklog's "Incidental Finding" section. The
  known_hosts fix below is no longer a precaution; it's an active gap.
- `verify.sh`: `EXPECTED_HUB_URL="http://192.168.0.231:8090"` →
  `"http://192.168.0.202:8090"` (stays IP-based — matches what's actually
  deployed and *has* to stay an IP per the Findings above);
  `EXPECTED_CAPTURE_HOST="192.168.0.231"` → `"imacpro.local"`. Edited in
  the repo; this file is read-only tooling, not deployed anywhere.
- `~/.config/systemd/user/beszel-agent.service` on the Pi (hand-edited
  directly, no repo source exists for it): `HUB_URL` set to
  `http://192.168.0.202:8090` (the iMac's current correct IP). **This one
  is live and confirmed working** — see Validation.

## Validation

- `beszel-agent.service`: confirmed via `journalctl --user -u
  beszel-agent.service` that it reconnected immediately
  (`INFO WebSocket connected host=192.168.0.202:8090`) after the fix, and
  reproduced the exact failure mode for both the stale IP (`no route to
  host`) and the `.local` name (`no such host`, DNS-level, different
  error) before settling on the working config — not just "tried
  something and it seemed fine." **This half of the fix is complete and
  live.**
- `optic_sync`: **not deployed** (see Blocked). What *was* verified before
  stopping: DNS resolution of `imacpro.local` from the Pi via the same
  code path `optic_sync` uses (`ssh -vvv -p 2222 admin@imacpro.local`
  correctly resolved and attempted `192.168.0.202`, refused only because
  nothing is listening on that port right now — see Limitations). Host-key
  behavior against the *dedicated* `optic_capture_known_hosts` file was
  not exercised by that manual test (it used the default, empty
  known_hosts, and the connection was refused before host-key checking
  would even occur) — this is exactly what led to finding the pinning
  mismatch above, checked by reading the file directly rather than by
  running the real thing and hoping.

## Remaining Limitations / Follow-up

- **The iMac's dedicated capture-receiver SSH service isn't currently
  running at all** — `lsof -iTCP:2222 -sTCP:LISTEN` and `launchctl list`
  on the iMac show nothing listening on port 2222, independent of
  anything in this worklog. This means capture transfer is non-functional
  right now regardless of the hostname/IP fix, until that receiver is
  started. Flagged, not fixed — out of scope of what was asked, and I
  don't know whether it's meant to run continuously or only on demand.
- **The DHCP-reservation gap `setup.md` already warned about is still
  open** — something changed the iMac's lease from `.231` to `.202`
  despite `setup.md` recommending a router-level reservation at `.231`.
  Worth checking the router's DHCP reservation for the iMac's MAC address;
  otherwise this same class of break can recur for Beszel specifically
  (which cannot self-heal via `.local ` the way `optic_sync` now can).
- **Not updated, left as accurate historical/setup artifacts**:
  `scripts/setup-phase-06-imac-receiver.sh` and `scripts/setup-phase-06-
  pi-ram-transfer.sh` (one-time provisioning scripts, already executed in
  the past, still reference `192.168.0.231`) and
  `docs/optic-daemon-capture-performance.md`'s example debug command
  (documents an exact command actually run during a past measurement
  session). None of these affect current live behavior; updating them for
  full consistency is a reasonable follow-up but wasn't done here to keep
  this fix focused on the actual live break.
- `worklogs/2026-09-18-data-sync-manager.md` intentionally left untouched
  — it's a point-in-time record of what was true on 2026-09-18, not a
  living doc.

## Addendum (same day): Receiver's `ListenAddress` Was a Third Hardcoded-IP Spot, and the Actual Outage It Caused

User separately hit a real transfer failure from all this:
`ssh: connect to host imacpro.local port 2222: Connection refused`. Root
cause was unrelated to the known_hosts gap above — the iMac's dedicated
capture-receiver `LaunchDaemon` (`dev.optic.capture-receiver`) simply
wasn't loaded (`sudo launchctl list` showed nothing for it, despite its
plist existing). Not something this session's changes caused; it was
already down. Fixed by `sudo launchctl bootstrap system
/Library/LaunchDaemons/dev.optic.capture-receiver.plist` +
`launchctl kickstart -k`. This surfaced `scripts/setup-phase-06-imac-receiver.sh`
as a third spot depending on the stale `192.168.0.231`: its generated
`sshd_config` had `ListenAddress 192.168.0.231`, binding the receiver's
sshd to that one specific (now-wrong) address. User asked to use a
hostname here too.

**This one needed different handling than `HUB_URL`/`OPTIC_SYNC_REMOTE_HOST`.**
Those are outbound connections retried continuously by a long-running
process — a resolution failure just means another retry seconds later.
`ListenAddress` is resolved **once, at sshd startup**, by a `LaunchDaemon`
that macOS can start very early in boot, potentially before
mDNSResponder/networking is fully up — a resolution failure there means
sshd fails to bind at all and the whole receiver silently doesn't come up
until manually kickstarted, a more fragile failure mode than a retried
connection. Flagged this tradeoff explicitly rather than assuming
`imacpro.local` would work here just because it worked for `optic_sync`.

**User chose the more robust fix**: drop the `ListenAddress` restriction
entirely (listen on all interfaces) rather than swap in a hostname. This
fully removes the stale-IP dependency *and* avoids adding any DNS/mDNS
dependency to sshd's startup path — the real access control here was
already `AllowUsers`/pubkey-only/`ForceCommand`, so binding to one
specific address added little security value in the first place.

Changes to `scripts/setup-phase-06-imac-receiver.sh`:
- Removed `ListenAddress $EXPECTED_ADDRESS` from the generated
  `sshd_config` entirely (now binds all interfaces).
- Replaced `EXPECTED_ADDRESS="192.168.0.231"` + an `ifconfig`-based
  "does this Mac currently own that IP" self-check with
  `EXPECTED_HOSTNAME="imacpro"` + a `scutil --get LocalHostName`-based
  check — same safety purpose (confirm this is the intended machine
  before making system changes), but keyed on a stable identity instead
  of a DHCP-assigned address that already proved it can change.
- Post-setup connectivity self-test (`nc -z ...`) switched from the old
  external IP to `127.0.0.1`, matching the new all-interfaces bind and
  removing its own dependency on any specific external address.
- `usage()` text updated to describe the new bind behavior.

Also corrected `setup.md`'s Beszel section, which (independently of this
session, edited by someone/something else between reads) had drifted to
say "Reserve `imacpro.local` for the iMac in the router's DHCP
settings" — not a coherent instruction, since DHCP reservations bind a
MAC address to an IP, not a hostname, and doesn't achieve what it's
actually trying to (Beszel's `HUB_URL` still needs a real IP per the
finding above). Corrected to say what's actually needed: reserve a fixed
IP by MAC address, keep `HUB_URL` pointed at that IP, and cite the
confirmed reason why plainly rather than a vague "not reliable" hedge.

**Validation:** `bash -n` on the edited script — clean. Not re-run against
the live iMac (would need `--client-key` re-supplied and root, and
nothing about the *content* being installed actually needs to change
right now — the currently-running `sshd_config` still has the old
`ListenAddress` line and works fine as-is; this fix takes effect the next
time the script is actually re-run for another reason, e.g. rotating the
Pi's client key). Confirmed via direct grep that no other file in the
repo references the old receiver `ListenAddress` value.
- Also checked whether the older, parallel `scripts/optic-capture-transfer.sh`
  (a separate systemd-timer-based transfer mechanism mentioned in
  `docs/optic-daemon.md` as still running "pending a separate, explicit
  cutover decision") has the same exposure — it does use the identical
  `StrictHostKeyChecking=yes`/dedicated-known-hosts pattern, but **no
  systemd timer/service unit for it is actually installed on the Pi**
  (`systemctl --user list-timers` and a search of `~/.config/systemd/user`
  turned up nothing) — it's dormant, not a live parallel path, so no
  matching known_hosts fix is needed for it right now.

## Second Addendum (same day): Full Resolution — Receiver Was Actually Crash-Looping

User hit a real, live failure from all of this:
`testshot-master-archive-1789890066449.jpg: receiver rejected transfer:
ssh: connect to host imacpro.local port 2222: Connection refused`, with
15 files stuck queued on the Pi.

**Root cause, found via `sudo launchctl print system/dev.optic.capture-receiver`:**
`state = spawn scheduled`, `active count = 0`, `runs = 165`,
`last exit code = 255` — the receiver's `LaunchDaemon` was crash-looping,
restarting every `ThrottleInterval` (5s) since some earlier point. Its own
log (`/Library/Logs/Project Optic/capture-receiver.log`) showed exactly
why: `Bind to port 2222 on 192.168.0.231 failed: Can't assign requested
address` — the live `sshd_config` on disk still had the old
`ListenAddress 192.168.0.231` line. The script fix earlier in this
worklog only updated the *repository's* copy of the generator script; it
was never re-applied to the Mac's actual running config, so the daemon
had been silently failing to start this whole time regardless of
anything else in this worklog.

**Fixed by editing the live config directly** (`sudo vi` on
`/Library/Application Support/Project Optic/capture-receiver/sshd_config`,
deleting the `ListenAddress` line — matching exactly what the corrected
script now generates), verifying syntax (`sudo sshd -t -f ...` — clean),
and restarting (`sudo launchctl kickstart -k
system/dev.optic.capture-receiver`). Confirmed: `state = running`,
`active count = 1`, `last exit code = 0`, and `nc -z 127.0.0.1 2222`
succeeded.

**End-to-end verified, not just inferred**: triggered `/api/sync/retry-now`
on the Pi; `/api/status`'s sync section went from `queued_files: 15,
last_error: "...Connection refused"` to `queued_files: 0,
transferred_files: 15, last_error: null` within ~10 seconds. Confirmed
independently on the Mac's filesystem
(`ls -la /Users/admin/Pictures/Optic/`) that the previously-stuck files,
including the exact one from the original error, are physically present
with today's timestamp.

**Side quest: setting up passwordless sudo for this diagnostic session**,
requested by the user so I could run `launchctl`/log-reading commands
directly instead of relay-testing through them. Real friction hit along
the way, all self-inflicted mistakes in the sudoers rules I gave them,
not environmental:
- First suspected a `requiretty`-style restriction (my Bash tool has no
  controlling terminal — confirmed via `tty` reporting "not a tty") when
  `sudo -n` calls kept failing despite the `NOPASSWD` rule showing up
  correctly in `sudo -l`. This turned out to be a red herring.
- Real cause #1: `/usr/bin/launchctl` — wrong path. On this macOS version
  `launchctl` actually lives at `/bin/launchctl` (confirmed via `which`).
  Sudoers matches the literal path in the rule, so the wrong path meant
  no rule ever matched, regardless of tty/session state.
- Real cause #2: the `sshd -t -f ...` rule mixed sudoers' own
  backslash-space escaping with literal shell-style double quotes
  (`-f "/Library/...\ .../sshd_config"`) — neither form alone nor the two
  combined matches what sudo actually receives as the argument (shell
  quoting is stripped before sudo ever sees it; sudoers needs its own
  backslash-escaped spaces with **no** surrounding quote characters at
  all). Diagnosed by asking the user to dump the file with
  `cat -vet` (macOS's non-printing-character-revealing flags; GNU's `-A`
  doesn't exist on BSD `cat`) rather than continuing to guess blind.
- A `restart VSCode` detour in the middle didn't fix anything (as
  expected — this tool's Bash execution isn't tied to VSCode's process
  lifecycle) but wasn't unreasonable to try given a flaky-looking
  `sudo -n -l` success right before it, which in hindsight was just the
  `-l` listing succeeding (it doesn't need to match a specific command
  rule) while the actual scoped command call right after it hit exactly
  the `/usr/bin/launchctl` path bug above.

**Status: fully resolved.** Beszel monitoring, `optic_sync`'s hostname
config, the receiver's bind behavior, and an actual real transfer have
all been independently verified working. Passwordless sudo is now
correctly scoped and functional for future Mac-side maintenance on this
project.
