#!/usr/bin/env python3
"""Generate the mokuro-bunko desktop icons (original artwork, MPL-2.0 like the project).

One design: a white open book (bunko = paperback) on a rounded teal square, the accent
colour of the web UI (web/_static/shared.css --accent-primary-dark). The tray states add
a badge in the lower right corner:

    idle       no badge
    working    blue circle with a "play" triangle
    paused     amber circle with two bars
    attention  red circle with "!"

Outputs (all committed; re-run this script after changing the artwork):

    svg/mokuro-bunko.svg, svg/tray-<state>.svg      the sources
    png/mokuro-bunko-<N>.png                        app icon, N in APP_SIZES
    png/tray-<state>-<N>.png                        tray icons, N in TRAY_SIZES
    mokuro-bunko.ico                                Windows (16..256, PNG-compressed entries)
    mokuro-bunko.icns                               macOS bundle icon (16..1024)
    hicolor/<N>x<N>/apps/mokuro-bunko.png           freedesktop icon theme layout
    hicolor/scalable/apps/mokuro-bunko.svg

Needs only Python 3 and `rsvg-convert` (librsvg) for rasterizing.

    python3 packaging/icons/generate.py
"""

from __future__ import annotations

import pathlib
import shutil
import struct
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent

BG = "#3d8577"  # shared.css --accent-primary-dark
BG_EDGE = "#2d6a5f"
PAGE = "#ffffff"
SPINE = "#d9ece8"
BADGES = {
    "working": "#2563eb",
    "paused": "#d97706",
    "attention": "#dc2626",
}
STATES = ["idle", "working", "paused", "attention"]
APP_SIZES = [16, 22, 24, 32, 48, 64, 128, 256, 512, 1024]
TRAY_SIZES = [16, 22, 24, 32, 44, 48, 64]
HICOLOR_SIZES = [16, 22, 24, 32, 48, 64, 128, 256, 512]
ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 256]


def book(scale: float = 1.0, dx: float = 0.0, dy: float = 0.0) -> str:
    """An open book in a 32x32 box: two curved pages meeting at the spine."""

    def p(x: float, y: float) -> str:
        return f"{x * scale + dx:.3f} {y * scale + dy:.3f}"

    left = (
        f"M{p(15.3, 10.2)} C{p(12.6, 8.2)} {p(9.0, 7.6)} {p(5.6, 8.3)} "
        f"L{p(5.6, 23.4)} C{p(9.0, 22.8)} {p(12.6, 23.3)} {p(15.3, 25.0)} Z"
    )
    right = (
        f"M{p(16.7, 10.2)} C{p(19.4, 8.2)} {p(23.0, 7.6)} {p(26.4, 8.3)} "
        f"L{p(26.4, 23.4)} C{p(23.0, 22.8)} {p(19.4, 23.3)} {p(16.7, 25.0)} Z"
    )
    # A few text lines on the right page: reads as "a book you can read", gone at 16 px.
    lines = "".join(
        f'<path d="M{p(18.6, y)} C{p(20.4, y - 1.0)} {p(22.4, y - 1.3)} {p(24.4, y - 1.1)}" '
        f'stroke="{SPINE}" stroke-width="{0.9 * scale:.3f}" stroke-linecap="round" fill="none"/>'
        for y in (12.6, 15.6, 18.6)
    )
    return f'<path d="{left}" fill="{PAGE}"/><path d="{right}" fill="{PAGE}"/>{lines}'


def badge(state: str) -> str:
    color = BADGES.get(state)
    if not color:
        return ""
    cx, cy, r = 24.2, 24.2, 7.0
    ring = f'<circle cx="{cx}" cy="{cy}" r="{r + 1.4}" fill="{BG}"/>'
    disc = f'<circle cx="{cx}" cy="{cy}" r="{r}" fill="{color}"/>'
    if state == "working":
        mark = f'<path d="M{cx - 2.2} {cy - 3.6} L{cx + 3.8} {cy} L{cx - 2.2} {cy + 3.6} Z" fill="#fff"/>'
    elif state == "paused":
        mark = (
            f'<rect x="{cx - 3.2}" y="{cy - 3.6}" width="2.3" height="7.2" rx="0.5" fill="#fff"/>'
            f'<rect x="{cx + 0.9}" y="{cy - 3.6}" width="2.3" height="7.2" rx="0.5" fill="#fff"/>'
        )
    else:  # attention
        mark = (
            f'<rect x="{cx - 1.2}" y="{cy - 4.4}" width="2.4" height="5.6" rx="1.1" fill="#fff"/>'
            f'<circle cx="{cx}" cy="{cy + 3.2}" r="1.35" fill="#fff"/>'
        )
    return ring + disc + mark


def svg(state: str | None) -> str:
    body = (
        f'<rect x="1" y="1" width="30" height="30" rx="7" fill="{BG}" stroke="{BG_EDGE}" stroke-width="0.6"/>'
        + book()
        + (badge(state) if state else "")
    )
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32" viewBox="0 0 32 32">'
        f"{body}</svg>\n"
    )


def rasterize(src: pathlib.Path, size: int, out: pathlib.Path) -> None:
    out.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["rsvg-convert", "-w", str(size), "-h", str(size), "-o", str(out), str(src)],
        check=True,
    )


def png_size(data: bytes) -> tuple[int, int]:
    return struct.unpack(">II", data[16:24])


def write_ico(pngs: list[bytes], out: pathlib.Path) -> None:
    """ICO with PNG-compressed entries (Windows Vista+ reads them at every size)."""
    header = struct.pack("<HHH", 0, 1, len(pngs))
    offset = 6 + 16 * len(pngs)
    entries, blobs = b"", b""
    for data in pngs:
        w, h = png_size(data)
        entries += struct.pack(
            "<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(data), offset + len(blobs)
        )
        blobs += data
    out.write_bytes(header + entries + blobs)


# ICNS chunk types that hold PNG data, by pixel size (and the @2x aliases).
ICNS_TYPES = [
    (b"icp4", 16),
    (b"icp5", 32),
    (b"icp6", 64),
    (b"ic07", 128),
    (b"ic08", 256),
    (b"ic09", 512),
    (b"ic10", 1024),
    (b"ic11", 32),
    (b"ic12", 64),
    (b"ic13", 256),
    (b"ic14", 512),
]


def write_icns(png_for: dict[int, bytes], out: pathlib.Path) -> None:
    chunks = b""
    for kind, size in ICNS_TYPES:
        data = png_for[size]
        chunks += kind + struct.pack(">I", len(data) + 8) + data
    out.write_bytes(b"icns" + struct.pack(">I", len(chunks) + 8) + chunks)


def main() -> int:
    if not shutil.which("rsvg-convert"):
        print("rsvg-convert (librsvg) is required", file=sys.stderr)
        return 1
    svg_dir, png_dir = HERE / "svg", HERE / "png"
    for d in (svg_dir, png_dir, HERE / "hicolor"):
        if d.exists():
            shutil.rmtree(d)
        d.mkdir(parents=True)

    app_svg = svg_dir / "mokuro-bunko.svg"
    app_svg.write_text(svg(None))
    for state in STATES:
        (svg_dir / f"tray-{state}.svg").write_text(svg(state if state != "idle" else None))

    sizes = sorted(set(APP_SIZES) | set(ICO_SIZES))
    app_png: dict[int, bytes] = {}
    for n in sizes:
        out = png_dir / f"mokuro-bunko-{n}.png"
        rasterize(app_svg, n, out)
        app_png[n] = out.read_bytes()
        if n not in APP_SIZES:
            out.unlink()
    for state in STATES:
        for n in TRAY_SIZES:
            rasterize(svg_dir / f"tray-{state}.svg", n, png_dir / f"tray-{state}-{n}.png")

    for n in HICOLOR_SIZES:
        dest = HERE / "hicolor" / f"{n}x{n}" / "apps" / "mokuro-bunko.png"
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(app_png[n])
    scalable = HERE / "hicolor" / "scalable" / "apps" / "mokuro-bunko.svg"
    scalable.parent.mkdir(parents=True, exist_ok=True)
    scalable.write_text(svg(None))

    write_ico([app_png[n] for n in ICO_SIZES], HERE / "mokuro-bunko.ico")
    write_icns(app_png, HERE / "mokuro-bunko.icns")
    print(f"wrote icons into {HERE}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
