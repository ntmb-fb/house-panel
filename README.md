# house-panel

A small home dashboard: your own floorplan SVGs with live device badges you can tap.
Built to replace Home Assistant without draining battery devices: nothing battery-powered is ever polled.

| Device | How state arrives | Controls |
|---|---|---|
| Philips Hue | bridge event stream (push) | tap toggles a light or room, long-press/right-click a light for brightness |
| Bluesound (BluOS) | `/Status` long poll (answers only on change) | play/pause, previous/next (preset on radio, track otherwise), presets, volume, mute |
| Onkyo (eISCP) | the receiver pushes on its TCP connection | power, volume, mute, input |
| LG webOS TV | websocket subscriptions (push); Wake-on-LAN to turn on | power, volume, mute, Home/Netflix/YouTube |
| tado° | cloud API every `interval_minutes` (default 30), and right after a change | target temperature (until the next schedule block), back to schedule |

Battery devices listed under `[panel] unavailable` are shown greyed out and never contacted.

## Files

The image contains only code. Everything about your house is mounted:

- `/floorplan`: `house.svg` + `house.css` (2D), `house3d.svg`, `house3d_south.svg`, `house3d_portrait.svg` + `house3d.css`.
  Elements are matched by id (`dev-*`, `room-*`, `val-*`). A device gets the class `on`, `off` or `unavailable`.
- `/data`:
  - `config.toml`, see [config.example.toml](config.example.toml)
  - `hue-key`: the bridge username / application key
  - `lgtv-key`: the TV's client key from an earlier pairing
  - `tado-token.json`: a tado OAuth token (device flow, `offline_access`); the service refreshes it and writes it back

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `DATA` | `/data` | Folder with `config.toml` and the secrets |
| `FLOORPLAN` | `/floorplan` | Folder with the SVG/CSS files |
| `PORT` | `8770` | Web port |

The page has no password: anyone on the network can use it.

## Install (Synology)

1. Put the files above in `/volume1/docker/house-panel/{data,floorplan}`.
2. Container Manager > Project > Create, paste `compose.yaml`.
3. Open `http://<nas>:8770`.
