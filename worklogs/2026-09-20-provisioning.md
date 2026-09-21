# Dated Worklog: 2026-09-20 - Provisioning as Code and Read-Only Root (Phase 9) Readiness

Status: **Part 1 implemented and verified read-only against the live Pi**:
the script's rules are byte-identical to the installed ones, its sudo dry run
reports `[OK]` for everything, and `verify.sh` Phases 1 and 8 pass on the Pi.
**Not applied**: the approved apply and idempotence re-run have not
happened. **Part 2 design written**, with its host facts confirmed
read-only; not yet reviewed. No host was changed.

## Objective

1. **Reproducible provisioning:** the PolicyKit rules behind the dashboard's
   Reboot, NTP sync, and timezone actions were installed by hand and are not
   in any setup script (see `worklogs/2026-09-19-reboot-nonewprivileges-fix.md`,
   `...-phase2i-...`, `...-phase2j-...`). A re-provisioned Pi would silently
   lose those three buttons. Capture them in an idempotent setup script and add
   checks to `verify.sh`.
2. **Phase 9 readiness:** `setup.md` §9 (OverlayFS read-only root) was never
   enabled. `docs/optic-daemon-capture-log.md` §4 records that enabling it
   would stop `history.db` writes (Option A). Design Option B (a dedicated
   writable partition or bind mount for `~/.local/state` and whatever
   else must stay writable, such as scheduler config) and a safe
   enable/rollback runbook.

## Hand-Installed Host Configuration Inventory

Sources: the worklogs named below, plus `setup.md` and every
`scripts/setup-*.sh`. "Scripted" means an existing setup script already
owns it. Contents marked *(from worklog)* were not yet captured from the
live Pi (see Validation).

| # | Item | Installed by | Daemon depends on it? | Owner after this change |
|---|------|--------------|-----------------------|-------------------------|
| 1 | `/etc/polkit-1/rules.d/60-optic-daemon-reboot.rules`: `liam` → `org.freedesktop.login1.reboot` | hand, `2026-09-19-reboot-nonewprivileges-fix.md` Round 2 (exact text quoted there) | **Yes**: dashboard *Reboot Pi* | new script |
| 2 | `/etc/polkit-1/rules.d/61-optic-daemon-ntp-sync.rules`: `liam` → `org.freedesktop.systemd1.manage-units`, only `unit == systemd-timesyncd.service` and `verb == restart` | hand, `...-phase2i-...` (described, text not quoted) | **Yes**: Config page *Sync now* | new script |
| 3 | `/etc/polkit-1/rules.d/62-optic-daemon-set-timezone.rules`: `liam` → `org.freedesktop.timedate1.set-timezone` | hand, `...-phase2j-...` (described, text not quoted) | **Yes**: Station save sets the system timezone | new script |
| 4 | `Linger=yes` for `liam` (`/var/lib/systemd/linger/liam`) | hand ("administrator-provisioned", `setup.md` §8F) | **Yes**: the user service starts at boot without a login | new script |
| 5 | `liam` in `video` and `render` groups | Pi OS first-boot user setup; `setup.md` §8B asserts it; `setup-optic-daemon-phase-01.sh` refuses to install without it | **Yes**: camera device access | new script (adds only if missing) |
| 6 | `/etc/sudoers.d/liam-nopasswd` (`NOPASSWD: ALL`) | hand, `2026-09-18-system-control-panel.md` (for Wi-Fi debugging) | **No**: the daemon no longer calls `sudo` (reboot fix Round 2). Setup scripts self-elevate with interactive `sudo`, which does not need it | **not provisioned** (see Decisions) |
| 7 | `/etc/systemd/system.conf.d/90-watchdog-headroom.conf` (`RuntimeWatchdogSec=90s`) | hand, `2026-09-18-system-control-panel.md` | No (it protects on-Pi builds) | **not provisioned; conflicts with Phase 3** (see Drift) |
| 8 | journald `Storage=persistent`, 16M caps | `setup-phase-01-journaling.sh` (updated `2026-09-19-persistent-journal-crash-evidence.md`) | No (diagnostics only) | already scripted; `verify.sh` Phase 1 was stale (see Drift) |
| 9 | `~/.local/state/optic-daemon` (0700) | `setup-optic-daemon-phase-01.sh` | Yes | already scripted |
| 10 | `/mnt/capture` tmpfs, `/etc/optic/capture-transfer.conf`, SSH key/known_hosts, transfer units | `setup-phase-06-pi-ram-transfer.sh` | Yes | already scripted |
| 11 | `~/.config/systemd/user/optic-daemon.service`, `~/.local/bin/{optic-daemon,web/}` | `setup-optic-daemon-phase-01.sh` / deploy script | Yes | already scripted |

No user-unit drop-ins for `optic-daemon.service` are recorded in any
worklog; the live check below looks for them anyway.

### Drift found between scripts, `verify.sh`, and the host (from worklogs)

- **Phase 1 journal:** `setup-phase-01-journaling.sh` now installs
  `Storage=persistent`, `SystemMaxUse=16M`, `RuntimeMaxUse=16M`, but
  `verify.sh` Phase 1 still requires `volatile` and `32M` and fails when
  persistent journal files exist. On the live Pi that is three FAILs caused
  by a stale checker, not by the host. Fixed in `verify.sh` (the setup
  script is the source of truth, and its worklog records the change as
  user-approved).
- **Phase 3 watchdog:** `setup-phase-03-watchdog.sh` and `verify.sh`
  require `RuntimeWatchdogSec=15s`. The hand-installed `90-watchdog-headroom.conf`
  sorts after `90-optic-watchdog.conf`, so the effective value is `90s`.
  `verify.sh` Phase 3 therefore fails on the live Pi. Re-running the Phase 3
  script fails its own post-check (it asserts 15s after `daemon-reexec`)
  without removing the override. **User decision (2026-09-20): 15s stays
  the plan.** Scripts and `verify.sh` are unchanged. Removing
  `90-watchdog-headroom.conf` from the Pi is a later host change that needs
  its own approval; until then Phase 3 FAILs on the live Pi, and that is
  expected.

## Acceptance Criteria

1. `scripts/setup-phase-08-daemon-host-access.sh` installs items 1–5 above,
   is idempotent (a second run reports nothing to change and writes
   nothing), and backs up any file it replaces.
2. The script's `--dry-run` reports, without writing, whether each rule on
   the host is byte-identical to the script's copy and whether its mode or
   ownership differ. Run against the live Pi it reports `[OK]` for all
   three rules (the byte-identical check). Any difference is resolved by
   making the script match the live file, not the other way round.
3. `verify.sh` Phase 8 reports each rule present with `root:root 0644`,
   checks behaviour with `pkcheck` (each action authorized for `liam`, and
   the NTP rule still **denies** a different unit), and checks linger.
   These checks are read-only. *Revised during implementation:* the reboot
   and timezone checks need no `sudo`, but the NTP-scope and file-mode
   checks need root (see Validation), so they use `sudo -n` and WARN when
   it is unavailable.
4. `verify.sh` Phase 1 matches the scripted persistent-journal config.
5. `docs/phase9-readonly-root.md` lists every runtime write path, says
   which would break under OverlayFS, and gives Option B, deploys under a
   read-only root, a staged enable procedure, rollback, and verify.sh
   checks. OverlayFS is **not** enabled.

## Test Plan (written before implementation)

Local (Mac, no host access needed):

- `bash -n` on the new script and `verify.sh` with macOS bash 3.2 and with
  Debian 13 bash 5.2 (the `vmpi-builder` VM, matching the Pi's Debian
  release). `shellcheck` is not installed on the Mac or the VM; record it
  as unavailable rather than installing it on shared machines.
- `--help` and argument errors (exit 64) run anywhere.
- `--print-rules DIR` writes the three rule files into a scratch directory
  as a non-root user, so their bytes can be diffed against copies fetched
  from the Pi without writing to the Pi.
- `--dry-run` inside the VM (no `liam`, no rules): expected to report
  `[CHANGE]` for every rule and linger, and to exit 0 without writing.

Read-only against the live Pi (allowed without asking):

- Capture `ls -la`/`stat` of `/etc/polkit-1/rules.d`, each rule's bytes
  (`sudo -n cat` if the directory is not world-readable; read only), and
  `sha256sum`.
- `diff` fetched live rules against `--print-rules` output: expect no
  difference. Any difference means the script is changed to match.
- `ssh liam@optic.local 'bash -s -- --dry-run' < scripts/setup-phase-08-daemon-host-access.sh`:
  expect `[OK]` for everything.
- `ssh liam@optic.local 'bash -s -- --phase 8 --no-color' < verify.sh` and
  `--phase 1`: expect the new checks to PASS.
- `pkaction --verbose` for the three actions, and a check for unexpected
  user-unit drop-ins (`systemctl --user cat optic-daemon.service`).

Needs explicit user approval (host write):

- A real run of the script on the Pi, then a second run to prove
  idempotence (no `Installed` lines, no new backups), then `verify.sh`.
  Because the rules are already present and byte-identical, the expected
  write set is empty. `polkitd` reloads rules on its own; no restarts.

Failure cases the script must handle:

- Not root (and no `sudo`): exit 77. Missing `liam`, missing `polkitd`
  (`pkaction` absent), missing `video`/`render` groups: exit 69, nothing
  written.
- Existing rule differs: back it up to `/var/backups/optic-hardening/`
  before replacing it.
- Post-apply `pkcheck` does not authorize an action, or authorizes
  `ssh.service` restart: exit 1 and report it.

## Decisions

- **Sudoers grant not provisioned.** It is not a daemon dependency, and
  putting a blanket `NOPASSWD: ALL` into a reusable script would grant it
  on every future Pi by default. It stays hand-managed as the user decided
  in `2026-09-18-system-control-panel.md`. Listed above so a rebuild
  knows it existed.
- **Watchdog override not provisioned.** It contradicts the Phase 3
  script. The user chose to keep 15s (2026-09-20). Note the trade-off from
  `2026-09-18-system-control-panel.md`: with 15s, heavy native builds on the
  Pi can trip the watchdog again.

## Implementation Summary

- `scripts/setup-phase-08-daemon-host-access.sh` (new, 0755): follows the
  `setup-phase-03` pattern (`--dry-run`, self-elevation with preserved
  arguments, backup to `/var/backups/optic-hardening/`, temp file + rename,
  post-apply assertions). It manages the three rule files (bytes must match
  exactly, `root:root 0644`), linger (`loginctl enable-linger` only if
  `/var/lib/systemd/linger/liam` is missing), and `video`/`render`
  (`usermod -a -G` only if missing). It never touches the `rules.d`
  directory's own owner or mode, and restarts nothing. After applying, it
  checks each action with `pkcheck` as root against a `liam`-owned subject
  process, and fails if `ssh.service` restart is authorized or if the check
  errors. `--print-rules DIR` writes the rules for off-host diffing.
  The rule text, comment headers included, is copied from the live Pi's
  files (see Live Validation). The first draft, rebuilt from the worklogs,
  had identical rule logic but lacked the headers.
- `verify.sh` Phase 1: now checks `Storage=persistent`,
  `SystemMaxUse=16M`, `RuntimeMaxUse=16M`, and that
  `/var/log/journal/<machine-id>/system.journal` exists (was: volatile,
  32M, no persistent files).
- `verify.sh` Phase 8: linger; `pkcheck` for reboot and set-timezone
  (as `liam`, no sudo); with root or `sudo -n`: NTP rule authorized, still
  denies `ssh.service` restart (only exit 1/2 counts as denied), and each
  rule file is `644 root:root`. Without root it WARNs instead. The block
  runs only when Phase 8 is selected.
- `setup.md`: new §8G (rules table, commands, what is intentionally not
  provisioned); §9 warning pointing to the new runbook.
- `docs/phase9-readonly-root.md` (new): overlay behaviour, write
  inventory, Option B, deploys, staged enable, rollback, verify.sh checks.

## Validation

Environment: macOS dev Mac (bash 3.2.57), `vmpi-builder` Lima VM (Debian
13.7, bash 5.2.37, polkitd 126-2, used read-only; nothing was installed or
written there except a temporary copy of the script in `/tmp`, removed).

| Check | Result |
|-------|--------|
| `bash -n` new script and `verify.sh`, bash 3.2 and 5.2 | pass (4/4) |
| `shellcheck` | **unavailable** (not installed on Mac or VM; not installed) |
| `--help` / unknown arg / `--print-rules` without value | exit 0 / 64 / 64 |
| `--print-rules DIR` on Mac | 3 files written; `node --check` passes on each (valid JS) |
| sha256 of managed rules | 60 `4d2e252d…d425`, 61 `1fba082d…a5`, 62 `20b595a7…586e` |
| VM `--dry-run` as `lima` | `[MISSING] user liam`, `[UNKNOWN] rules.d not readable…`, linger/groups `[CHANGE]`; exit 0 |
| VM `sudo --dry-run` | `[CHANGE] Install` ×3; `rules.d` still empty afterwards (`sudo ls -la`) |
| VM apply as non-root from a file | re-executed through sudo with args, then exit 69 "User liam does not exist; no changes were made" |
| VM apply from stdin (`bash -s`) | exit 77 with the "copy it to the Pi first" message |
| VM `verify.sh --phase 1` | no shell errors; FAILs are real (VM journald has no Optic drop-in) |
| VM `verify.sh --phase 8` | no shell errors; `liam` missing → linger FAIL + "rules were not checked" FAIL |
| VM Phase 8 copy with `daemon_user=lima`, as `lima` and as root | both paths ran: 3 authorization FAILs (no rules, correct), ssh scope PASS, 3 rule-missing FAILs; no leftover `sleep` processes |

Found and fixed during testing:

1. The first draft started the helper process even when another phase was
   selected (a `setpriv` error showed up in `--phase 1` output). Now gated on
   `SECTION_ENABLED`.
2. `setpriv --reuid=<name>`: switched to numeric uid/gid.
3. Race: `pkcheck` could see the helper before it dropped root. Both files
   now wait until `/proc/<pid>` is owned by the target uid.
4. The ssh-denial check treated a `pkcheck` error (exit 127) as "denied".
   Now only exit 1/2 count as a denial.

Also confirmed in the VM, and designed around:
`/etc/polkit-1/rules.d` is `root:polkitd 0750` on Debian 13, and polkit
refuses `--detail` from a non-root caller ("Only trusted callers … can use
CheckAuthorization() and pass details").

First attempt: `ssh liam@optic.local` → "Could not resolve hostname" (four
attempts, last at 22:17 PDT). ARP showed a Raspberry Pi MAC at
192.168.0.197, but its ED25519 host key matched none of the saved
`optic.local` keys, so I did not connect. The user then brought the Pi back
online (`optic`, up 8 minutes at first contact).

### Live Validation (read-only, 2026-09-20)

`optic.local` resolves to 192.168.0.195 and to an IPv6 link-local address.
Connections over the IPv6 address failed intermittently ("connect to host
optic.local port 22: Undefined error: 0"), so the live checks used `ssh -4`,
still verified against the saved `optic.local` host key.

| Check | Result |
|-------|--------|
| `sudo -n stat /etc/polkit-1/rules.d` | `750 root:polkitd`; `polkitd 126-2` (same as the VM) |
| Fetch rules: `sudo -n tar -C /etc/polkit-1/rules.d -cf - 6*…` | 60: 384 B, 61: 573 B, 62: 383 B, all `root:root 0644` |
| `diff` against the first-draft `--print-rules` | only differences: 3–4 line `//` comment headers at the top of each live file; rule logic identical, including the reconstructed 61 and 62 |
| Script updated with the headers; `diff -r` again | **no differences** (sha256 60 `cbaf0732…8189`, 61 `0d4dab5a…15d8`, 62 `2358dd11…c9c`) |
| `bash -s -- --dry-run` as `liam` | `[UNKNOWN]` rules (directory unreadable, as designed); linger, video, render `[OK]`; exit 0 |
| `sudo -n bash -s -- --dry-run` | `[OK]` ×3 rules, linger, video, render; exit 0 |
| `verify.sh --phase 1` | PASS=6 WARN=0 FAIL=0 (was 3 FAILs with the old checks) |
| `verify.sh --phase 8` | PASS=13 WARN=0 FAIL=0 (all new checks PASS, incl. ssh.service still denied) |
| `systemctl --user cat optic-daemon.service` | only the unit file itself; no drop-ins |

Runbook facts read on the Pi (recorded in `docs/phase9-readonly-root.md`):
the raspi-config overlay functions (`overlayroot=tmpfs` prepended to
`cmdline.txt`; `overlayroot` package not installed yet; `enable_bootro`
/`disable_bootro` refuse while the overlay is active, which changed the
Stage 4 and rollback ordering), `auto_initramfs=1`, the beszel-agent
`DATA_DIR`/key paths, timesyncd's `StateDirectory=` and ordering, build-cache
sizes (`~/.cache` 8.2G, `~/.local/src` 2.3G), the state directory (372K,
106 rows, average `detail_json` 753 B), and the disk (238 GB SD card, root 7%
used).

## Remaining Limitations and Next Steps

1. ~~Live read-only comparison~~: done, see Live Validation.
2. **Approved apply + second run** on the Pi to prove idempotence.
   Expected write set: empty, since the dry run already reports `[OK]` for
   everything. Not run: it writes to the host and needs explicit approval.
3. **Phase 3 watchdog:** 15s decided. Removing the Pi's 90s override
   (and accepting the build-time watchdog risk) is a separate, approved host
   change; Phase 3 FAILs on the live Pi until then.
4. `verify.sh` Phase 8 uses `sudo -n`, which writes an auth entry to the
   journal. Settings are unchanged, but it is not strictly write-free.
5. `README.md` still calls OverlayFS part of the current architecture, and
   `docs/optic-daemon-capture-log.md` §4 predicts loud failures, which is
   wrong for a RAM-upper overlay. Both are outside this track's files. They
   are flagged in the runbook and should be updated when the Phase 9 plan is
   approved.
6. `CLAUDE.md` imports `@AGENTS.md`, which does not exist in this worktree,
   so there was no tooling-constraints list to consult or append to.
7. Phase 9 open decisions: runbook §3.3.

## Files Changed

- `scripts/setup-phase-08-daemon-host-access.sh` (new)
- `verify.sh` (Phase 1 journal checks; Phase 8 host-access checks)
- `setup.md` (§8G new; §9 warning)
- `docs/phase9-readonly-root.md` (new)
- `worklogs/2026-09-20-provisioning.md` (this file)

## Parallel-Session Coordination (applies to all four tracks)

This track runs in its own git worktree alongside three others
(`feat/capture-latency`, `feat/health-alerts`, `feat/provisioning`,
`feat/timelapse-builder`), all branched from `main` at `a6e1509`.

- **Do not bump `Cargo.toml` `version` on this branch.** The bump happens
  once, at merge time into `main`, immediately before deploy.
- **The Pi and camera are a single shared resource.** Ask the user before any
  deploy, service restart, reboot, or on-hardware test, and do not assume
  another session is not using it.
- **Stay inside the files this track owns (below).** If a change outside them
  becomes necessary, record why here and tell the user; it is a merge-conflict
  risk with the other tracks.
- Do not commit or push without explicit user approval (`CLAUDE.md`).
- Merge order is decided by the user; after each merge the other branches
  rebase onto `main`.

## Files This Track Owns

`scripts/`, `systemd/` (only if needed), `setup.md`, `verify.sh`, new
`docs/` runbook. No `src/` changes expected.
