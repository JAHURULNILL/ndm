#!/usr/bin/env python3
"""Generate the NDM extension icons (icon16/48/128.png) with pure stdlib.

Note: the shipped icon set (app + extension) is rendered anti-aliased
by scripts/make_icons.py (PIL, supersampled); this stdlib version
remains as a no-dependency fallback producing the same mark.

Same mark as the app icon: dark rounded square with a blue down arrow
and a base bar. No PIL needed — the PNG is written with zlib + struct.
"""
import struct
import zlib


def make_icon(size):
    px = bytearray()
    cx = size / 2
    shaft_w = size * 0.16
    shaft_top, shaft_bot = size * 0.18, size * 0.58
    head_top, head_bot = size * 0.50, size * 0.80
    head_half = size * 0.30
    bar_y, bar_h = size * 0.86, size * 0.06
    rad = size * 0.18
    for y in range(size):
        px.append(0)  # PNG filter byte per row
        for x in range(size):
            r = g = b = 0
            a = 0  # transparent outside the rounded square
            dx = max(rad - x, 0, x - (size - rad - 1))
            dy = max(rad - y, 0, y - (size - rad - 1))
            if dx * dx + dy * dy <= rad * rad:
                r, g, b = 32, 34, 40  # dark charcoal square
                a = 255
            if a and abs(x - cx) <= shaft_w / 2 and shaft_top <= y <= shaft_bot:
                r, g, b = 64, 156, 255  # arrow shaft
            if a and head_top <= y <= head_bot:
                t = (y - head_top) / max(1, (head_bot - head_top))
                if abs(x - cx) <= head_half * (1 - t):
                    r, g, b = 64, 156, 255  # arrow head
            if a and abs(x - cx) <= head_half and bar_y <= y <= bar_y + bar_h:
                r, g, b = 64, 156, 255  # base bar
            px += bytes((r, g, b, a))
    return px


def chunk(typ, data):
    return (
        struct.pack(">I", len(data))
        + typ
        + data
        + struct.pack(">I", zlib.crc32(typ + data) & 0xFFFFFFFF)
    )


def write_png(path, size):
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(make_icon(size))))
        + chunk(b"IEND", b"")
    )
    with open(path, "wb") as f:
        f.write(png)
    print("wrote", path, size)


if __name__ == "__main__":
    import os

    here = os.path.dirname(os.path.abspath(__file__))
    for size in (16, 48, 128):
        write_png(os.path.join(here, f"icon{size}.png"), size)
