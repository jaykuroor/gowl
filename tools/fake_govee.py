#!/usr/bin/env python3
"""Fake Govee LAN device for hardware-free testing of gowl.

Speaks the protocol in docs/PROTOCOL.md: answers scans and status queries, accepts
turn/brightness/colorwc/razer commands, validates razer frames byte-by-byte, and
prints live stats (packets/s, bytes/s, invalid frames) plus the current segment
colors as truecolor blocks.

    python3 tools/fake_govee.py                       # real ports 4001/4003, replies to 4002
    python3 tools/fake_govee.py --port-base 14000     # test ports 14001/14003, replies to 14002
    python3 tools/fake_govee.py --json-log --quiet    # one JSON line per datagram (for tests)
    python3 tools/fake_govee.py --razer-timeout 1.0   # drop out of razer mode if starved
    python3 tools/fake_govee.py --self-test

Point gowl at it with GOWL_DEVICE=127.0.0.1 (or the --ip you pass).
Stdlib only.
"""

from __future__ import annotations

import argparse
import base64
import json
import selectors
import socket
import struct
import sys
import threading
import time

MULTICAST = "239.255.255.250"


class FakeDevice:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.scan_port = args.port_base + 1
        self.reply_port = args.port_base + 2
        self.control_port = args.port_base + 3
        self.on = 1
        self.brightness = 100
        self.color = (255, 255, 255)
        self.kelvin = 0
        self.razer = False
        self.last_frame_at = 0.0
        self.segments: list[tuple[int, int, int]] = [(0, 0, 0)] * args.segments
        self.stats = {"pkts": 0, "bytes": 0, "frames": 0, "invalid": 0, "frames_while_off": 0}
        self.totals = dict(self.stats)
        self.stop = threading.Event()

        self.scan_sock = self._udp(args.bind, self.scan_port, multicast=args.port_base == 4000)
        self.ctrl_sock = self._udp(args.bind, self.control_port)
        self.reply_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

    # ------------------------------------------------------------------ sockets
    @staticmethod
    def _udp(bind: str, port: int, multicast: bool = False) -> socket.socket:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        if hasattr(socket, "SO_REUSEPORT"):
            s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
        s.bind((bind, port))
        if multicast:
            try:
                mreq = struct.pack("4s4s", socket.inet_aton(MULTICAST), socket.inet_aton("0.0.0.0"))
                s.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, mreq)
            except OSError as exc:
                print(f"[fake] multicast join failed ({exc}); unicast/broadcast scans still work",
                      file=sys.stderr)
        s.setblocking(False)
        return s

    def reply(self, ip: str, payload: dict) -> None:
        self.reply_sock.sendto(json.dumps(payload, separators=(",", ":")).encode(), (ip, self.reply_port))

    # ------------------------------------------------------------------ logging
    def log(self, **fields) -> None:
        if self.args.json_log:
            fields["t"] = round(time.monotonic(), 6)
            print(json.dumps(fields, separators=(",", ":")), flush=True)

    def warn(self, msg: str) -> None:
        if not self.args.quiet:
            print(f"[fake] {msg}", file=sys.stderr, flush=True)

    # ------------------------------------------------------------------ handlers
    def on_scan(self, data: bytes, addr) -> None:
        try:
            msg = json.loads(data)
            ok = msg["msg"]["cmd"] == "scan"
        except (ValueError, KeyError, TypeError):
            ok = False
        self.log(port="scan", src=addr[0], cmd="scan", valid=ok)
        if not ok:
            self.warn(f"bad scan datagram from {addr}: {data[:80]!r}")
            return
        self.reply(addr[0], {"msg": {"cmd": "scan", "data": {
            "ip": self.args.ip, "device": self.args.mac, "sku": self.args.sku,
            "bleVersionHard": "3.01.01", "bleVersionSoft": "1.03.01",
            "wifiVersionHard": "1.00.10", "wifiVersionSoft": "1.02.03"}}})

    def on_control(self, data: bytes, addr) -> None:
        self.stats["pkts"] += 1
        self.stats["bytes"] += len(data)
        try:
            msg = json.loads(data)["msg"]
            cmd, body = msg["cmd"], msg.get("data", {})
        except (ValueError, KeyError, TypeError):
            self.stats["invalid"] += 1
            self.log(port="control", src=addr[0], cmd=None, valid=False, len=len(data))
            self.warn(f"unparseable control datagram: {data[:80]!r}")
            return

        if cmd == "turn":
            self.on = int(body["value"])
            self.log(port="control", cmd=cmd, valid=self.on in (0, 1), value=self.on)
        elif cmd == "brightness":
            v = int(body["value"])
            self.brightness = v
            self.log(port="control", cmd=cmd, valid=0 <= v <= 100, value=v)
        elif cmd == "colorwc":
            c = body["color"]
            self.color = (int(c["r"]), int(c["g"]), int(c["b"]))
            self.kelvin = int(body.get("colorTemInKelvin", 0))
            self.razer = False
            self.log(port="control", cmd=cmd, valid=True, color=self.color, kelvin=self.kelvin)
        elif cmd in ("devStatus", "status"):
            self.log(port="control", cmd=cmd, valid=True)
            self.reply(addr[0], {"msg": {"cmd": "devStatus", "data": {
                "onOff": self.on, "brightness": self.brightness,
                "color": {"r": self.color[0], "g": self.color[1], "b": self.color[2]},
                "colorTemInKelvin": self.kelvin}}})
        elif cmd == "razer":
            self.on_razer(body.get("pt", ""), len(data))
        else:
            self.stats["invalid"] += 1
            self.log(port="control", cmd=cmd, valid=False, reason="unknown cmd")
            self.warn(f"unknown cmd {cmd!r}")

    def on_razer(self, pt: str, size: int) -> None:
        try:
            raw = base64.b64decode(pt, validate=True)
        except (ValueError, TypeError):
            return self._bad_razer("base64", pt)
        if len(raw) < 6 or raw[0] != 0xBB:
            return self._bad_razer("header", raw.hex())
        xor = 0
        for b in raw[:-1]:
            xor ^= b
        if xor != raw[-1]:
            return self._bad_razer(f"checksum want {xor:02x} got {raw[-1]:02x}", raw.hex())

        kind = raw[3]
        if kind == 0xB1:
            if raw[1:3] != b"\x00\x01" or len(raw) != 6:
                return self._bad_razer("B1 length", raw.hex())
            self.razer = bool(raw[4])
            self.last_frame_at = time.monotonic()
            self.log(port="control", cmd="razer_mode", valid=True, on=self.razer)
            self.warn(f"razer mode {'ON' if self.razer else 'OFF'}")
            return
        if kind != 0xB0:
            return self._bad_razer(f"unknown kind {kind:02x}", raw.hex())

        length = (raw[1] << 8) | raw[2]
        n = raw[5]
        if raw[4] != 0x01:
            self.warn(f"flag byte is {raw[4]:02x}, legacy always sends 01")
        if length != 2 + 3 * n or len(raw) != 7 + 3 * n:
            return self._bad_razer(f"length field {length} for n={n}, frame {len(raw)} bytes", raw.hex())
        colors = [tuple(raw[6 + 3 * i: 9 + 3 * i]) for i in range(n)]
        if n != self.args.segments:
            self.warn(f"frame has {n} segments, device configured with {self.args.segments}")
        if not self.razer:
            self.stats["frames_while_off"] += 1
        self.stats["frames"] += 1
        self.segments = colors
        self.last_frame_at = time.monotonic()
        self.log(port="control", cmd="razer_frame", valid=True, n=n, colors=colors,
                 razer_on=self.razer, size=size)

    def _bad_razer(self, reason: str, detail: str) -> None:
        self.stats["invalid"] += 1
        self.log(port="control", cmd="razer", valid=False, reason=reason)
        self.warn(f"invalid razer frame ({reason}): {detail[:120]}")

    # ------------------------------------------------------------------ loop
    def status_line(self) -> str:
        blocks = "".join(f"\x1b[48;2;{r};{g};{b}m  \x1b[0m" for r, g, b in self.segments)
        s = self.stats
        mode = "RAZER" if self.razer else ("on" if self.on else "off")
        return (f"{s['pkts']:3d} pkt/s {s['bytes']:5d} B/s frames {s['frames']:3d} "
                f"invalid {s['invalid']} off-frames {s['frames_while_off']} "
                f"[{mode} bri {self.brightness}] {blocks}")

    def run(self) -> None:
        sel = selectors.DefaultSelector()
        sel.register(self.scan_sock, selectors.EVENT_READ, self.on_scan)
        sel.register(self.ctrl_sock, selectors.EVENT_READ, self.on_control)
        if not self.args.quiet:
            print(f"[fake] {self.args.sku} {self.args.mac} {self.args.segments} seg — scan :{self.scan_port}, "
                  f"control :{self.control_port}, replies → :{self.reply_port}", file=sys.stderr)
        next_tick = time.monotonic() + 1.0
        while not self.stop.is_set():
            timeout = max(0.0, next_tick - time.monotonic())
            for key, _ in sel.select(timeout):
                try:
                    data, addr = key.fileobj.recvfrom(65535)
                except BlockingIOError:
                    continue
                key.data(data, addr)
            now = time.monotonic()
            if (self.args.razer_timeout and self.razer
                    and now - self.last_frame_at > self.args.razer_timeout):
                self.razer = False
                self.log(cmd="razer_timeout", valid=True)
                self.warn(f"razer mode timed out after {self.args.razer_timeout}s without frames")
            if now >= next_tick:
                next_tick += 1.0
                if not self.args.quiet and not self.args.json_log:
                    print("\r\x1b[2K" + self.status_line(), end="", file=sys.stderr, flush=True)
                for k in self.stats:
                    self.totals[k] += self.stats[k]
                    self.stats[k] = 0
        sel.close()


def parse_args(argv=None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--bind", default="0.0.0.0")
    p.add_argument("--ip", default="127.0.0.1", help="IP reported in scan replies")
    p.add_argument("--port-base", type=int, default=4000, help="scan=base+1 reply=base+2 control=base+3")
    p.add_argument("--segments", type=int, default=10)
    p.add_argument("--sku", default="H619C")
    p.add_argument("--mac", default="FA:CE:00:00:00:00:00:01")
    p.add_argument("--razer-timeout", type=float, default=0.0, help="seconds; 0 = never")
    p.add_argument("--json-log", action="store_true")
    p.add_argument("--quiet", action="store_true")
    p.add_argument("--self-test", action="store_true")
    return p.parse_args(argv)


def self_test() -> int:
    args = parse_args(["--port-base", "15000", "--quiet", "--bind", "127.0.0.1"])
    dev = FakeDevice(args)
    t = threading.Thread(target=dev.run, daemon=True)
    t.start()

    client = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    client.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    client.bind(("127.0.0.1", 15002))
    client.settimeout(1.0)
    ctrl = ("127.0.0.1", 15003)

    def send(obj):
        client.sendto(json.dumps(obj).encode(), ctrl)

    failures = []
    client.sendto(b'{"msg":{"cmd":"scan","data":{"account_topic":"reserve"}}}', ("127.0.0.1", 15001))
    reply = json.loads(client.recvfrom(4096)[0])
    if reply["msg"]["data"]["sku"] != "H619C":
        failures.append("scan reply")

    send({"msg": {"cmd": "razer", "data": {"pt": "uwABsQEK"}}})           # razer ON
    pt = "uwAgsAEKABH/JWzySsflbyLYlH3Ludi+3jOxA46kKOmXTUSK1Q=="             # 10-seg vector
    send({"msg": {"cmd": "razer", "data": {"pt": pt}}})
    bad = bytearray(base64.b64decode(pt))
    bad[-1] ^= 0xFF
    send({"msg": {"cmd": "razer", "data": {"pt": base64.b64encode(bytes(bad)).decode()}}})
    send({"msg": {"cmd": "brightness", "data": {"value": 42}}})
    send({"msg": {"cmd": "devStatus", "data": {}}})
    status = json.loads(client.recvfrom(4096)[0])["msg"]["data"]
    time.sleep(0.05)
    dev.stop.set()
    t.join(2)
    totals = {k: dev.totals[k] + dev.stats[k] for k in dev.stats}

    if status["brightness"] != 42:
        failures.append("status brightness")
    if not dev.razer:
        failures.append("razer mode not on")
    if dev.segments[1] != (37, 108, 242):
        failures.append(f"segment colors {dev.segments[:2]}")
    if totals["frames"] != 1 or totals["invalid"] != 1:
        failures.append(f"counters {totals}")
    print("self-test:", "OK" if not failures else "FAIL " + ", ".join(failures))
    return 1 if failures else 0


def main() -> int:
    args = parse_args()
    if args.self_test:
        return self_test()
    dev = FakeDevice(args)
    try:
        dev.run()
    except KeyboardInterrupt:
        print(file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
