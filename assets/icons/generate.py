#!/usr/bin/env python3
"""Original fastcord artwork. Regenerate with Python 3 + Pillow; no downloads.

The paths below generate both editable SVGs and iced canvas commands. Only the
desktop exports/contact sheet need Pillow. --check verifies committed outputs.
"""
from __future__ import annotations

import argparse
import html
import io
import math
from pathlib import Path
import re
import struct
import subprocess

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
ICONS: dict[str, list[tuple[str, bool, bool]]] = {}


def path(d: str, fill: bool = False, red: bool = False):
    return (d, fill, red)


def circle(x: float, y: float, r: float, fill: bool = False):
    k = r * 0.55228475
    return path(
        f"M{x+r} {y} C{x+r} {y+k} {x+k} {y+r} {x} {y+r} "
        f"C{x-k} {y+r} {x-r} {y+k} {x-r} {y} "
        f"C{x-r} {y-k} {x-k} {y-r} {x} {y-r} "
        f"C{x+k} {y-r} {x+r} {y-k} {x+r} {y} Z", fill)


def box(x: float, y: float, w: float, h: float, r: float = 1.5, fill: bool = False):
    return path(f"M{x+r} {y} L{x+w-r} {y} Q{x+w} {y} {x+w} {y+r} "
                f"L{x+w} {y+h-r} Q{x+w} {y+h} {x+w-r} {y+h} "
                f"L{x+r} {y+h} Q{x} {y+h} {x} {y+h-r} "
                f"L{x} {y+r} Q{x} {y} {x+r} {y} Z", fill)


def add(name: str, *shapes):
    ICONS[name] = list(shapes)


slash = path("M3 3 L17 17", red=True)
add("brand", path("M3 7 Q3 3 7 3 L14 3 Q18 3 18 7 L18 12 Q18 16 14 16 L9 16 L5 19 L5 15", False),
    path("M11 5 L7 10 L10 10 L8 14 L14 8 L11 8 Z", True), path("M2 8 L5 8 M1 11 L4 11"))
add("mic", box(7, 2, 6, 10, 3), path("M4.5 9 Q4.5 15 10 15 Q15.5 15 15.5 9 M10 15 L10 18 M7 18 L13 18"))
add("mic-muted", *ICONS["mic"], slash)
add("headphones", path("M3.5 13 L3.5 10 Q3.5 3 10 3 Q16.5 3 16.5 10 L16.5 13"), box(3, 10, 4, 7, 1.5, True), box(13, 10, 4, 7, 1.5, True))
add("headphones-deafened", *ICONS["headphones"], slash)
add("profile", circle(10, 6, 3.2), path("M3.5 18 Q3.5 12 10 12 Q16.5 12 16.5 18"))
add("safety", path("M10 2 L16.5 4.5 L16.5 9 Q16.5 14 10 18 Q3.5 14 3.5 9 L3.5 4.5 Z"), path("M7 9.5 L9 11.5 L13 7.5"))
add("audio", path("M2.5 8 L6 8 L10 4.5 L10 15.5 L6 12 L2.5 12 Z", True), path("M13 6.5 Q16 10 13 13.5 M15.5 3.5 Q21 10 15.5 16.5"))
add("audio-muted", path("M2.5 8 L6 8 L10 4.5 L10 15.5 L6 12 L2.5 12 Z", True), path("M13 8 L17 12 M17 8 L13 12"))
add("exit", path("M9 3 L3 3 L3 17 L9 17 M8 10 L18 10 M14 6 L18 10 L14 14"))
add("camera", box(2, 5, 11, 10, 2), path("M13 8 L18 5.5 L18 14.5 L13 12 Z"))
add("camera-on", box(2, 5, 11, 10, 2, True), path("M14 8 L18 5.5 L18 14.5 L14 12 Z", True))
add("screen", box(2, 3, 16, 11), path("M10 14 L10 18 M6 18 L14 18"))
add("screen-sharing", *ICONS["screen"], path("M10 11 L10 6 M7.5 8.5 L10 6 L12.5 8.5"))
add("soundboard", path("M3 8 L3 12 M6.5 4 L6.5 16 M10 6.5 L10 13.5 M13.5 3 L13.5 17 M17 8 L17 12"))
add("hang-up", path("M2 11 Q10 3 18 11 L17 14 L13 12.5 L13 10.5 Q10 9 7 10.5 L7 12.5 L3 14 Z", True))
add("call", path("M4 2 L8 6 L6 9 Q8 13 11 14 L14 12 L18 16 L15 18 Q5 16 2 6 Z"))
add("signal", *(box(x, 18-h, 2.5, h, .5, True) for x, h in [(2, 4), (6.5, 8), (11, 12), (15.5, 16)]))
add("settings", circle(10, 10, 3), path("M7 3 L8 1.8 L12 1.8 L13 3 L13 4.5 L15 5.5 L16.5 5 L18.2 8 L17 9 L17 11 L18.2 12 L16.5 15 L15 14.5 L13 15.5 L13 17 L12 18.2 L8 18.2 L7 17 L7 15.5 L5 14.5 L3.5 15 L1.8 12 L3 11 L3 9 L1.8 8 L3.5 5 L5 5.5 L7 4.5 Z"))
add("chat", path("M5 3 L15 3 Q18 3 18 6 L18 12 Q18 15 15 15 L9 15 L4 18 L4 15 Q2 15 2 12 L2 6 Q2 3 5 3 Z"), path("M6 7 L14 7 M6 11 L11 11"))
add("inbox", path("M5 3 L15 3 L18 11 L18 16 Q18 17 17 17 L3 17 Q2 17 2 16 L2 11 Z M2 11 L7 11 L8 13 L12 13 L13 11 L18 11"))
add("discover", circle(10, 10, 8), path("M13.5 6.5 L11.5 11.5 L6.5 13.5 L8.5 8.5 Z"))
add("friends", circle(7, 6, 3), path("M1.5 17 Q1.5 12 7 12 Q12.5 12 12.5 17 M13 3 Q18 3 17 7 Q16.5 9 14 9 M14 12 Q18.5 12 18.5 17"))
add("group", circle(10, 6, 2.5), path("M5 17 Q5 12 10 12 Q15 12 15 17 M3.5 4 Q0 7 4 9 M3 12 Q1 13 1 16 M16.5 4 Q20 7 16 9 M17 12 Q19 13 19 16"))
add("channel", path("M7 2 L5 18 M15 2 L13 18 M3 7 L18 7 M2 13 L17 13"))
add("announcement", path("M3 7 L8 7 L16 3 L16 15 L8 11 L3 11 Z M5 11 L6 17 L9 17 L8 12 M19 7 L19 11"))
add("stage", circle(10, 5.5, 2.5), path("M5.5 13 Q5.5 9.5 10 9.5 Q14.5 9.5 14.5 13 M3 8 Q0 13 5 16 M17 8 Q20 13 15 16 M7 18 L13 18"))
add("thread", path("M7 2 L5.5 12 M14 2 L13 7 M3 5 L17 5 M2 10 L7 10"), box(8, 9, 10, 7, 1), path("M11 16 L11 19 L14 16"))
add("forum", box(2, 3, 16, 14), path("M6 7 L6 7.1 M9 7 L14 7 M6 11 L6 11.1 M9 11 L14 11 M6 15 L12 15"))
add("search", circle(8.5, 8.5, 5.5), path("M13 13 L18 18"))
add("pin", path("M7 2 L15 5 L13 7 L11.5 11 L13 14 L4 10 L7 9 L8.5 5 Z M8 12 L5 18"))
add("unpin", *ICONS["pin"], slash)
add("emoji", circle(10, 10, 8), circle(7, 8, .7, True), circle(13, 8, .7, True), path("M6 12 Q10 17 14 12"))
add("sticker", path("M5 2 L15 2 Q18 2 18 5 L18 11 L11 18 L5 18 Q2 18 2 15 L2 5 Q2 2 5 2 Z M18 11 L13 11 Q11 11 11 13 L11 18"), circle(6.5, 7, .7, True), circle(12.5, 7, .7, True), path("M6 10 Q8 12 10 10"))
add("gif", box(1.5, 4, 17, 12), path("M7 7 L4 7 L4 13 L7 13 L7 10 L6 10 M10 7 L10 13 M13 13 L13 7 L16 7 M13 10 L15.5 10"))
add("poll", box(2, 2, 16, 16), path("M6 14 L6 10 M10 14 L10 5 M14 14 L14 8"))
add("attach", path("M7 12 L12.5 6.5 Q15 4 16.5 6 Q18 7.5 16 9.5 L9 16.5 Q5 20 2.5 16.5 Q0 13.5 3.5 10 L11 2.5 Q13 0.5 15 2.5"))
add("image", box(2, 2, 16, 16), circle(7, 6.5, 1.5), path("M2 15 L7 10 L11 14 L14 11 L18 15"))
add("paste", box(4, 4, 13, 14), box(7, 2, 7, 4, 1, True), path("M8 10 L13 10 M8 14 L12 14"))
add("send", path("M2 3 L18 10 L2 17 L5 10 Z M5 10 L18 10"))
add("edit", path("M3 13 L13 3 Q14 2 15 3 L17 5 Q18 6 17 7 L7 17 L2 18 Z M11 5 L15 9 M3 13 L7 17"))
add("delete", path("M3 5 L17 5 M7 5 L7 2 L13 2 L13 5 M5 5 L6 18 L14 18 L15 5 M8 8 L8.5 15 M12 8 L11.5 15"))
add("reply", path("M8 4 L2 9 L8 14 L8 10 Q15 9 18 17 L18 12 Q17 5 8 7 Z"))
add("forward", path("M12 4 L18 9 L12 14 L12 10 Q5 9 2 17 L2 12 Q3 5 12 7 Z"))
add("copy", box(7, 7, 11, 11), path("M13 4 L13 2 L2 2 L2 13 L4 13"))
add("link", path("M8 6 L11 3 Q14 0 17 3 Q20 6 17 9 L14 12 M6 8 L3 11 Q0 14 3 17 Q6 20 9 17 L12 14 M7 13 L13 7"))
add("id", path("M5 5 L1 10 L5 15 M15 5 L19 10 L15 15 M12 3 L8 17"))
add("react", path("M17 11 Q16 18 9 18 Q1 18 1 10 Q1 2 9 2 Q12 2 13 3 M15 2 L15 8 M12 5 L18 5 M5 12 Q9 16 13 12"), circle(6, 8, .7, True), circle(11, 8, .7, True))
add("more", *(circle(x, 10, 1.4, True) for x in [3, 10, 17]))
add("close", path("M5 5 L15 15 M15 5 L5 15"))
add("check", path("M3 10 L8 15 L17 5"))
add("plus", path("M10 3 L10 17 M3 10 L17 10"))
add("minus", path("M3 10 L17 10"))
add("download", path("M10 2 L10 13 M6 9 L10 13 L14 9 M3 13 L3 18 L17 18 L17 13"))
add("upload", path("M10 14 L10 3 M6 7 L10 3 L14 7 M3 13 L3 18 L17 18 L17 13"))
add("file", path("M4 2 L11 2 L16 7 L16 18 L4 18 Z M11 2 L11 7 L16 7 M7 11 L13 11 M7 14 L12 14"))
add("folder", path("M2 5 L2 17 L18 17 L18 6 L10 6 L8 3 L2 3 Z"))
add("external", path("M11 2 L18 2 L18 9 M18 2 L9 11 M8 4 L3 4 L3 17 L16 17 L16 12"))
add("play", path("M5 3 L17 10 L5 17 Z", True))
add("pause", box(4, 3, 4, 14, .5, True), box(12, 3, 4, 14, .5, True))
add("stop", box(4, 4, 12, 12, 1, True))
add("record", circle(10, 10, 6, True))
add("refresh", path("M17 8 Q16 2 9 2 Q3 2 2 8 M17 3 L17 8 L12 8 M3 12 Q4 18 11 18 Q17 18 18 12 M3 17 L3 12 L8 12"))
add("fullscreen", path("M7 2 L2 2 L2 7 M13 2 L18 2 L18 7 M2 13 L2 18 L7 18 M18 13 L18 18 L13 18"))
add("restore", path("M2 7 L7 7 L7 2 M18 7 L13 7 L13 2 M7 18 L7 13 L2 13 M13 18 L13 13 L18 13"))
add("grid", *(box(x, y, 6, 6, 1) for y in [2, 12] for x in [2, 12]))
add("focus", box(2, 2, 16, 11), box(12, 15, 6, 3, .5, True), path("M2 17 L9 17"))
add("speaker", box(2, 2, 16, 16), circle(10, 7, 2.3), path("M5.5 15 Q5.5 11 10 11 Q14.5 11 14.5 15"))
add("eye", path("M1 10 Q10 -1 19 10 Q10 21 1 10 Z"), circle(10, 10, 3))
add("eye-off", *ICONS["eye"], slash)
add("bell", path("M4 14 L6 11 L6 7 Q6 3 10 3 Q14 3 14 7 L14 11 L16 14 Z M8 17 Q10 20 12 17 M10 2 L10 3"))
add("bell-muted", *ICONS["bell"], slash)
add("lock", box(4, 9, 12, 9), path("M6 9 L6 6 Q6 2 10 2 Q14 2 14 6 L14 9 M10 12 L10 15"))
add("unlock", box(4, 9, 12, 9), path("M6 9 L6 6 Q6 2 10 2 Q14 2 14 6 M10 12 L10 15"))
add("report", path("M4 18 L4 2 M4 3 Q8 0 12 3 Q15 5 18 3 L18 12 Q15 14 12 12 Q8 9 4 12"))
add("block", circle(10, 10, 8), path("M4.5 4.5 L15.5 15.5"))
add("warning", path("M10 2 L19 18 L1 18 Z M10 7 L10 11 M10 14 L10 14.2"))
add("info", circle(10, 10, 8), path("M10 9 L10 15"), circle(10, 5.5, .8, True))
add("help", circle(10, 10, 8), path("M7 7 Q7 3 11 5 Q15 7 11 10 L10 11"), circle(10, 14.5, .8, True))
add("keyboard", box(1, 4, 18, 12), path("M4 7 L4 7.2 M8 7 L8 7.2 M12 7 L12 7.2 M16 7 L16 7.2 M4 10 L4 10.2 M8 10 L8 10.2 M12 10 L12 10.2 M16 10 L16 10.2 M5 13 L15 13"))
add("noise-suppression", path("M2 8 L2 12 M5 4 L5 16 M8 7 L8 13 M11 5 L11 15 M14 8 L14 12 M17 9 L17 11 M3 18 L17 2"))
add("filter", path("M2 3 L18 3 L12 10 L12 16 L8 18 L8 10 Z"))
add("sort", path("M4 3 L4 17 M1 14 L4 17 L7 14 M10 4 L18 4 M10 9 L16 9 M10 14 L13 14"))
add("calendar", box(2, 4, 16, 14), path("M6 2 L6 6 M14 2 L14 6 M2 9 L18 9 M6 12 L6 12.2 M10 12 L10 12.2 M14 12 L14 12.2 M6 15 L6 15.2 M10 15 L10 15.2"))
add("invite", circle(7, 6, 3), path("M1.5 17 Q1.5 12 7 12 Q10 12 11 14 M15 9 L15 17 M11 13 L19 13"))
add("mark-read", path("M2 5 L9 5 M2 10 L7 10 M2 15 L7 15 M10 13 L13 16 L19 8"))
add("chevron-down", path("M5 7 L10 12 L15 7"))
add("chevron-up", path("M5 13 L10 8 L15 13"))
add("chevron-left", path("M12 5 L7 10 L12 15"))
add("chevron-right", path("M8 5 L13 10 L8 15"))

STATES = {
    "mic": ("Mic", "muted", False), "mic-muted": ("Mic", "muted", True),
    "headphones": ("Headphones", "deafened", False), "headphones-deafened": ("Headphones", "deafened", True),
    "camera": ("Camera", "on", False), "camera-on": ("Camera", "on", True),
    "screen": ("Screen", "sharing", False), "screen-sharing": ("Screen", "sharing", True),
}


def variant(name):
    if name in STATES:
        kind, field, value = STATES[name]
        return f"Icon::{kind} {{ {field}: {str(value).lower()} }}"
    return "Icon::" + "".join(word.title() for word in name.split("-"))


def commands(d):
    tokens = re.findall(r"[MLQCZ]|-?\d+(?:\.\d+)?(?:e[+-]?\d+)?", d)
    i = 0
    while i < len(tokens):
        op = tokens[i]
        count = {"M": 2, "L": 2, "Q": 4, "C": 6, "Z": 0}[op]
        yield op, [float(n) for n in tokens[i+1:i+1+count]]
        i += count + 1


def svg(shapes, color="currentColor"):
    parts = ['<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 20 20">']
    for d, fill, red in shapes:
        ink = "#f23f43" if red else color
        attrs = f'fill="{ink}"' if fill else f'fill="none" stroke="{ink}" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"'
        parts.append(f'  <path d="{d}" {attrs}/>')
    return "\n".join(parts + ["</svg>", ""])


def rust():
    lines = ["// Generated by assets/icons/generate.py. Edit the source paths there.",
             "use iced::{Point, Color};", "use iced::widget::canvas::{self, Path, Stroke, Style};",
             "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
             '#[allow(dead_code)] // Complete artwork catalog; controls use only the applicable glyphs.',
             "pub enum Icon {"]
    seen = set()
    for name in ICONS:
        if name in STATES:
            kind, field, _ = STATES[name]
            declaration = f"{kind} {{ {field}: bool }}"
        else:
            kind = variant(name)[6:]
            declaration = kind
        if kind not in seen:
            lines.append(f"    {declaration},")
            seen.add(kind)
    lines += ["}", "#[cfg(test)]", "pub const ALL: &[Icon] = &["]
    lines += [f"    {variant(name)}," for name in ICONS]
    lines += ["];", "#[cfg(debug_assertions)]", "pub const NAMED: &[(&str, Icon)] = &["]
    lines += [f'    ("{name}", {variant(name)}),' for name in ICONS]
    lines += ["];", "pub fn draw(icon: Icon, frame: &mut canvas::Frame, color: Color) {",
              "let stroke = |color| Stroke { style: Style::Solid(color), width: 1.7, line_cap: canvas::LineCap::Round, line_join: canvas::LineJoin::Round, ..Stroke::default() };",
              "match icon {"]
    methods = {"M": "move_to", "L": "line_to", "Q": "quadratic_curve_to", "C": "bezier_curve_to"}
    for name, shapes in ICONS.items():
        lines.append(f"{variant(name)} => {{")
        for d, fill, red in shapes:
            lines.append("let path = Path::new(|b| {")
            for op, nums in commands(d):
                if op == "Z":
                    lines.append("b.close();")
                else:
                    args = ", ".join(f"Point::new({round(x, 4)!r}, {round(y, 4)!r})" for x, y in zip(nums[::2], nums[1::2]))
                    lines.append(f"b.{methods[op]}({args});")
            ink = "crate::style::RED" if red else "color"
            lines.append("});")
            lines.append(f"frame.fill(&path, {ink});" if fill else f"frame.stroke(&path, stroke({ink}));")
        lines.append("}")
    lines += ["}", "}", ""]
    source = "\n".join(lines)
    # Match workspace formatting without changing unrelated files.
    rustfmt = Path.home() / ".cargo/bin/rustfmt.exe"
    command = str(rustfmt) if rustfmt.exists() else "rustfmt"
    return subprocess.run([command, "--edition", "2024", "--emit", "stdout"], input=source,
                          text=True, capture_output=True, check=True).stdout


def raster(shapes, size, ink="white"):
    """Rasterize these simple closed/open paths at 4x for desktop exports."""
    from PIL import Image, ImageDraw
    scale = size * 4 / 20
    im = Image.new("RGBA", (size * 4, size * 4))
    draw = ImageDraw.Draw(im)
    for d, fill, red in shapes:
        color = "#f23f43" if red else ink
        subpaths, points = [], []
        current = (0, 0)
        for op, nums in commands(d):
            if op == "M":
                if points:
                    subpaths.append(points)
                current = tuple(nums)
                points = [current]
            elif op == "L":
                current = tuple(nums)
                points.append(current)
            elif op in ("Q", "C"):
                start = current
                for step in range(1, 33):
                    t = step / 32
                    u = 1 - t
                    if op == "Q":
                        q = tuple(u*u*start[k] + 2*u*t*nums[k] + t*t*nums[k+2] for k in range(2))
                    else:
                        q = tuple(u**3*start[k] + 3*u*u*t*nums[k] + 3*u*t*t*nums[k+2] + t**3*nums[k+4] for k in range(2))
                    points.append(q)
                current = tuple(nums[-2:])
            elif op == "Z":
                points.append(points[0])
                current = points[0]
        if points:
            subpaths.append(points)
        for points in subpaths:
            pixels = [(round(x*scale), round(y*scale)) for x, y in points]
            if fill:
                draw.polygon(pixels, fill=color)
            else:
                width = max(1, round(1.7*scale))
                draw.line(pixels, fill=color, width=width, joint="curve")
                radius = width / 2
                for x, y in pixels:
                    draw.ellipse((x-radius, y-radius, x+radius, y+radius), fill=color)
    return im.resize((size, size), Image.Resampling.LANCZOS)


def png(im):
    buf = io.BytesIO()
    im.save(buf, format="PNG", optimize=True)
    return buf.getvalue()


def exports():
    from PIL import Image, ImageDraw, ImageFont
    outputs = {}
    # The master uses the exact same mark as the rail. Clear space is 18%.
    mark = raster(ICONS["brand"], 640)
    master = Image.new("RGBA", (1024, 1024))
    ImageDraw.Draw(master).rounded_rectangle((0, 0, 1023, 1023), radius=228, fill="#5865f2")
    master.alpha_composite(mark, (192, 176))
    outputs[HERE / "app/fastcord.svg"] = (
        '<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">\n'
        '<rect width="1024" height="1024" rx="228" fill="#5865f2"/>\n'
        '<g transform="translate(192 176) scale(32)">\n' +
        svg(ICONS["brand"], "#fff").split("\n", 1)[1].rsplit("</svg>", 1)[0] + '</g>\n</svg>\n').encode()
    for size in [16, 24, 32, 48, 64, 128, 256, 512, 1024]:
        outputs[HERE / f"app/fastcord-{size}.png"] = png(master.resize((size, size), Image.Resampling.LANCZOS))
    # PNG-in-ICO entries, including 16/24 px and the 256 px Explorer view.
    sizes = [16, 24, 32, 48, 64, 128, 256]
    header = struct.pack("<HHH", 0, 1, len(sizes))
    entries, payload = b"", b""
    for size in sizes:
        data = outputs[HERE / f"app/fastcord-{size}.png"]
        entries += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(data), 6 + 16*len(sizes) + len(payload))
        payload += data
    outputs[HERE / "app/fastcord.ico"] = header + entries + payload
    chunks = b""
    for tag, size in [(b"icp4", 16), (b"icp5", 32), (b"icp6", 64), (b"ic07", 128), (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024)]:
        data = outputs[HERE / f"app/fastcord-{size}.png"]
        chunks += tag + struct.pack(">I", len(data)+8) + data
    outputs[HERE / "app/fastcord.icns"] = b"icns" + struct.pack(">I", len(chunks)+8) + chunks
    # Review sheet is local, deterministic and contains no external fonts/assets.
    font = ImageFont.load_default(size=12)
    sheet = Image.new("RGB", (1120, 220 + math.ceil(len(ICONS)/8)*100), "#1e1f22")
    draw = ImageDraw.Draw(sheet)
    sheet.paste(master.resize((112, 112), Image.Resampling.LANCZOS), (30, 26), master.resize((112, 112), Image.Resampling.LANCZOS))
    draw.text((166, 48), "fastcord / icon system", font=ImageFont.load_default(size=30), fill="white")
    draw.text((168, 94), f"{len(ICONS)} original glyphs / 20 px grid / 1.7 px rounded stroke", font=font, fill="#949ba4")
    for index, (name, shapes) in enumerate(ICONS.items()):
        x, y = 20+(index % 8)*138, 182+(index // 8)*100
        draw.rounded_rectangle((x, y, x+128, y+88), radius=9, fill="#2b2d31")
        glyph = raster(shapes, 30, "#dbdee1")
        sheet.paste(glyph, (x+49, y+12), glyph)
        draw.text((x+64, y+58), name, anchor="mt", font=font, fill="#949ba4")
    outputs[HERE / "preview.png"] = png(sheet)
    return outputs


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    outputs = {HERE / f"ui/{name}.svg": svg(shapes).encode() for name, shapes in ICONS.items()}
    outputs[ROOT / "crates/fastcord-app/src/icons/catalog.rs"] = rust().replace("\r\n", "\n").encode()
    outputs.update(exports())
    cards = "\n".join(f'<figure>{svg(shapes)}<figcaption>{html.escape(name)}</figcaption></figure>' for name, shapes in ICONS.items())
    outputs[HERE / "index.html"] = ('''<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>fastcord icons</title>
<style>body{margin:40px;background:#1e1f22;color:#dbdee1;font:14px system-ui}header{display:flex;align-items:center;gap:24px;margin-bottom:36px}header img{width:96px}h1{margin:0 0 8px}p{color:#949ba4}main{display:grid;grid-template-columns:repeat(auto-fill,minmax(130px,1fr));gap:12px}figure{margin:0;padding:20px 4px;background:#2b2d31;border-radius:10px;text-align:center}figure svg{width:24px;height:24px}figcaption{margin-top:16px;color:#949ba4;font-size:12px}body.light{background:#f2f3f5;color:#232428}body.light figure{background:white}button{padding:8px 16px}</style>
<header><img src="app/fastcord-128.png" alt="fastcord"><div><h1>fastcord icons</h1><p>Original vectors / 20 px / rounded 1.7 px stroke</p><button onclick="document.body.classList.toggle('light')">Light / dark</button></div></header><main>''' + cards + "</main></html>\n").encode()
    stale = []
    for target, data in outputs.items():
        if args.check:
            if not target.exists() or target.read_bytes() != data:
                stale.append(str(target.relative_to(ROOT)))
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
    if stale:
        raise SystemExit("Stale icon outputs:\n" + "\n".join(stale))
    print(f"{'Verified' if args.check else 'Generated'} {len(ICONS)} glyphs and desktop icons ({len(outputs)} files).")


if __name__ == "__main__":
    main()
