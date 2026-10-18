# gowl

Make Govee lights follow your screen or your music, on a Linux Wayland desktop, over your home network.
One small program, no app, no account, no cloud.

- **Screen sync:** the lights take the colors at the edges of your screen (great behind a monitor).
- **Music sync:** the lights react to whatever your computer is playing.
- **Plain control:** on, off, brightness, color, white temperature.
- **Keybind-friendly:** `gowl toggle screen` / `gowl toggle music` start, stop or switch, with a short notification.
- **Shell-friendly:** a status line for bars, and a ready-made quickshell service + tile.

It talks to the lights directly over the LAN (Govee's "LAN Control"), uses almost no CPU or memory, and sends
nothing anywhere else.

> **Personal project.** I built gowl for my own convenience on my own setup (Fedora + niri, one Govee H6609)
> and share it as-is, in case it is useful to someone. There is **no warranty** and no support promise.
> It is **unofficial**: not affiliated with, endorsed by or connected to Govee, or to the LumiSync project.

## Credits

gowl is a from-scratch Rust rewrite of the Govee LAN, screen-sync and music-sync parts of
**[LumiSync](https://github.com/Minlor/LumiSync)** by **Minlor** (MIT license). The protocol knowledge, the
color and smoothing behaviour, the audio-reactive reactions and palettes and the SKU segment table all follow
LumiSync, and gowl's test vectors are generated from LumiSync's own code so the two behave alike.
**Thank you, Minlor** — without LumiSync this would not exist. LumiSync's copyright and license are in
[`NOTICE`](NOTICE).

## What it works with

- **Lights:** any Govee light that has **LAN Control** and segment ("razer" / DreamView) support. Nothing is
  tied to my device: the light, its segment layout and its keepalive come from your config (`gowl calibrate`
  writes them). Tested only on an **H6609**.
- **Screen sync:** a Wayland compositor with `wlr-screencopy` — niri, sway, Hyprland, river, … (not GNOME or
  KDE). Tested on niri.
- **Music sync:** PipeWire (with its PulseAudio server) or PulseAudio.
- **VPNs:** built to keep working with a VPN up; checked by hand with ProtonVPN, and in a simulation for
  Tailscale-, WireGuard- and OpenVPN-style routing (see [VPNs](#vpns)).

## Requirements

- Linux with a Wayland compositor as above (developed on Fedora 44, niri 26.04).
- Rust (stable) and cargo to build: `sudo dnf install rust cargo`.
- The light on the same network as the computer, with **LAN Control** switched on in the Govee Home app
  (device settings).
- For music: `libpulse` (Fedora: `pulseaudio-libs`, installed with PipeWire's pulse server). It is loaded only
  in music mode.
- For notifications: `notify-send` (Fedora: `libnotify`) and any notification server. Optional.

gowl itself depends on nothing but the C library (one Rust crate, `libc`).

## Install

Per user, no root. From the source directory:

```sh
make install              # build, then install for your user (prints every change it makes)
gowl setup                # find your light, calibrate it, pick keys and login behaviour
```

`make install` puts:

| What | Where |
|---|---|
| the program | `~/.local/bin/gowl` (`PREFIX=…` to change `~/.local`) |
| the login service (screen sync), **not enabled** unless `AUTOSTART=1` or you say yes in `gowl setup` | `~/.config/systemd/user/gowl.service` |
| a config with every setting commented out — only if you have none (never overwritten) | `~/.config/gowl/config` |
| niri keys **Mod+M** (screen sync on/off) and **Mod+Shift+M** (music sync on/off) | `~/.config/gowl/niri-binds.kdl`, plus one `include` line at the end of `~/.config/niri/config.kdl` |
| quickshell service + tile, and `config.example` | `~/.local/share/gowl/` |

The niri line is added only if neither key is bound yet (in your config or anything it includes), only after
`niri validate` accepted the edited config, and after a backup in `~/.config/gowl/backups/`. If a key is taken,
the install goes on without keys and says how to pick others. `NIRI_KEYS=0` skips niri; `gowl keys add` /
`gowl keys remove` do just that part later. The keys and the service call gowl by its full path, because niri and
systemd often don't have `~/.local/bin` on their PATH. Running `make install` again only updates what changed.

`make uninstall` stops a running sync, stops and disables the service, takes the niri line out (validated, backed
up), and removes the program, the service, the keys file and `~/.local/share/gowl`. It keeps
`~/.config/gowl/config` (and the niri backups); `make uninstall PURGE=1` removes those and the device cache too.

**`gowl setup`** explains LAN Control, then finds the light: the one in your config, a network scan, or — when
the firewall hides the light's answers, as Fedora's does by default — its IP address typed in or picked from the
Govee devices this computer has seen. The light blinks blue and you confirm it. Then: segments and `gowl
calibrate`, optionally `gowl calibrate --keepalive`, screen sync at login, niri keys, notifications, and a
quickshell hint. It changes only the lines it needs in your config and never touches the firewall.

By hand instead: `cargo build --release`, copy `target/release/gowl` anywhere, and write the config (below);
`GOWL_DEVICE=192.168.1.50 gowl color ff0000` works without any config.

## Commands

```
gowl setup                           first-run questions (needs a terminal)
gowl scan [--sweep] [--timeout MS]   find lights on the network; remembers them
gowl list                            lights found before (no network)
gowl on | off [DEV]
gowl brightness 0-100 [DEV]
gowl color RRGGBB | R,G,B [DEV]
gowl temp KELVIN [DEV]               white, 2000-9000
gowl status [DEV]                    what the light reports
gowl doctor [DEV]                    network diagnostics (LAN interface, VPN, firewall)
gowl outputs                         list monitors (name, size, scale)
gowl screen [--output NAME] [--fps 1-40] [--depth 0.10-0.50] [DEV...]
gowl music  [--reaction R] [--palette P] [--fps N] [DEV...]
gowl calibrate [DEV]                 find where each segment sits; saves layout=
gowl calibrate --keepalive [DEV]     find how long the light holds a frame; saves keepalive_ms=
gowl pause | resume | stop           control the running sync
gowl toggle [screen|music] [ARGS]    what the keys run (below)
gowl state                           the sync's state as one JSON line
gowl watch                           the state line now and on every change (for bars)
gowl keys [add|remove]               niri keys Mod+M / Mod+Shift+M: show, add or remove
```

`DEV` is an alias from the config, an IP, a MAC, or a number from `gowl list`; without it gowl uses
`GOWL_DEVICE`, the config's `device =`, or the only light it knows. `screen` and `music` take several lights.
Only one sync runs at a time.

**`gowl toggle screen` / `gowl toggle music`:** that mode running → stop it; paused → resume it; the other mode
running → switch (the other one stops first, so two syncs never fight over the light); nothing running →
start. Keys: Mod+M and Mod+Shift+M (`make install` or `gowl keys add` sets them up in niri; the template is
[`packaging/niri-binds.kdl`](packaging/niri-binds.kdl)). `toggle`, `pause`, `resume` and `stop` each show a
short desktop notification ("Screen sync on",
"Switched from screen to music sync", "Music sync off", or what went wrong, e.g. "No light configured"). A
sync started from a key that fails later also says so. Set `notify = off` to silence them.

Reactions: `flow pulse energy_fill band_split center_burst wave bass_bounce chase twinkle auto`.
Palettes: `rgb auto rainbow aurora sunset ocean neon jewel fire spectrum warm cool mono`.

Exit codes: 0 ok, 1 runtime error (also: nothing running), 2 usage or a bad config value, 3 no light
configured, 4 screen capture unavailable, 5 no LAN interface reaches the light, 6 another gowl sync holds the
light.

## Config

`~/.config/gowl/config` (or `$XDG_CONFIG_HOME/gowl/config`, or `GOWL_CONFIG`). `key = value`, `#` comments,
every key optional; [`config.example`](config.example) lists every key with its default, commented out (that is
the file `make install` starts you with).

| Key | Default | What it does |
|---|---|---|
| `device` | — | default light: alias, IP or MAC |
| `output` | first monitor | monitor to follow (`gowl outputs`) |
| `fps` | 30 | screen frames per second, 1-40 (40 is the lights' limit) |
| `idle_fps` | 5 | capture rate once the screen edges stop changing (0 = off) |
| `idle_after_ms` | 1000 | how long "not changing" must last first |
| `depth` | 0.28 | how far the edge zones reach into the screen (0.10-0.50) |
| `saturation` | 1.15 | color boost |
| `screen_brightness` | 0.75 | screen sync color multiplier, 0-1 |
| `smoothing` | 0.55 | higher = snappier, lower = smoother |
| `threshold` | 3 | smallest color change worth sending |
| `keepalive_ms` | 500 | resend interval on a still screen; `gowl calibrate --keepalive` finds your light's |
| `reaction`, `palette` | flow, rgb | music look |
| `music_fps` | 60 | music frames per second (sent at most 40/s) |
| `music_gain`, `music_brightness`, `music_smoothing` | 1.7, 0.85, 0.6 | music level, brightness, smoothing |
| `music_fade_ms` | 1000 | when the music stops, dim to black over this long (0 = go black at once) |
| `lan_if` | automatic | force the network interface (normally chosen by the light's subnet) |
| `notify` | on | desktop notifications for toggle/pause/resume/stop and failures |

Per light, in a `[device ALIAS]` section: `mac`, `ip`, `sku`, `segments`, `layout` (written by
`gowl calibrate`), `reverse`, `offset`, `port` (control port override, default 4003).

```ini
device = desk
keepalive_ms = 5600

[device desk]
ip = 192.168.1.50
layout = 1 7 8 8 8 9 6 3 2 2
```

Environment: `GOWL_DEVICE`, `GOWL_CONFIG`, `GOWL_LOG=debug` (verbose, once-per-second counters).

## Status for bars (quickshell, waybar, …)

A running sync keeps one JSON line in `$XDG_RUNTIME_DIR/gowl/state` and rewrites it only when something changes:

```json
{"running":true,"pid":4242,"mode":"screen","paused":false,"fps":30,"devices":[{"name":"desk","ip":"192.168.1.50","iface":"enp5s0","ok":true}],"error":null}
{"running":false}
```

| Field | Meaning |
|---|---|
| `running` | a sync (or calibration) is running; when false it is the only field |
| `pid` | its process id |
| `mode` | `screen`, `music`, `calibrate`, or `unknown` (an older gowl) |
| `paused` | paused with `gowl pause` (the light is back to normal) |
| `fps` | screen frames/s, or music renders/s |
| `reaction`, `palette` | music only |
| `devices` | each light: `name` (alias, else IP), `ip`, `iface` (LAN interface, `""` while none reaches it), `ok` |
| `error` | `null`, or why the light isn't following: `"screen output gone"`, `"sound server unreachable"`, `"desk: no LAN interface reaches it"`, `"desk: sending fails"` |

- `gowl state` prints the current line (and notices a crashed sync's leftover file).
- `gowl watch` prints it once, then again on every change, and otherwise sleeps — no polling, no wakeups (it
  watches only gowl's own directory, so other programs' files don't wake it). It exits when its reader goes
  away. Use it from any bar that runs a command and reads lines.

### quickshell

[`contrib/quickshell/Gowl.qml`](contrib/quickshell/Gowl.qml) is a `pragma Singleton` service, and
[`contrib/quickshell/GowlTile.qml`](contrib/quickshell/GowlTile.qml) an example tile (Screen and Music toggles
plus a status line; right-click pauses). `make install` copies them to `~/.local/share/gowl/quickshell/`; gowl
never edits your shell. Copy both into a directory of your shell that holds singletons, e.g.:

```sh
cp ~/.local/share/gowl/quickshell/*.qml ~/.config/quickshell/myshell/services/
```

The service runs one `gowl watch` (restarted with a back-off if it exits), so it costs nothing while the state
doesn't change, and finds gowl on PATH or in `~/.local/bin` (or set `Gowl.gowl` to a path). Its properties
mirror the state line — `running`, `mode`, `paused`, `devices`, `error` — plus `available` (gowl answered),
`screenOn` and `musicOn`; its functions are `toggleScreen()`, `toggleMusic()`, `pause()`, `resume()`, `stop()`,
`color(hex)` and `brightness(pct)`.

```qml
import qs.services

GowlTile { width: 260 }        // the example tile, in any panel
Text { text: Gowl.musicOn ? "♪" : Gowl.screenOn ? "▣" : ""; visible: Gowl.running }
```

## Running at login

[`packaging/gowl.service`](packaging/gowl.service) is a systemd user unit that runs `gowl screen` with the
graphical session. `make install` installs it; `make install AUTOSTART=1`, `gowl setup` or
`systemctl --user enable gowl` turns it on. `gowl stop` or a toggle ends it with status 0, so systemd leaves it
stopped until the next login (or `systemctl --user start gowl`). Errors that can clear up (the compositor or the
network not ready yet at login, a crash) are retried after 2 s, stepping up to a minute; errors that can't — no
light configured, a bad config, the light already held by another gowl — stop it for good (exit 2, 3, 6). A sync
you start from a key runs on its own, outside the service; if you press a key (or run `gowl stop`) while the
service is waiting to retry, it stays stopped instead of starting screen sync behind your back.

## Troubleshooting

**No light found / `gowl status` gets no answer.**
1. Check **LAN Control** is on for the light in the Govee Home app, and that light and computer are on the
   same network.
2. Run `gowl doctor`: it shows which interface gowl uses, whether a VPN took over the route, and whether the
   firewall rejected the light's replies.
3. Fedora's default firewall drops the light's replies to `scan` and `status` (sync and plain commands don't
   need replies, so they work regardless). Allow only your LAN, and only for this session first:
   ```sh
   sudo firewall-cmd --zone=public --add-rich-rule='rule family="ipv4" source address="192.168.1.0/24" port port="4002" protocol="udp" accept'
   # works? keep it across reboots:
   sudo firewall-cmd --permanent --zone=public --add-rich-rule='rule family="ipv4" source address="192.168.1.0/24" port port="4002" protocol="udp" accept'
   ```
   Use your own subnet and zone (`gowl doctor` prints the exact line). Or skip it: put `ip =` in the config.
4. Give the light a **DHCP reservation** in your router, so its address never changes.

**The lights go black on some windows.** Windows you gave niri's `block-out-from "screen-capture"` window rule
(say, a password manager) are black in every screen capture, so the lights see black. That's intended;
change the rule if you want them synced. (`block-out-from "screencast"` doesn't affect gowl.)

**Colors land in the wrong place, or run the wrong way round.** Run `gowl calibrate`: it lights one segment at a
time and you type where it is (numpad layout); it saves `layout =` for that light.

**The light flickers back to its own mode on a still screen.** Run `gowl calibrate --keepalive`; it measures how
long your light holds a frame and saves a safe `keepalive_ms`.

**A key does nothing.** Look for the notification. Without one, check `gowl keys` (is the include there, is a
key taken?). Binding keys by hand: compositors and shells started by systemd often have
`PATH=/usr/local/bin:/usr/bin`, without `~/.local/bin`, so bind the full path (`gowl keys add` does). Running
`gowl toggle screen` in a terminal shows what gowl says.

## VPNs

gowl binds every socket to the network interface that holds the light's subnet, so a VPN that captures all
traffic can't pull the light's packets into the tunnel, and it rebinds by itself when networks change.
Checked by hand: ProtonVPN connecting and disconnecting during a running sync on my machine was seamless (no
gap, no restart). Not checked by hand: Tailscale, WireGuard and OpenVPN. Their typical routing (a full
tunnel, Tailscale's overlapping LAN route and exit node, multicast pulled into the tunnel) is reproduced in a
network-namespace simulation (`tools/netns_vpn_test.py`), where every packet stays on the LAN interface and a
network change mid-sync is followed within about half a second. gowl never changes routes,
firewall or VPN settings and **never bypasses a kill switch**: if a VPN kill switch blocks LAN traffic,
gowl says so and you allow LAN access in the VPN app.

## Performance

Measured on my machine (Fedora 44, 3440x1440 at 100 Hz, NVIDIA RTX 4060), real H6609, 2026-10-18:

| Scenario | gowl CPU (one core) | Wakeups/s | Memory (RSS) |
|---|---|---|---|
| Screen sync, fullscreen video, 30 fps | 0.37 % | 69.5 | 8.4 MB |
| Music sync, music playing | 0.68 %\* | 48.8\* | 6.1 MB |
| Paused | 0 % | 0 | 3.1 MB |

\* Measured before a fix that stopped music mode from sending every frame twice; to be re-measured on the
light. Against the test device after the fix: 0.31 %, 50 wakeups/s, 6.1 MB, 39 packets/s.

Binary: about 754 KiB (771,744 bytes), depending only on the C library. While paused gowl does nothing at all; on a still
screen it sends only the keepalive. The compositor's own cost of handing out screen captures is separate and
depends on the GPU: niri spent a noticeable share of a core on this NVIDIA machine at 30 fps, which is why
gowl drops to `idle_fps` while the screen edges don't change. Method and full numbers: `docs/BUDGETS.md`.

## Privacy

gowl only talks to your lights on your local network. No internet traffic, no DNS lookups, no telemetry, no
update checks, no accounts. Notifications are local (`notify-send`).

## License

gowl is released under the MIT License, see [`LICENSE`](LICENSE). LumiSync's MIT notice: see [`NOTICE`](NOTICE).
