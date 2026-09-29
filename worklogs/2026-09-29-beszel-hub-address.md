# Dated Worklog: 2026-09-29 - Beszel Monitoring Was Silently Dead (Hub Bound to a Stale IP)

Status: **fixed and verified on both hosts**; repo changes pending review.

## Objective

Host monitoring had produced nothing for days. The Pi's journal was filling
with, every 10 seconds:

```
beszel-agent WARN WebSocket connection failed
  err="dial tcp 192.168.0.202:8090: connect: no route to host"
```

## Diagnosis (observed 2026-09-29)

Two separate faults, both invisible from either side alone:

1. **The hub was listening on an address the Mac no longer had.**
   `launchctl list` showed `dev.beszel.hub` running, and `lsof` showed
   `beszel ... TCP 192.168.0.202:8090 (LISTEN)`. But `ifconfig` showed this
   Mac holds only `192.168.0.231` (en1). Its launcher
   (`~/.local/lib/beszel/run-hub`) resolves the LAN address **at startup**
   and passes it to `beszel serve --http "${lan_ip}:8090"`. It started when
   the Mac was `.202`; the DHCP lease later changed, and the process kept a
   socket on an address that no longer existed. The hub looked healthy the
   whole time.
2. **The Pi's agent still pointed at the old address** — `HUB_URL` in
   `~/.config/systemd/user/beszel-agent.service` was
   `http://192.168.0.202:8090`. The agent cannot use `.local` names
   (`setup.md` §Monitoring: its static Go resolver never issues mDNS), so it
   needs a literal IP.

Consequence beyond lost monitoring: about 1,200 log lines an hour into the
16 MiB persistent journal, evicting the crash evidence that journal exists
to preserve. It also meant the 2026-09-25 power-loss outage (the Pi was off
for ~60 hours) went unnoticed.

## Fix Applied

On the Mac (backups kept alongside each file):

- `~/.local/lib/beszel/run-hub`: binds `--http "0.0.0.0:8090"` instead of the
  startup LAN address, so a lease change cannot strand it again. Its
  `BESZEL_HUB_APP_URL` also still said `http://tests-iMac-Pro.local:8090`, a
  hostname from two renames ago; now `http://imac.local:8090`.
- `launchctl kickstart -k gui/501/dev.beszel.hub`.

On the Pi:

- `HUB_URL=http://192.168.0.231:8090` in the agent's user unit, then
  `systemctl --user daemon-reload` and `restart`.

## Validation

| # | Check | Result |
|---|---|---|
| 1 | `sh -n run-hub` | clean |
| 2 | `lsof -nP -iTCP:8090 -sTCP:LISTEN` after restart | `beszel ... TCP *:8090 (LISTEN)` — all interfaces |
| 3 | `curl http://192.168.0.231:8090/api/health` from the Mac | `{"message":"API is healthy.","code":200}` |
| 4 | Same from the Pi | `200` |
| 5 | Agent log after restart | `INFO WebSocket connected host=192.168.0.231:8090` |
| 6 | `no route to host` count after the reconnect | **0** (was ~30/minute) |
| 7 | `bash -n verify.sh` | clean |

## Repo Changes

- `verify.sh`: `EXPECTED_HUB_URL` → `http://192.168.0.231:8090`.
- `setup.md`: the hub launcher must bind `0.0.0.0:8090`, with the reason.
- `docs/pi-services-audit.md`: hub address updated, noting what was observed
  at audit time rather than rewriting it.

## Remaining Limitations / Follow-up

- **The root cause is unfixed:** the agent needs a literal IP, so the next
  DHCP lease change breaks monitoring again. Reserve a fixed IP for the Mac
  on the router by MAC address, as `setup.md` already recommends. This is the
  third outage from this one cause: Beszel (September), `optic_sync`'s
  hostname (2026-09-28), and this.
- `verify.sh` hard-codes the hub IP, so it will need editing again after any
  reservation change. A `verify.sh` override variable would be better.
- Nothing watches the hub itself. A dead hub is indistinguishable from a
  healthy one unless someone opens the dashboard. The external heartbeat
  (`docs/optic-daemon-alerts.md`) is the intended answer.
