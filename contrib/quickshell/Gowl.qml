pragma Singleton
import QtQuick
import Quickshell
import Quickshell.Io

// gowl — Govee LAN light sync (screen / music) — as a quickshell service.
//
// Cost: one `gowl watch` process. It sleeps in poll() and prints one JSON line per state change (sync started,
// paused, resumed, switched, a light unreachable or back, stopped); nothing here polls and no timer runs while
// it is up. If it exits (gowl missing or replaced, logout) it is started again after 1 s, doubling to 30 s,
// and the back-off resets with the next line. Commands go out with Quickshell.execDetached, so a sync started
// from the shell survives a shell reload, and gowl shows its own notification for each toggle/pause/stop.
//
// Use: put this file in a directory of singletons (e.g. services/) and read Gowl.running, Gowl.mode,
// Gowl.paused, Gowl.devices, Gowl.error; call Gowl.toggleScreen(), toggleMusic(), pause(), resume(), stop(),
// color("ff8800"), brightness(40). gowl is looked up on PATH, then in ~/.local/bin (or set Gowl.gowl).
//
// One state line (see gowl's README, "Status for bars"):
//   {"running":true,"pid":1234,"mode":"screen","paused":false,"fps":30,
//    "devices":[{"name":"desk","ip":"192.168.1.3","iface":"enp5s0","ok":true}],"error":null}
//   {"running":false}
Singleton {
    id: root

    // ── settings ──
    // Command name or absolute path. Empty = "gowl" on PATH, else ~/.local/bin/gowl: a shell started by systemd
    // (or from niri) usually has PATH=/usr/local/bin:/usr/bin, without ~/.local/bin.
    property string gowl: ""
    property bool enabled: true            // false = stop watching (nothing runs)
    readonly property string command: gowl !== "" ? gowl
        : _local ? Quickshell.env("HOME") + "/.local/bin/gowl" : "gowl"

    // ── state (from `gowl watch`) ──
    property var state: ({ running: false })
    readonly property bool available: _seen                       // gowl answered at least once
    readonly property bool running: state.running === true
    readonly property string mode: running ? (state.mode || "unknown") : ""   // screen | music | calibrate | unknown
    readonly property bool paused: running && state.paused === true
    readonly property var devices: running && state.devices ? state.devices : []   // [{name, ip, iface, ok}]
    readonly property string error: running ? (state.error || "") : _watchError
    readonly property bool screenOn: running && mode === "screen" && !paused
    readonly property bool musicOn: running && mode === "music" && !paused

    // ── actions (fire and forget; the watcher reports the result) ──
    // toggle: that mode running → stop; paused → resume; the other mode running → switch; nothing → start.
    function toggleScreen() { _run(["toggle", "screen"]) }
    function toggleMusic() { _run(["toggle", "music"]) }
    function pause() { _run(["pause"]) }
    function resume() { _run(["resume"]) }
    function stop() { _run(["stop"]) }
    function color(hex) { _run(["color", String(hex).replace(/^#/, "")]) }
    function brightness(pct) {
        _run(["brightness", String(Math.round(Math.max(0, Math.min(100, Number(pct) || 0))))])
    }

    function _run(args) { Quickshell.execDetached([root.command].concat(args)) }

    // ── watcher ──
    property bool _seen: false
    property bool _local: false            // trying ~/.local/bin/gowl (gowl: "" only)
    property string _watchError: ""
    property int _backoff: 1000

    function _apply(line) {
        let s
        try { s = JSON.parse(line) } catch (e) { return }
        if (s === null || typeof s !== "object") return
        state = s
        _seen = true
        _watchError = ""
        _backoff = 1000
    }

    function _start() { if (root.enabled && !watcher.running) watcher.running = true }

    onEnabledChanged: {
        if (enabled) _start()
        else { restart.stop(); watcher.running = false; state = { running: false } }
    }
    Component.onCompleted: _start()

    Process {
        id: watcher
        command: [root.command, "watch"]
        stdout: SplitParser { onRead: line => root._apply(line) }
        stderr: StdioCollector { id: watchErr }
        onRunningChanged: {
            if (running || !root.enabled) return
            if (root.state.running !== false) root.state = { running: false }
            root._watchError = watchErr.text.trim() || (root.gowl !== "" ? "can't run `" + root.gowl + " watch`"
                : "gowl not found on PATH or in ~/.local/bin")
            // Never answered: alternate between PATH and ~/.local/bin until one works.
            if (!root._seen && root.gowl === "") root._local = !root._local
            restart.interval = root._backoff
            root._backoff = Math.min(root._backoff * 2, 30000)
            restart.restart()
        }
    }
    Timer {
        id: restart
        onTriggered: root._start()
    }

    function dump() {
        return JSON.stringify({ available: available, running: running, mode: mode, paused: paused,
            devices: devices, error: error })
    }
}
