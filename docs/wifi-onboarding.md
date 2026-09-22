# Design Proposal: First-Time Wi-Fi Setup

Status: **proposal, not implemented** (2026-09-21). Written after the
services audit (`docs/pi-services-audit.md`), to check that disabling
unused services does not rule out a future setup path.

## 1. Problem

Imagine Optic as a finished product: someone unboxes it at home, and it
has never been on their Wi-Fi. Today the only way to give it Wi-Fi is to
set it when the SD card is written (Raspberry Pi Imager → cloud-init). That
needs a computer, and it does not help a box that is already assembled.
The goal is to set Wi-Fi up with only an iPhone.

## 2. Constraints

- 1 GB RAM. optic-daemon is capped at `MemoryMax=500M`; the setup path
  should add only a few MiB, and only while it is running.
- Unattended for 365 days: the setup mode must never take the box off a
  working network, and never interrupt a capture.
- No RTC battery: an out-of-box Pi has the wrong clock until NTP syncs.
- The daemon runs unprivileged on TCP 8000 (`docs/optic-daemon-system-service.md`).
- The setup path is a way into the box, so it needs a password and a time
  limit.

## 3. Facts Observed on the Pi (2026-09-21)

| Fact | Evidence |
|------|----------|
| The Wi-Fi chip supports access-point mode, on 2.4 and 5 GHz | `nmcli -f WIFI-PROPERTIES device show wlan0`: `AP: yes`, `5GHZ: yes` |
| NetworkManager runs the network; its shared (hotspot) mode needs dnsmasq | NetworkManager 1.52.1 active; `dnsmasq-base` 2.91 installed |
| Bluetooth hardware exists but is off | `dtoverlay=disable-bt` in `config.txt`; `bluetooth.service` disabled; `bluez` 5.82 still installed |
| Ethernet profile exists | `netplan-eth0` (DHCP), `eth0` currently has no cable |
| USB-C network gadget is available but off | `rpi-usb-gadget-ics.service` ("USB gadget ICS auto-switcher"), disabled |

Not verified: whether this chip can host a hotspot and stay joined to a
network at the same time; how iOS captive-portal detection behaves against
this Pi; whether the Pi 5 power button can be reused as a "setup" button.

## 4. Options

| # | Option | iPhone steps | Pi side | Effort | Verdict |
|---|--------|--------------|---------|--------|---------|
| 1 | **Setup hotspot + captive page** | Join `Optic-Setup-XXXX` in Settings; a setup page opens; choose the home network, type its password | NetworkManager hotspot, plus a setup mode in optic-daemon | Medium | **Recommended** |
| 2 | Bluetooth (BLE) setup | Open an Optic iPhone app; it finds the box and sends the Wi-Fi name and password | Turn Bluetooth back on; a small BLE setup service (for example the Improv Wi-Fi protocol) | High: needs an iPhone app, because Safari on iOS has no Web Bluetooth | Only if an app is planned anyway |
| 3 | Ethernet cable | Plug into the router, open `optic.local:8000`, enter Wi-Fi in the dashboard | A Wi-Fi form in the dashboard | Low | **Fallback**, and useful on its own |
| 4 | Set Wi-Fi when writing the SD card | Needs a computer | cloud-init (today's path) | None | Keep for builders, not for users |
| 5 | USB-C network to a computer | Needs a computer (iPhone unverified) | USB gadget mode | Medium | Not for iPhone |
| – | Apple's accessory setup (WAC) | – | – | Requires Apple's paid MFi program | Not available |

## 5. Recommended Design (Option 1, with Option 3 as a Fallback)

### 5.1 When setup mode starts

- **At boot:** no saved Wi-Fi connects within about 90 s *and* Ethernet
  has no link. This matches the existing 90 s NTP wait.
- **Never** while any network (Wi-Fi or Ethernet) is connected, so a
  working station cannot be pulled offline.
- **Optional physical trigger** (unverified): a long press of the Pi 5
  power button, or a jumper, to re-enter setup after a move.
- Ends after 15 minutes without a successful setup, then tries the saved
  networks again. That keeps a router outage from leaving the box stuck
  in setup mode.

### 5.2 Flow

1. The box scans the nearby networks first, while the radio is still free,
   and keeps the list for the page.
2. It starts the hotspot `Optic-Setup-<last 4 of MAC>`, WPA2, with a
   per-device password printed on a label (for example as a Wi-Fi QR code
   the iPhone camera can join).
3. The iPhone joins; iOS probes `captive.apple.com` and opens the setup
   page (captive-portal behavior: unverified).
4. The page shows the scanned networks. The user picks one and types the
   password. The page also sends **the phone's current time and time
   zone**. With no RTC battery, that gives the box a correct clock before
   NTP.
5. The box saves a NetworkManager profile, then drops the hotspot and
   tries the new network (the single radio probably can't do both; see
   §3).
6. Success: the box is reachable at `optic.local:8000`. Failure (wrong
   password, out of range): the hotspot returns with the error shown, and
   the phone rejoins it.

### 5.3 Parts

| Part | Where | Note |
|------|-------|------|
| Hotspot | NetworkManager profile, `mode=ap`, `ipv4.method=shared` | NM's shared mode runs dnsmasq for DHCP |
| Captive redirect | DNS answers pointing everything at the box, and TCP 80 → 8000 | iOS probes port 80. The daemon cannot bind 80 unprivileged, so this needs an nftables redirect, a systemd socket on :80, or `CAP_NET_BIND_SERVICE`. That is a sandbox decision to make deliberately |
| Setup page + API | optic-daemon, active only in setup mode | Scan list, save credentials, set time and time zone. Reuse the existing timezone path (polkit rule 62) |
| Saving the Wi-Fi profile | NetworkManager over D-Bus | Needs a new narrow polkit rule, like rules 60–64 |
| State machine | optic-daemon, or a small separate unit | Keeps setup mode out of the capture path; the scheduler pauses captures while in setup |

### 5.4 Services this depends on

These must stay enabled, and none of them is touched by the audit's
disable list: NetworkManager, wpa_supplicant, `dnsmasq-base` (a package,
not a service), avahi (so `optic.local` works after setup), and polkit.
Bluetooth stays off; only Option 2 would need it.

## 6. Effect on the Audit's Choices

- **Bluetooth:** off since Phase 4. Reversible: remove `dtoverlay=disable-bt`
  from the Phase 4 block in `scripts/setup-phase-04-unused-hardware.sh`,
  drop the `bluetooth.service` and `disable-bt` checks from that script
  and `verify.sh`, enable `bluetooth.service`, and reboot.
- **cloud-init:** disabling it only affects Option 4 on a box that is
  already running. A freshly written SD card still carries Imager's
  settings, because writing the card also removes
  `/etc/cloud/cloud-init.disabled`.
- **Everything else** in the disable list is unrelated to network setup.

## 7. Open Questions

1. Is this a product for other people, or a box for one owner? For one
   owner, Option 3 (a dashboard Wi-Fi form) may be enough.
2. Setup trigger: boot-only, or also a physical button?
3. Security: a printed password label, or an open hotspot limited to a
   short window?
4. Is an iPhone app on the roadmap (which would make Option 2 worth it)?

## 8. Tests Needed Before Implementing

- Can the chip host a hotspot and stay joined to a network at the same
  time? (`nmcli` on the Pi.)
- iOS captive-portal detection with the redirect.
- Memory while in setup mode (NM shared mode + dnsmasq).
- Wrong password, router off, and a power cut during setup.
- A working station never enters setup mode (router reboot during a
  capture).
