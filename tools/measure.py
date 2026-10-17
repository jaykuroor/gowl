#!/usr/bin/env python3
"""Budget measurement harness for gowl (see docs/BUDGETS.md).

Reads /proc directly — no sysstat, no root. Measures gowl and niri over a window:
CPU % of one core, context switches/s (≈ wakeups), RSS/HWM, threads, mapped libs,
and — with --fake — datagrams and bytes the device received.

    python3 tools/measure.py screen 30                  # against the configured device
    python3 tools/measure.py screen 30 --fake           # against tools/fake_govee.py on 127.0.0.1
    python3 tools/measure.py music 30 --fake
    python3 tools/measure.py paused 20 --fake           # starts screen, then `gowl pause`
    python3 tools/measure.py baseline 30                # niri only, nothing running (S0)
    python3 tools/measure.py screen 30 -- --fps 45      # extra gowl args after --

Prints a human table and a markdown row for the BUDGETS.md results log.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CLK_TCK = os.sysconf("SC_CLK_TCK")
PAGE = os.sysconf("SC_PAGE_SIZE")


def find_pid(comm: str) -> int | None:
    for p in Path("/proc").iterdir():
        if p.name.isdigit():
            try:
                if (p / "comm").read_text().strip() == comm:
                    return int(p.name)
            except OSError:
                pass
    return None


def cpu_ticks(pid: int) -> int:
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return int(fields[11]) + int(fields[12])  # utime + stime


def ctx_switches(pid: int) -> int:
    total = 0
    for task in Path(f"/proc/{pid}/task").iterdir():
        try:
            for line in (task / "status").read_text().splitlines():
                if line.startswith(("voluntary_ctxt_switches", "nonvoluntary_ctxt_switches")):
                    total += int(line.split()[1])
        except OSError:
            pass
    return total


def status_kb(pid: int, key: str) -> int:
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith(key + ":"):
            return int(line.split()[1])
    return 0


def mapped_libs(pid: int) -> list[str]:
    libs = set()
    try:
        for line in Path(f"/proc/{pid}/maps").read_text().splitlines():
            parts = line.split()
            if len(parts) >= 6 and ".so" in parts[5]:
                libs.add(Path(parts[5]).name)
    except OSError:
        pass
    return sorted(libs)


class PacketCounter:
    def __init__(self, proc: subprocess.Popen) -> None:
        self.pkts = 0
        self.frame_bytes = 0
        self.invalid = 0
        self.lock = threading.Lock()
        threading.Thread(target=self._read, args=(proc,), daemon=True).start()

    def _read(self, proc: subprocess.Popen) -> None:
        for line in proc.stdout:
            try:
                ev = json.loads(line)
            except ValueError:
                continue
            if ev.get("port") != "control":
                continue
            with self.lock:
                self.pkts += 1
                self.frame_bytes += ev.get("size", 0)
                if not ev.get("valid", True):
                    self.invalid += 1

    def snapshot(self) -> tuple[int, int, int]:
        with self.lock:
            return self.pkts, self.frame_bytes, self.invalid


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mode", choices=["screen", "music", "paused", "baseline"])
    ap.add_argument("seconds", type=float)
    ap.add_argument("--bin", default=str(ROOT / "target/release/gowl"))
    ap.add_argument("--fake", action="store_true", help="run tools/fake_govee.py and target 127.0.0.1")
    ap.add_argument("--warmup", type=float, default=3.0)
    ap.add_argument("extra", nargs="*", help="extra gowl args (put after --)")
    a = ap.parse_args()

    niri = find_pid("niri")
    env = dict(os.environ)
    fake = counter = lumi = None

    try:
        if a.fake:
            fake = subprocess.Popen([sys.executable, str(ROOT / "tools/fake_govee.py"), "--json-log", "--quiet"],
                                    stdout=subprocess.PIPE, text=True)
            counter = PacketCounter(fake)
            env["GOWL_DEVICE"] = "127.0.0.1"
            time.sleep(0.3)

        if a.mode != "baseline":
            if not Path(a.bin).is_file():
                sys.exit(f"binary not found: {a.bin} (cargo build --release)")
            sub = "screen" if a.mode == "paused" else a.mode
            lumi = subprocess.Popen([a.bin, sub, *a.extra], env=env)
            time.sleep(a.warmup)
            if lumi.poll() is not None:
                sys.exit(f"gowl exited early with {lumi.returncode}")
            if a.mode == "paused":
                subprocess.run([a.bin, "pause"], env=env, check=False)
                time.sleep(1.0)

        pids = {"gowl": lumi.pid if lumi else None, "niri": niri}
        start = {n: (cpu_ticks(p), ctx_switches(p)) for n, p in pids.items() if p}
        pk0 = counter.snapshot() if counter else (0, 0, 0)
        t0 = time.monotonic()
        time.sleep(a.seconds)
        dt = time.monotonic() - t0
        end = {n: (cpu_ticks(p), ctx_switches(p)) for n, p in pids.items() if p}
        pk1 = counter.snapshot() if counter else (0, 0, 0)

        rows = {}
        for n in start:
            cpu = (end[n][0] - start[n][0]) / CLK_TCK / dt * 100
            cs = (end[n][1] - start[n][1]) / dt
            rows[n] = (cpu, cs)

        print(f"\n== {a.mode} for {dt:.1f}s ==")
        for n, (cpu, cs) in rows.items():
            print(f"{n:7s} CPU {cpu:6.3f}% of one core   ctx-switches {cs:7.1f}/s")
        rss = hwm = threads = 0
        if lumi:
            rss, hwm = status_kb(lumi.pid, "VmRSS"), status_kb(lumi.pid, "VmHWM")
            threads = status_kb(lumi.pid, "Threads")
            libs = mapped_libs(lumi.pid)
            print(f"gowl  RSS {rss/1024:.2f} MB  HWM {hwm/1024:.2f} MB  threads {threads}")
            print(f"gowl  mapped libs: {', '.join(libs)}")
            if a.mode != "music" and any("pulse" in l for l in libs):
                print("WARNING: libpulse mapped outside music mode (dlopen not lazy?)")
            if threads != 1:
                print("WARNING: gowl has more than one thread")
            print(f"binary  {Path(a.bin).stat().st_size/1024:.0f} KB")
        pkts_s = bytes_pkt = 0.0
        if counter:
            pkts = pk1[0] - pk0[0]
            pkts_s = pkts / dt
            bytes_pkt = (pk1[1] - pk0[1]) / max(1, pkts)
            print(f"device  {pkts_s:.2f} datagrams/s  ~{bytes_pkt:.0f} B/frame  invalid {pk1[2]-pk0[2]}")

        commit = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=ROOT,
                                capture_output=True, text=True).stdout.strip() or "-"
        lc = rows.get("gowl", (0, 0))
        nc = rows.get("niri", (0, 0))
        print("\nmarkdown row (niri column is absolute; subtract the S0 baseline for the Δ):")
        print(f"| {time.strftime('%Y-%m-%d')} | {commit} | {a.mode} {' '.join(a.extra)} | {lc[0]:.3f} | "
              f"{lc[1]:.1f} | {rss/1024:.2f} | {pkts_s:.2f} | {nc[0]:.2f} | fake={a.fake} |")
        return 0
    finally:
        if lumi and lumi.poll() is None:
            lumi.send_signal(signal.SIGTERM)
            try:
                lumi.wait(2)
            except subprocess.TimeoutExpired:
                lumi.kill()
                print("WARNING: gowl did not exit within 2 s of SIGTERM")
        if fake:
            fake.terminate()


if __name__ == "__main__":
    sys.exit(main())
