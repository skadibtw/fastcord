#!/usr/bin/env python3
"""Generates the bundled camera background presets (issue #85).

Every picture is computed from formulas in this file, so it is original work
with no third-party artwork: gradients, soft light spots and a stripe pattern.
Output: one raw NV12 file per preset, 320x180, BT.709 limited range (the
camera pipeline's default), tightly packed (Y plane then interleaved UV).
The runtime scales them to the camera picture size.

    python assets/camera/generate_presets.py

Deterministic: the same script always writes the same bytes.
"""
import math
import os
import random

W, H = 320, 180
OUT = os.path.dirname(os.path.abspath(__file__))


def clamp(v):
    return max(0.0, min(1.0, v))


def mix(a, b, t):
    return tuple(x + (y - x) * t for x, y in zip(a, b))


def spots(seed, count, rmin, rmax):
    rnd = random.Random(seed)
    return [
        (rnd.uniform(0, W), rnd.uniform(0, H), rnd.uniform(rmin, rmax), rnd.uniform(0.10, 0.28))
        for _ in range(count)
    ]


def dusk(x, y):
    # Warm vertical gradient with soft light spots.
    c = mix((0.16, 0.12, 0.30), (0.95, 0.55, 0.40), y / H)
    return lift(c, x, y, spots(1, 14, 8, 26), (1.0, 0.85, 0.65))


def mint(x, y):
    # Cool diagonal gradient with soft light spots.
    c = mix((0.05, 0.28, 0.32), (0.55, 0.88, 0.78), (x / W + y / H) / 2)
    return lift(c, x, y, spots(2, 12, 10, 30), (0.9, 1.0, 0.95))


def slate(x, y):
    # Neutral dark gradient with a vignette.
    d = math.hypot((x - W / 2) / (W / 2), (y - H / 2) / (H / 2))
    base = 0.30 - 0.10 * clamp(d - 0.4)
    return (base, base + 0.01, base + 0.04)


def studio(x, y):
    # Light neutral wall with faint diagonal stripes.
    stripe = 0.5 + 0.5 * math.sin((x + y) * 0.22)
    base = 0.80 + 0.04 * stripe - 0.06 * (y / H)
    return (base, base - 0.005, base - 0.02)


def lift(color, x, y, lights, tint):
    out = list(color)
    for cx, cy, r, strength in lights:
        d = math.hypot(x - cx, y - cy) / r
        if d < 1.4:
            a = strength * clamp(1.4 - d) ** 1.5
            out = [o + (t - o) * a for o, t in zip(out, tint)]
    return tuple(out)


def to_yuv(rgb):
    r, g, b = (clamp(c) for c in rgb)
    y = 0.2126 * r + 0.7152 * g + 0.0722 * b
    u = (b - y) / 1.8556
    v = (r - y) / 1.5748
    return 16 + 219 * y, 128 + 224 * u, 128 + 224 * v


def render(name, fn):
    px = [[to_yuv(fn(x + 0.5, y + 0.5)) for x in range(W)] for y in range(H)]
    luma = bytearray(round(px[y][x][0]) for y in range(H) for x in range(W))
    chroma = bytearray()
    for y in range(0, H, 2):
        for x in range(0, W, 2):
            block = [px[y + dy][x + dx] for dy in (0, 1) for dx in (0, 1)]
            chroma.append(round(sum(p[1] for p in block) / 4))
            chroma.append(round(sum(p[2] for p in block) / 4))
    with open(os.path.join(OUT, name + ".nv12"), "wb", newline=None) as f:
        f.write(bytes(luma) + bytes(chroma))


for name, fn in (("dusk", dusk), ("mint", mint), ("slate", slate), ("studio", studio)):
    render(name, fn)
