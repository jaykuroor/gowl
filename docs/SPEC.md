# SPEC — what gowl does

## In scope (v1)
1. **Discovery** of Govee LAN devices (multicast scan, optional /24 unicast sweep).
2. **One-shot control**: power, brightness, solid color, white temperature, status.
3. **Screen sync** on niri: capture one output, average edge zones, smooth, stream
   per-segment colors ("razer" frames) to one or more devices.
4. **Music sync**: capture the default sink's monitor, FFT into bass/mid/treble, render
   a reaction × palette, stream segment colors.
5. **Runtime control** of a running sync: pause/resume, stop, toggle/switch (niri keybind friendly), with
   desktop notifications (`notify-send`, no D-Bus code) and a status file + `gowl state`/`gowl watch` for bars.
   Contrib: a quickshell singleton + tile; packaging: systemd user unit, niri binds (installed in Phase 7).
6. **Calibrate**: light segments one by one so the user can confirm count, order, direction;
   `--keepalive` finds the slowest keepalive the light tolerates.

## Out of scope (do not build)
Qt/any GUI, tray icon, Govee cloud/MQTT/Platform API, accounts, Tuya/LSC, iDotMatrix/BLE,
plug metering, update checker, album-art palette, Windows/macOS/X11, xdg-desktop-portal
/ PipeWire video capture (keep a trait seam for it; see `ARCHITECTURE.md`), the legacy
LED-mapping editor (replaced by `gowl calibrate` → `layout`, plus `reverse`/`offset`/`depth` config).

## CLI
Hand-parsed. `DEV` = alias | IP | MAC | index from `gowl list`; omitted → config
`device=` → the only cached device → error. Several `DEV`s allowed for sync modes.

```
gowl scan [--sweep] [--timeout MS]   discover; write device cache; print table
gowl list                            print cached devices (no network)
gowl on|off [DEV]
gowl brightness 0-100 [DEV]
gowl color RRGGBB | R,G,B [DEV]
gowl temp KELVIN [DEV]               2000-9000
gowl status [DEV]                    devStatus query (falls back to "status")
gowl outputs                         list Wayland outputs: name, size, scale, transform
gowl screen [--output NAME] [--fps 1-40] [--depth 0.10-0.50] [DEV...]
gowl music  [--reaction R] [--palette P] [--fps N] [DEV...]
gowl calibrate [DEV]                 segment walk: 1 red segment at a time, type its numpad
                                     position; saves segments= and layout= (D-040)
gowl calibrate --keepalive [DEV]     hold a frame, repeat it every 0.5/1/2/3/5/8 s, ask after each
                                     whether the light dropped out; saves keepalive_ms = 70 % of
                                     the longest interval it held (D-046)
gowl pause | resume | stop           signal the running sync (pidfile); stop waits ≤ 1 s for the exit
gowl doctor [DEV]                    network diagnostics: LAN iface, VPN hijack, firewall (docs/NETWORK.md)
gowl toggle [screen|music] [ARGS]    (D-059) that mode running → stop (wait ≤ 1 s); paused → resume;
                                     the other mode running/paused → stop it (wait ≤ 2 s for its exit,
                                     razer OFF ×2), then run this one; calibrate running → refuse (exit 1);
                                     nothing running → run that mode (default screen) in the foreground with
                                     ARGS (niri `spawn`)
gowl state                           print the status line (D-060); {"running":false} when idle; exit 0
gowl watch                           print the status line now and on every change (inotify on
                                     $XDG_RUNTIME_DIR/gowl/ + a pidfd on the session, 0 wakeups between
                                     changes); exits on SIGTERM/SIGINT/SIGHUP or when stdout's reader
                                     goes away
gowl --version | --help
```
Exit codes: 0 ok, 1 runtime error (also: pause/resume/stop with nothing running, or a second
sync while one runs), 2 usage error, 3 no device, 4 capture unavailable, 5 no LAN interface for the device.
One streaming session at a time: `screen`, `music` and `calibrate` hold the pidfile.
All diagnostics to stderr, one line each; `GOWL_LOG=debug` for verbose.

## Config
`$XDG_CONFIG_HOME/gowl/config` (default `~/.config/gowl/config`). `key = value`,
`#` starts a comment anywhere on a line (values never contain `#`), whitespace trimmed,
`[device ALIAS]` sections. Unknown keys → warning, not error.
See `config.example`. Env overrides: `GOWL_DEVICE`, `GOWL_LOG`, `GOWL_CONFIG`.

Device cache (written by `scan`): `$XDG_STATE_HOME/gowl/devices`, one device per line:
`mac ip sku`. Pidfile: `$XDG_RUNTIME_DIR/gowl.pid`, claimed by an exclusive OFD lock held for the
session (the file carries the pid; an unlocked file is stale, D-048).
`calibrate` rewrites the config byte-for-byte except the keys it sets (`segments`/`layout`
in the device section, global `keepalive_ms`).

## Behaviour that must match legacy
| Thing | Legacy value (see `legacy/lumisync/config/options.py`) |
|---|---|
| Screen sampling | edge zones from `led_mapping.generate_screen_mapping`, capture depth 0.28, ≤24×24 samples per zone, gamma-correct (linear-light) average |
| Saturation | 1.15 around Rec.709 luma |
| Brightness factor | screen 0.75, music 0.85 (multiplies channels; separate from device brightness) |
| Smoothing | per-zone EMA, α = 0.55 screen, 0.60 music; starts from black (first frames fade in), zone-count change snaps |
| Send policy | send if any channel moved > 3, else keepalive every 0.5 s |
| Rate caps | screen 45 fps (gowl default 30 — see BUDGETS), music 60 fps, device cap 40 Hz |
| Stream start/end | razer mode ON before first frame, OFF on exit/pause |
| Segment count | user override → SKU table → 10 |
| Audio bands | Hann, rfft, bass 20–250 / mid 250–4000 / treble 4000–16000 Hz, AutoGain (target 0.10, max boost 40, attack 0.15 s, release 1.0 s), music gain 1.7 |
| Music idle (gowl, D-054/D-063) | 1.5–2.0 s after the last audio window, the last output fades to black over `music_fade_ms` (default 1000; ease-out (1 − p)², 30 ms timer steps through the normal gate, exact black last; 0 = instant black). A window arriving mid-fade cancels it at once and the EMA continues from the dimmed output. |

Additions over legacy (allowed because they reduce cost or fix Wayland gaps):
damage-aware zone recompute, pull-paced capture (no frames when screen static),
per-device `reverse`/`offset` or a calibrated `layout` (D-040), per-device control `port` (D-049),
multi-device fan-out from one capture, VPN-proof interface-bound sockets with automatic rebind on network changes,
and an idle capture rate (D-057): `idle_fps` (default 5; 0 or ≥ `fps` turns it off) once no zone has moved more
than `threshold` for `idle_after_ms` (default 1000); the first frame that does brings back `fps` at once. The
cost is up to one idle interval (200 ms at 5 fps) more delay on the first reaction after a calm spell.

## Notifications (D-059)
`toggle`, `pause`, `resume`, `stop` notify on success ("Screen sync on" — sent once setup worked —,
"Switched from screen to music sync", "Music sync paused/resumed/off") and on failure (main's error path:
"No light configured — run gowl setup", "Light unreachable: no LAN interface for it", "Screen capture
unavailable", "No gowl sync is running", …). `screen`/`music` notify once if they exit on an error (not a
usage error). Spawned as `notify-send -a gowl -t 2500 -h string:x-canonical-private-synchronous:gowl -- SUMMARY
[BODY]` by a double fork (never waited for; missing notify-send = silent). The sync loop spawns nothing else.
Config `notify = on|off` (default on).

## Status file (D-060)
`$XDG_RUNTIME_DIR/gowl/state` (directory mode 0700; the pidfile stays `$XDG_RUNTIME_DIR/gowl.pid`), one JSON line, atomic rename, written by screen/music/calibrate only on a
change: start, pause, resume, output gone/back, sound server down/back, a device rebound / moved / unreachable
/ recovered (`Streams::health_gen`). Removed on exit (after the pidfile) and by the panic hook; a leftover is
ignored unless the pidfile is locked by the pid it names. Schema:
`{"running":true,"pid":N,"mode":"screen|music|calibrate","paused":B,"fps":N,["reaction":S,"palette":S,]
"devices":[{"name":S,"ip":S,"iface":S,"ok":B}],"error":null|S}` or `{"running":false}`; a locked pidfile without
a matching file (older gowl, starting up) reads as running with `mode` from `/proc/PID/cmdline` (`unknown` if
none) and empty `devices`.

## Lost session (D-064)
Music: when the sound server can't be reached (logout, PipeWire restart) the light gets razer OFF ×2 at once
and no keepalives; razer ON again when the stream reopens; after 10 minutes unreachable `gowl music` exits
(status 1). Screen: the Wayland connection closing ends `gowl screen` with razer OFF ×2 (status 1).
A failed direct `gowl screen|music` (the systemd unit's restarts) notifies the same failure once per 10 minutes.

## Shutdown contract
On SIGINT/SIGTERM/SIGHUP/SIGQUIT or `stop` (or a panic, via a hook): send razer OFF to every streaming device (twice,
10 ms apart — UDP), remove pidfile, exit 0 within 100 ms. On `pause`: razer OFF, stop
capture requests (zero wakeups except signalfd), keep Wayland connection, free the capture buffer
(reallocated on resume; also while the output is gone, D-055). Pause and
output-lost send razer OFF twice too. Music mode on
`pause` closes the monitor stream (zero wakeups) and reopens it on `resume`. Both are
idempotent. A network change while paused rebinds silently; razer ON waits for `resume`.
