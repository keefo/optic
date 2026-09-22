# Dated Worklog: 2026-09-21 - Shut Down Pi Button

Status: **deployed (0.1.30, full) and host-access verified on the Pi; the real shutdown is not yet run (awaiting the user's test)**

## Objective

User request: add a **Shut down Pi** button beside **Reboot Pi** in the
dashboard footer. The footer is shared (`src/web/footer.js`) and appears on
every page, so the button appears on all four.

## Acceptance Criteria

- Every page footer shows **Shut down Pi** right after **Reboot Pi**, styled
  as a destructive action like Reboot.
- Clicking it asks for confirmation. The prompt says the Pi will not come
  back by itself: someone must press its power button or unplug and replug
  it. Cancel sends nothing.
- On confirm, `POST /api/system/shutdown` runs `systemctl poweroff` over
  D-Bus (no sudo), the same way Reboot works. A failure is shown in the
  footer summary, not swallowed.
- PolicyKit allows `liam` exactly `org.freedesktop.login1.power-off`, through
  a new `63-optic-daemon-power-off.rules` managed by
  `scripts/setup-phase-08-daemon-host-access.sh`. `verify.sh --phase 8`
  checks it.

## Design

- `src/system_status.rs`: `power_off_host()`, a copy of `reboot_host()` with
  `poweroff`.
- `src/web.rs`: route `/api/system/shutdown` → `system_shutdown`.
- `src/web/footer.js` + the four HTML pages: `#footer-shutdown` button.
- Phase 8 script: new rule, `pkcheck` post-check; `verify.sh`: rule decision
  and file-mode checks.

## Test Plan

- T1 `cargo fmt --check`, `cargo clippy --locked --all-targets`,
  `cargo test --locked` (Mac). New unit test: every page carries the
  `footer-shutdown` button after `footer-reboot`, and `footer.js` posts to
  `/api/system/shutdown`. No test calls the handler: on a Linux CI runner
  it would try to power the runner off.
- T2 Biome lint of `src/web/footer.js` and the HTML (same as the deploy
  script).
- T3 `bash -n` on the changed scripts; `--print-rules DIR` writes the new
  rule and it is valid rule JS.
- T4 on the Pi (needs user approval: rule install + deploy + a real
  shutdown and a physical power-on): the Phase 8 script reports the new rule
  authorized; `verify.sh --phase 8` passes; the button shuts the Pi down
  (the dashboard goes offline, the green LED goes off) and after power-on the
  daemon comes back.
- Failure case: without the rule, the button shows `System action failed:`
  with systemctl's error, and the Pi stays up. Covered on the Pi if the user
  wants to check it before installing the rule; otherwise by code review
  (same error path as Reboot).

## Implementation

| File | Change |
|------|--------|
| `src/system_status.rs` | `power_off_host()`: `systemctl poweroff`, error on non-zero exit |
| `src/web.rs` | `POST /api/system/shutdown` → `system_shutdown`; test `every_page_footer_has_shutdown_beside_reboot` |
| `src/web/footer.js` | `#footer-shutdown` click → confirm → `footerSystemAction` (existing error path) |
| `src/web/{index,scheduler,capture-history,config}.html` | `Shut down Pi` button (`class="bad"`) right after `Reboot Pi` |
| `scripts/setup-phase-08-daemon-host-access.sh` | New managed rule `63-optic-daemon-power-off.rules`; `pkcheck` post-check for `org.freedesktop.login1.power-off` |
| `verify.sh` | Phase 8: power-off decision check and rule file-mode check |
| `setup.md` §G, `docs/optic-daemon.md` §4 API list | Document the button, endpoint, and rule |
| `CLAUDE.md` | Tooling note on nested heredocs (see Failures) |

No CSS change: `.system-footer .button-row` already wraps (`flex-wrap`).

## Validation (Mac, 2026-09-21)

| Test | Command | Result |
|------|---------|--------|
| T1 | `cargo fmt --check` | pass (after `cargo fmt` reflowed the new test) |
| T1 | `cargo clippy --locked --all-targets -- -D warnings` | pass |
| T1 | `cargo test --locked` | pass, 159 tests, includes the new footer test |
| T2 | `npx @biomejs/biome@2.5.14 ci` | pass, `Checked 10 files`, no fixes |
| T3 | `bash -n` on `verify.sh` and the Phase 8 script | pass |
| T3 | `setup-phase-08-daemon-host-access.sh --print-rules <scratch>` | wrote all four rules. Evaluating the new rule with a stub `polkit` in Node: `power-off`/`liam` → `YES`; `reboot`/`liam` → no decision; `power-off`/other user → no decision |
| T4a | Pi (user approved): `sudo -n optic-setup-phase-08-daemon-host-access --dry-run` | 60/61/62 `[OK]`; `[CHANGE] Install PolicyKit rule: …/63-optic-daemon-power-off.rules`; linger and groups `[OK]` |
| T4a | Pi: same script, applied | `Installed /etc/polkit-1/rules.d/63-optic-daemon-power-off.rules`; its own `pkcheck` loop passed: "Reboot, shutdown, NTP sync, and timezone actions are authorized." Root is `ext4` (no overlay), so the rule persists |
| T4b | Deploy: `scripts/build-deploy-optic-daemon.sh` (full) | exit 0, `SUCCESS: optic-daemon 0.1.30 is active` (Pi was on 0.1.29, so this also shipped the rest of `main`) |
| T4b | `GET /api/system/shutdown` (non-destructive: route exists, POST only) | `405`, same as `/api/system/reboot`; an unknown `/api/system/nonexistent` returns `500` |
| T4b | Served `/`, `/scheduler.html`, `/capture-history.html`, `/config.html` | each has `footer-reboot` then `footer-shutdown` "Shut down Pi"; `/footer.js` references `/api/system/shutdown` |
| T4b | `verify.sh --phase 8` on the Pi, as `liam` (uses `sudo -n` for root checks) | `PASS=15 WARN=0 FAIL=0`, including `PolicyKit authorizes liam for Shut down Pi — org.freedesktop.login1.power-off` and `63-optic-daemon-power-off.rules (644 root:root)` |
| T4c | Real shutdown with the button | **not run**, by user choice: the user tests it when someone can power the Pi back on |
| Failure case | Button without the rule | not run on the Pi. By code review it uses the same `AppError` → `System action failed: …` path as Reboot |

## Failures Encountered

- A Python heredoc (`<<'EOF'`) that contained the rule's own `<<'EOF'` ended
  early, and zsh then tried to parse the rest (`parse error near ';;'`).
  `git diff` showed that nothing had been applied. It was rerun with an
  outer `<<'PYEOF'`, and the lesson was added to `CLAUDE.md`.
- SSH to `optic.local` briefly failed with `Undefined error: 0` (it resolves
  to both an IPv6 link-local address and 192.168.0.195), and one scp + dry-run
  call hung with no output. The call was stopped. `sha256sum` showed the upload
  had landed, and rerunning the dry run on its own with SSH keepalives
  worked. Plain `optic.local` then connected 3/3, so the fault was transient.
  `timeout` is not installed on the Mac; noted in `CLAUDE.md`.

## Limitations and Risks

- The real power-off has not been observed. PolicyKit authorizes it, but
  `systemctl poweroff` itself runs only when the user clicks the button.
- Found during deploy, not caused by this change: stopping the old 0.1.29
  process timed out and systemd SIGKILLed it (`Failed with result 'timeout'`).
  An unknown path under `/` returns `500` instead of `404`. Both are left for
  separate follow-up.

- No handler-level test on purpose: calling it on Linux CI would power off
  the runner.
- Like the reboot rule, only `org.freedesktop.login1.power-off` is granted.
  If another user has a logged-in session, logind asks for
  `power-off-multiple-sessions` instead, and the button fails with an error.
  The reboot rule has always had the same limit.
- The Pi needs someone on site to power it back on. The confirmation prompt
  says so.
- On the read-only root (Phase 9) the rule must be installed with the overlay
  off (`docs/phase9-readonly-root.md` §4).

## User Verification (steps 1–3 done by Claude on 2026-09-21; step 4 is yours)

1. Install the rule: copy the Phase 8 script to the Pi, run it with
   `--dry-run` (expect `[CHANGE] Install PolicyKit rule: …63-optic-daemon-power-off.rules`),
   then run it for real (expect "Reboot, shutdown, NTP sync, and timezone actions are authorized").
2. Deploy: `scripts/build-deploy-optic-daemon.sh` (full; this adds a new route).
3. `verify.sh --phase 8` → `PolicyKit authorizes liam for Shut down Pi`.
4. Open any page and click **Shut down Pi**. Cancel sends nothing. Confirm:
   the page goes offline and the Pi's LED shows it is halted. Power it on, then check
   `systemctl --user is-active optic-daemon.service`.

## Revision 1 (2026-09-21): one Power button with a menu

Status: **deployed (assets only) and verified served on the Pi; awaiting the user's check**

User request after the first version: merge the two footer buttons into one
button with a power icon. Clicking it drops down a menu with Reboot and Shut
down. The labels drop "Pi".

### Acceptance Criteria

- Every page footer has one **Power** button (icon only: the user asked for
  no text label mid-revision; its name is `aria-label`/`title` "Power") in place of
  the separate Reboot Pi / Shut down Pi buttons. Restart daemon is unchanged.
- Clicking it opens a menu with **Reboot** and **Shut down**. The menu opens
  upward, because the footer is at the bottom of the page.
- Choosing an item closes the menu, then shows the same confirmation prompt
  and posts to the same endpoint as before. Outside click and Escape close
  the menu without sending anything. `aria-expanded` follows the menu state,
  and Up/Down move between the items.
- No inline styles or scripts (CSP `style-src 'self'; script-src 'self'`);
  the icon is inline SVG using `currentColor`.
- No backend or PolicyKit change.

### Test Plan

- R1 `cargo fmt --check`, `clippy -D warnings`, `cargo test --locked`: the
  footer test now checks that each page has one `footer-power` button whose
  menu holds `footer-reboot` then `footer-shutdown`, and that the old
  "Reboot Pi" / "Shut down Pi" labels are gone.
- R2 `biome ci`.
- R3 in a browser, with `src/web` served statically on the Mac (no daemon;
  status calls fail harmlessly) and `window.confirm` stubbed to log and
  return `false`, so no dialog blocks the page and nothing is posted:
  open/close by click, outside click, Escape (focus returns to the button),
  item click closes the menu and calls confirm with the right prompt, the
  menu is visible above the button at desktop width and at 375 px width.
- R4 Pi: assets-only deploy (`--assets`), **only with user approval**, then
  check that the served pages have the new markup.

### Implementation

| File | Change |
|------|--------|
| `src/web/{index,scheduler,capture-history,config}.html` | `.power-menu`: icon-only `#footer-power` (inline SVG, `aria-haspopup="menu"`, `aria-expanded`, `aria-label="Power"`) plus a `#footer-power-menu` (`role="menu"`, `hidden`) holding `#footer-reboot` "Reboot" and `#footer-shutdown` "Shut down" |
| `src/web/footer.js` | `setPowerMenuOpen()`; toggles on button click, closes on outside click, Escape (focus back to the button) and item choice; Up/Down wrap between items; items then call the existing `footerSystemAction` |
| `src/web/styles.css` | `.power-menu*` rules: menu absolutely positioned above the button, right-aligned; square icon button |
| `src/web.rs` | Test renamed to `every_page_footer_has_one_power_menu`: one Power button with an accessible name, a menu holding Reboot then Shut down, no old "…Pi" labels |
| `setup.md` §G, `docs/optic-daemon.md` | Wording: the Power menu's Reboot / Shut down |

No backend, endpoint, or PolicyKit change.

### Validation

| Test | Result |
|------|--------|
| R1 `cargo fmt --check`, `clippy -D warnings`, `cargo test --locked` | pass; 159 tests incl. `every_page_footer_has_one_power_menu` |
| R2 `biome ci` | first run failed (formatting of one `const next =` line in `footer.js`); fixed with `biome format --write`; rerun: `Checked 10 files`, no fixes |
| R3 Chrome, `src/web` served by `python3 -m http.server` on 127.0.0.1, `config.html`, `confirm` stubbed to return `false`, POSTs recorded | Initially hidden, `aria-expanded=false`. Click opens it (focus on Reboot); Down→Shut down, Down wraps→Reboot, Up wraps→Shut down; Escape closes, focus back on Power; second click closes; outside click closes; Reboot and Shut down each close the menu and call confirm with the right prompt; **0 POSTs** sent. Desktop: menu bottom 6 px above the button, right-aligned. Narrowest Chrome allows (500 px viewport): footer wraps, menu inside the viewport (x 87–219), opens above, no horizontal scroll. Icon-only button 36×42, same height as Restart daemon. Screenshots checked by eye |
| R4 Pi assets deploy (user approved): `scripts/build-deploy-optic-daemon.sh --assets` | exit 0, `SUCCESS: static web assets deployed and verified`. The daemon was not restarted (same PID 11306 in the log). `curl` of `/`, `/scheduler.html`, `/capture-history.html`, `/config.html`: each has `footer-power`, `footer-reboot`, `footer-shutdown` and no "Reboot Pi"/"Shut down Pi"; served `footer.js` has `setPowerMenuOpen`, `styles.css` has `.power-menu-list` |

Limitation: 375 px could not be emulated (Chrome's minimum window gives a
500 px viewport). The menu extends 50 px left of the Power button, and the
Restart daemon button is always to its left, so it cannot leave the screen.

## Revision 2 (2026-09-21): Config page section order

User request: move **SYSTEM / Time & NTP** above **PREVIEW / Celestial
times** in `src/web/config.html`.

- Change: the two `<section>` blocks were swapped as-is. The page is a
  single-column grid (`main.config-main`); no CSS or JS depends on section
  order.
- Test (Chrome, static `src/web`): the section order is now
  `GLOBAL / Station`, `SYSTEM / Time & NTP`, `PREVIEW / Celestial times`,
  checked in the DOM and by screenshot. `biome ci` passes.
- Status: **deployed** with the full 0.1.30 deploy of the events log
  (`worklogs/2026-09-21-system-events-log.md`). The served `config.html`
  has station → time → celestial.

## Update: Reboot path validated on the Pi (2026-09-21)

During the system-events work (user approved), Power → Reboot's endpoint
(`POST /api/system/reboot`) rebooted the Pi, and it came back in ~21 s with
the daemon active. That confirms the reboot path works from the new menu's
endpoint. **Shut down** has still not been pressed on the Pi; that is the
user's acceptance step (someone must power it back on).
