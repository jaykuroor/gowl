#!/usr/bin/env python3
"""Write full-screen test images (binary PPM) for checking screen-sync zone layout.

    python3 tools/test_pattern.py [--size 2560x1440] [--out /tmp/gowl-patterns]

Show one fullscreen with `imv -f FILE` or `mpv --fs --image-display-duration=inf FILE`
and check the lights against the description printed for each file. Stdlib only.
"""

from __future__ import annotations

import argparse
from pathlib import Path

RED, GREEN, BLUE = (255, 0, 0), (0, 255, 0), (0, 0, 255)
YELLOW, WHITE, BLACK = (255, 255, 0), (255, 255, 255), (0, 0, 0)


def runs(*parts: tuple[tuple[int, int, int], int]) -> bytes:
    """Build one row from (color, width) runs."""
    return b"".join(bytes(color) * width for color, width in parts)


def write_ppm(path: Path, w: int, h: int, row_for_y) -> None:
    cache: dict[int, bytes] = {}
    with path.open("wb") as f:
        f.write(f"P6\n{w} {h}\n255\n".encode())
        for y in range(h):
            key, builder = row_for_y(y)
            if key not in cache:
                cache[key] = builder()
                assert len(cache[key]) == 3 * w, (path.name, y)
            f.write(cache[key])


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--size", default="1920x1080")
    ap.add_argument("--out", default="/tmp/gowl-patterns")
    a = ap.parse_args()
    w, h = (int(v) for v in a.size.lower().split("x"))
    half_w = w // 2
    edge = max(1, w // 40)
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)

    def solid(color):
        return lambda y: (0, lambda: runs((color, w)))

    patterns = {
        "solid_red": ("every segment red", solid(RED)),
        "solid_white": ("every segment white", solid(WHITE)),
        "solid_black": ("every segment dark", solid(BLACK)),
        "top_blue_bottom_green": (
            "segments along the top blue, along the bottom green, sides split",
            lambda y: (y < h // 2, lambda top=y < h // 2: runs((BLUE if top else GREEN, w)))),
        "left_red_right_yellow": (
            "left side red, right side yellow, top/bottom split in the middle",
            lambda y: (0, lambda: runs((RED, half_w), (YELLOW, w - half_w)))),
        "quadrants": (
            "top-left red, top-right green, bottom-left blue, bottom-right yellow",
            lambda y: (y < h // 2, lambda top=y < h // 2:
                       runs((RED if top else BLUE, half_w), (GREEN if top else YELLOW, w - half_w)))),
        "thin_red_frame": (
            f"{edge}px red border on black: segments red-ish but dimmer (averaging works)",
            lambda y: (y < edge or y >= h - edge, lambda full=(y < edge or y >= h - edge):
                       runs((RED, w)) if full else runs((RED, edge), (BLACK, w - 2 * edge), (RED, edge)))),
    }
    for name, (desc, fn) in patterns.items():
        path = out / f"{name}.ppm"
        write_ppm(path, w, h, fn)
        print(f"{path}  ->  {desc}")


if __name__ == "__main__":
    main()
