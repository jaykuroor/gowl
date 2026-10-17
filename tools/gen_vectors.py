#!/usr/bin/env python3
"""Generate golden test vectors from the legacy LumiSync Python code.

The new implementation must reproduce these outputs (exactly for protocol bytes,
within the stated tolerance for color/audio math). Re-run whenever the legacy
checkout changes:

    python3 -I tools/gen_vectors.py legacy tests/vectors

Needs only Python 3.11+ and numpy. Legacy modules are loaded by file path with
their heavy imports (webcolors, colorama, Qt, ...) stubbed out, so nothing from
the legacy tree's dependency list has to be installed.
"""

from __future__ import annotations

import base64
import importlib.util
import json
import math
import sys
import types
from pathlib import Path

import numpy as np


def load_legacy(root: Path):
    pkg_root = root / "lumisync"
    if not (pkg_root / "led_mapping.py").is_file():
        sys.exit(f"not a LumiSync checkout: {root}")

    # Stub third-party modules the selected files import at module level.
    for name in ("webcolors", "colorama"):
        stub = types.ModuleType(name)
        stub.Fore = types.SimpleNamespace(RED="", LIGHTGREEN_EX="", YELLOW="")
        sys.modules[name] = stub

    def package(name: str, path: Path):
        mod = types.ModuleType(name)
        mod.__path__ = [str(path)]
        sys.modules[name] = mod
        return mod

    def module(name: str, path: Path):
        spec = importlib.util.spec_from_file_location(name, path)
        mod = importlib.util.module_from_spec(spec)
        sys.modules[name] = mod
        spec.loader.exec_module(mod)
        parent, _, child = name.rpartition(".")
        setattr(sys.modules[parent], child, mod)
        return mod

    package("lumisync", pkg_root)
    package("lumisync.utils", pkg_root / "utils")
    package("lumisync.sync", pkg_root / "sync")
    led_mapping = module("lumisync.led_mapping", pkg_root / "led_mapping.py")
    colors = module("lumisync.utils.colors", pkg_root / "utils" / "colors.py")
    processing = module("lumisync.sync.processing", pkg_root / "sync" / "processing.py")

    # audio.py only needs numpy/colorsys at import time.
    audio = module("lumisync.sync.audio", pkg_root / "sync" / "audio.py")
    return led_mapping, colors, processing, audio


def xor_frame(frame: list[int]) -> list[int]:
    c = 0
    for b in frame:
        c ^= b
    return frame + [c]


def synthetic_frame(h: int, w: int) -> np.ndarray:
    """Deterministic RGB test image. Rust tests regenerate it with the same formula:
    r = (7x + 3y) mod 256, g = (2x + 5y) mod 256, b = (x*x + y) mod 256
    where x is the column and y the row."""
    y, x = np.mgrid[0:h, 0:w]
    r = (7 * x + 3 * y) % 256
    g = (2 * x + 5 * y) % 256
    b = (x * x + y) % 256
    return np.stack([r, g, b], axis=-1).astype(np.uint8)


def palette(n: int) -> list[tuple[int, int, int]]:
    """Deterministic segment colors: ((37i) mod 256, (91i+17) mod 256, (255 - 13i) mod 256)."""
    return [((37 * i) % 256, (91 * i + 17) % 256, (255 - 13 * i) % 256) for i in range(n)]


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    legacy_root, out_dir = Path(sys.argv[1]), Path(sys.argv[2])
    out_dir.mkdir(parents=True, exist_ok=True)
    led_mapping, colors, processing, audio = load_legacy(legacy_root)

    # ---------------------------------------------------------------- protocol
    razer_mode = {}
    for on in (0, 1):
        frame = xor_frame([0xBB, 0x00, 0x01, 0xB1, on])
        razer_mode["on" if on else "off"] = {
            "bytes_hex": bytes(frame).hex(),
            "pt": base64.b64encode(bytes(frame)).decode(),
        }

    segment_frames = []
    for n in (1, 4, 10, 14, 20, 55, 255):
        cols = palette(n)
        pt = colors.convert_colors(cols)
        segment_frames.append({
            "count": n,
            "colors": cols,
            "bytes_hex": base64.b64decode(pt).hex(),
            "pt": pt,
        })
    # Edge cases: all black, all white.
    for name, col in (("black", (0, 0, 0)), ("white", (255, 255, 255))):
        cols = [col] * 10
        pt = colors.convert_colors(cols)
        segment_frames.append({"count": 10, "name": name, "colors": cols,
                               "bytes_hex": base64.b64decode(pt).hex(), "pt": pt})

    def legacy_json(obj) -> str:
        # Exactly what legacy connection.send() puts on the wire.
        return json.dumps(obj)

    commands = {
        "scan": '{"msg":{"cmd":"scan","data":{"account_topic":"reserve"}}}',
        "turn_on": legacy_json({"msg": {"cmd": "turn", "data": {"value": 1}}}),
        "turn_off": legacy_json({"msg": {"cmd": "turn", "data": {"value": 0}}}),
        "brightness_42": legacy_json({"msg": {"cmd": "brightness", "data": {"value": 42}}}),
        "color_ff8000": legacy_json({"msg": {"cmd": "colorwc", "data": {
            "color": {"r": 255, "g": 128, "b": 0}, "colorTemInKelvin": 0}}}),
        "dev_status": legacy_json({"msg": {"cmd": "devStatus", "data": {}}}),
        "status": legacy_json({"msg": {"cmd": "status", "data": {}}}),
        "razer_on": legacy_json({"msg": {"cmd": "razer", "data": {"pt": razer_mode["on"]["pt"]}}}),
        "razer_frame_10": legacy_json({"msg": {"cmd": "razer", "data": {"pt": segment_frames[2]["pt"]}}}),
    }
    kelvin = {str(k): list(colors.kelvin_to_rgb(k)) for k in (2000, 2700, 4000, 5500, 6500, 9000)}

    (out_dir / "protocol.json").write_text(json.dumps({
        "_note": "Byte-exact. JSON whitespace on the wire may differ (devices accept compact JSON); 'pt' payloads must match exactly.",
        "ports": {"scan_dst": 4001, "reply_listen": 4002, "control_dst": 4003, "multicast": "239.255.255.250"},
        "razer_mode": razer_mode,
        "segment_frames": segment_frames,
        "legacy_commands": commands,
        "kelvin_to_rgb": kelvin,
    }, indent=1))

    # ------------------------------------------------------------- led mapping
    mappings = []
    for n in (1, 2, 3, 4, 10, 14, 20, 55):
        for aspect_name, aspect in (("16:9", 16 / 9), ("21:9", 21 / 9), ("32:9", 32 / 9)):
            for depth in (0.10, 0.28, 0.50):
                mappings.append({
                    "zones": n, "aspect": aspect_name, "aspect_ratio": aspect, "depth": depth,
                    "rects": led_mapping.generate_screen_mapping(n, aspect, depth),
                })
    (out_dir / "led_mapping.json").write_text(json.dumps({
        "_note": "Normalized rects {x,y,w,h} in 0..1, zone order = legacy order (top right->left, left top->bottom, bottom left->right, right bottom->top). Tolerance 1e-9.",
        "cases": mappings,
    }, indent=1))

    # -------------------------------------------------------------- processing
    zone_cases = []
    for (h, w) in ((90, 160), (1080, 1920), (1440, 3440)):
        frame = synthetic_frame(h, w)
        for n in (10, 14):
            rects = led_mapping.generate_screen_mapping(n, w / h)
            for gamma in (True, False):
                zone_cases.append({
                    "height": h, "width": w, "zones": n, "gamma_correct": gamma,
                    "colors": processing.average_zone_colors(frame, rects, gamma_correct=gamma),
                })

    sat_in = palette(12) + [(128, 128, 128), (255, 0, 0), (10, 200, 30)]
    smoother = processing.ColorSmoother(0.55, 3)
    smooth_seq_in = [[(0, 0, 0), (255, 255, 255), (10, 20, 30)],
                     [(255, 0, 0), (0, 0, 0), (40, 50, 60)],
                     [(255, 0, 0), (0, 0, 0), (40, 50, 60)],
                     [(0, 255, 0), (128, 128, 128), (0, 0, 0)]]
    smooth_seq_out = [smoother.update(f) for f in smooth_seq_in]

    (out_dir / "processing.json").write_text(json.dumps({
        "_note": "Synthetic frame formula in gen_vectors.synthetic_frame(). Integer/fixed-point ports may differ by at most 1 per channel.",
        "frame_formula": "r=(7x+3y)%256 g=(2x+5y)%256 b=(x*x+y)%256 ; x=column y=row",
        "max_samples_per_axis": 24,
        "average_zone_colors": zone_cases,
        "saturation_1_15": {"in": sat_in, "out": processing.apply_saturation(sat_in, 1.15)},
        "brightness_0_75": {"in": sat_in, "out": processing.apply_brightness(sat_in, 0.75)},
        "resample": [{"in": palette(10), "count": c, "out": colors.resample_colors_to_count(palette(10), c)}
                     for c in (4, 10, 14, 20)],
        "smoother_alpha_0_55": {"in": smooth_seq_in, "out": smooth_seq_out,
                                "note": "first frame snaps to target"},
        "frame_needs_send": [
            {"prev": None, "cur": [[1, 1, 1]], "threshold": 3, "last_sent_at": None, "now": 0.0,
             "expect": processing.frame_needs_send(None, [(1, 1, 1)], 3, last_sent_at=None, now=0.0)},
            {"prev": [[1, 1, 1]], "cur": [[3, 3, 3]], "threshold": 3, "last_sent_at": 0.0, "now": 0.1,
             "expect": processing.frame_needs_send([(1, 1, 1)], [(3, 3, 3)], 3, last_sent_at=0.0, now=0.1)},
            {"prev": [[1, 1, 1]], "cur": [[5, 1, 1]], "threshold": 3, "last_sent_at": 0.0, "now": 0.1,
             "expect": processing.frame_needs_send([(1, 1, 1)], [(5, 1, 1)], 3, last_sent_at=0.0, now=0.1)},
            {"prev": [[1, 1, 1]], "cur": [[1, 1, 1]], "threshold": 3, "last_sent_at": 0.0, "now": 0.6,
             "expect": processing.frame_needs_send([(1, 1, 1)], [(1, 1, 1)], 3, last_sent_at=0.0, now=0.6)},
        ],
    }, indent=1))

    # ------------------------------------------------------------------- audio
    sr = 48000
    audio_cases = []
    for n in (1024, 1104):
        t = np.arange(n) / sr
        signals = {
            "silence": np.zeros(n),
            "sine_100hz": 0.5 * np.sin(2 * math.pi * 100 * t),
            "sine_1khz": 0.5 * np.sin(2 * math.pi * 1000 * t),
            "sine_8khz": 0.5 * np.sin(2 * math.pi * 8000 * t),
            "mix": 0.3 * np.sin(2 * math.pi * 60 * t) + 0.2 * np.sin(2 * math.pi * 2000 * t)
                   + 0.1 * np.sin(2 * math.pi * 10000 * t),
        }
        for name, sig in signals.items():
            audio_cases.append({"n": n, "signal": name,
                                "bands": list(audio.spectral_bands(sig.astype(np.float32), sr))})

    gain = audio.AutoGain(60)
    gain_in = [(0.2, 0.1, 0.05)] * 5 + [(0.02, 0.01, 0.005)] * 5 + [(0.0, 0.0, 0.0)] + [(1.0, 0.5, 0.2)] * 3
    gain_out = [list(gain.process(b)) for b in gain_in]

    (out_dir / "audio.json").write_text(json.dumps({
        "_note": "Hann window, rfft, magnitude * 2/sum(window), band RMS. Bands: bass 20-250, mid 250-4000, treble 4000-min(16000,nyquist) Hz, [low, high). Relative tolerance 1e-3 (abs 1e-6 near zero).",
        "sample_rate": sr,
        "signals": "0.5*sin(2*pi*f*t) etc; see gen_vectors.py",
        "spectral_bands": audio_cases,
        "auto_gain_fps60": {"in": gain_in, "out": gain_out,
                            "params": {"target": 0.10, "max_boost": 40.0, "attack_s": 0.15, "release_s": 1.0}},
    }, indent=1))

    print(f"wrote vectors to {out_dir}")


if __name__ == "__main__":
    main()
