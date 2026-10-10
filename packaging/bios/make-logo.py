#!/usr/bin/env python3
"""Render the Conduit mark as the firmware boot logo (24-bit BMP, black background).

Draws the same geometry as packaging/common/icons/conduit.svg (without the
rounded tile) so it sits on the firmware's black screen. The firmware centers
the image and publishes it as the ACPI BGRT logo, which Windows keeps above its
boot spinner.

Usage: make-logo.py OUT.bmp [SIZE]
"""
import math
import sys

from PIL import Image, ImageDraw

SS = 8  # supersampling factor
CX, CY, R = 128.0, 128.0, 68.0
STROKE = 30.0
CYAN = (0x22, 0xD3, 0xEE)
VIOLET = (0xA7, 0x8B, 0xFA)
DARK = (0x0F, 0x17, 0x2A)


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def main():
    out = sys.argv[1]
    height = int(sys.argv[2]) if len(sys.argv) > 2 else 192
    size = 256 * SS
    s = lambda v: v * SS  # noqa: E731

    # Arc from the top-right endpoint (176,79) the long way round to (176,177).
    a0 = math.atan2(79 - CY, 176 - CX)  # about -45.6 degrees
    a1 = math.atan2(177 - CY, 176 - CX)  # about +45.6 degrees
    sweep = (a0 - a1) % (2 * math.pi)  # long way, counter-clockwise on screen

    # Pipe: stroke mask, coloured with a horizontal gradient x 62..196.
    mask = Image.new("L", (size, size), 0)
    md = ImageDraw.Draw(mask)
    steps = 2000
    for i in range(steps + 1):
        a = a1 + sweep * i / steps
        x, y = CX + R * math.cos(a), CY + R * math.sin(a)
        r = STROKE / 2
        md.ellipse([s(x - r), s(y - r), s(x + r), s(y + r)], fill=255)
    grad = Image.new("RGB", (size, size))
    gd = ImageDraw.Draw(grad)
    for px in range(size):
        t = min(max((px / SS - 62) / (196 - 62), 0.0), 1.0)
        gd.line([(px, 0), (px, size)], fill=lerp(CYAN, VIOLET, t))
    img = Image.new("RGB", (size, size), (0, 0, 0))
    img.paste(grad, (0, 0), mask)

    # Flow dots along the pipe: white at 50% opacity, every 16 units of arc.
    dots = Image.new("L", (size, size), 0)
    dd = ImageDraw.Draw(dots)
    length = sweep * R
    d = 0.0
    while d <= length + 1e-6:
        a = a0 - d / R
        x, y = CX + R * math.cos(a), CY + R * math.sin(a)
        dd.ellipse([s(x - 2.5), s(y - 2.5), s(x + 2.5), s(y + 2.5)], fill=128)
        d += 16.0
    img.paste(Image.new("RGB", (size, size), (255, 255, 255)), (0, 0), dots)

    # Endpoint caps.
    draw = ImageDraw.Draw(img)
    for x, y in ((176, 79), (176, 177)):
        draw.ellipse([s(x - 19), s(y - 19), s(x + 19), s(y + 19)], fill=DARK)
        draw.ellipse([s(x - 13), s(y - 13), s(x + 13), s(y + 13)], fill=(255, 255, 255))

    # Crop to the mark with a small margin, keep it square, scale down.
    x0, y0, x1, y1 = (s(v) for v in (CX - R - STROKE / 2 - 10, 79 - 19 - 10, 176 + 19 + 10, 177 + 19 + 10))
    side = max(x1 - x0, y1 - y0)
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    box = [round(cx - side / 2), round(cy - side / 2), round(cx + side / 2), round(cy + side / 2)]
    img = img.crop(box).resize((height, height), Image.LANCZOS)
    img.save(out, format="BMP")


if __name__ == "__main__":
    main()
