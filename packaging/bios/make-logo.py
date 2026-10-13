#!/usr/bin/env python3
"""Render the Conduit firmware boot logo (24-bit BMP, black background).

The firmware (OVMF, see build.sh) centers the image on its black screen and
publishes it as the ACPI BGRT logo, which Windows keeps above its boot spinner.
Constraints that shape every style:

  * 24-bit uncompressed BMP, no alpha: the background is exactly (0,0,0) so the
    image blends into the screen, and every glow fades to exact black before the
    image border.
  * The firmware centers the image (LogoDxe: EdkiiPlatformLogoDisplayAttributeCenter);
    Windows draws its spinner at a fixed place below the screen centre (about three
    quarters of the way down), so a logo should stay within about 480 px of height.
  * The image sits in the firmware volume (about 9 MB free uncompressed, about
    1.8 MB free after the volume's LZMA compression), so keep the BMP small.
  * It is static; no animation is possible.

Styles (--list prints them):
  classic   the mark as before: flat gradient "C" ring, two white dots
  aura      the mark as a lit tube with a soft bloom and depth
  wordmark  the mark above a geometric "CONDUIT" wordmark
  flow      neon line art; light trails thread through the C
  emblem    the mark inside a detailed instrument-dial emblem
  premium   conic gradient, hairline highlight, floor reflection; dithered

The geometry is that of packaging/common/icons/conduit.svg (without the tile).
The letters of the wordmark are drawn from lines and arcs, so no font is needed.
All styles but classic need numpy (pip install numpy pillow).

Usage: make-logo.py [--style NAME] OUT.bmp [HEIGHT]
       make-logo.py --preview DIR [--style NAME ...]   previews on 5120x1440 and 1920x1080
"""
import argparse
import math
import os
import sys

from PIL import Image, ImageDraw

try:
    import numpy as np
except ImportError:  # only the classic style works without numpy
    np = None

SS = 8  # supersampling factor of the classic style
CX, CY, R = 128.0, 128.0, 68.0
STROKE = 30.0
CYAN = (0x22, 0xD3, 0xEE)
VIOLET = (0xA7, 0x8B, 0xFA)
DARK = (0x0F, 0x17, 0x2A)


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


# ---------------------------------------------------------------------------
# classic: PIL only, supersampled
# ---------------------------------------------------------------------------
def classic(height=192):
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
    return img.crop(box).resize((height, height), Image.LANCZOS)


# ---------------------------------------------------------------------------
# The other styles: analytic distance fields in floating point (numpy), so edges
# are exactly anti-aliased at any size and glows/gradients are dithered to 8 bit
# only once, at the very end.
# ---------------------------------------------------------------------------
ALPHA = math.radians(45.6)  # half of the C's opening


def col(c):
    return np.array(c, float) / 255.0


class Cv:
    """A pixel grid with coordinates in design units (the 256-unit mark space),
    origin in the middle, y down. k = pixels per unit."""

    def __init__(self, cw, ch, height):
        self.k = height / ch
        self.H = int(height)
        self.W = int(round(cw * self.k))
        self.X = ((np.arange(self.W) + 0.5 - self.W / 2) / self.k)[None, :].repeat(self.H, 0)
        self.Y = ((np.arange(self.H) + 0.5 - self.H / 2) / self.k)[:, None].repeat(self.W, 1)
        self.img = np.zeros((self.H, self.W, 3))

    def cov(self, sd):
        """Pixel coverage of a shape from its signed distance (units)."""
        return np.clip(0.5 - sd * self.k, 0.0, 1.0)

    def blur(self, a, sigma):
        """Gaussian blur, sigma in units, zero padded so nothing wraps around."""
        sp = sigma * self.k
        pad = int(3 * sp) + 2
        a = np.pad(a, ((pad, pad), (pad, pad)) + ((0, 0),) * (a.ndim - 2))
        h, w = a.shape[:2]
        ky = np.fft.fftfreq(h)[:, None]
        kx = np.fft.rfftfreq(w)[None, :]
        g = np.exp(-2 * (math.pi * sp) ** 2 * (ky * ky + kx * kx))
        f = np.fft.rfft2(a, axes=(0, 1))
        f = f * (g[:, :, None] if a.ndim == 3 else g)
        out = np.fft.irfft2(f, s=(h, w), axes=(0, 1))
        return out[pad:-pad, pad:-pad]

    def over(self, color, alpha):
        alpha = np.asarray(alpha)[..., None]
        self.img = self.img * (1 - alpha) + np.asarray(color) * alpha

    def add(self, light):
        self.img = self.img + light


def hgrad(cv, x0, x1, tilt=0.0):
    t = np.clip((cv.X + tilt * cv.Y - x0) / (x1 - x0), 0, 1)[..., None]
    return col(CYAN) * (1 - t) + col(VIOLET) * t


def lgrad(t):
    t = np.clip(t, 0, 1)[..., None]
    return col(CYAN) * (1 - t) + col(VIOLET) * t


def sd_seg(cv, ax, ay, bx, by):
    px, py = cv.X - ax, cv.Y - ay
    ex, ey = bx - ax, by - ay
    h = np.clip((px * ex + py * ey) / (ex * ex + ey * ey), 0, 1)
    return np.hypot(px - ex * h, py - ey * h)


def sd_arc(cv, cx, cy, rho, a_lo, a_hi):
    """Distance to a circular arc (centreline), angles in radians, a_lo < a_hi."""
    dx, dy = cv.X - cx, cv.Y - cy
    r = np.hypot(dx, dy) + 1e-9
    th = (np.arctan2(dy, dx) - a_lo) % (2 * math.pi)
    inside = th <= (a_hi - a_lo)
    d1 = np.hypot(cv.X - (cx + rho * math.cos(a_lo)), cv.Y - (cy + rho * math.sin(a_lo)))
    d2 = np.hypot(cv.X - (cx + rho * math.cos(a_hi)), cv.Y - (cy + rho * math.sin(a_hi)))
    return np.where(inside, np.abs(r - rho), np.minimum(d1, d2))


class Tube:
    """The C: distance to its centreline, offset vector, arc length from the top end."""

    def __init__(self, cv, acx, acy, s):
        self.s, self.acx, self.acy = s, acx, acy
        self.R = R * s
        self.hw = STROKE / 2 * s
        dx, dy = cv.X - acx, cv.Y - acy
        r = np.hypot(dx, dy) + 1e-9
        th = np.arctan2(dy, dx) % (2 * math.pi)
        self.inside = (th >= ALPHA) & (th <= 2 * math.pi - ALPHA)
        self.top = (acx + self.R * math.cos(ALPHA), acy - self.R * math.sin(ALPHA))
        self.bot = (acx + self.R * math.cos(ALPHA), acy + self.R * math.sin(ALPHA))
        d1 = np.hypot(cv.X - self.top[0], cv.Y - self.top[1])
        d2 = np.hypot(cv.X - self.bot[0], cv.Y - self.bot[1])
        use1 = d1 < d2
        qx = np.where(self.inside, acx + self.R * dx / r, np.where(use1, self.top[0], self.bot[0]))
        qy = np.where(self.inside, acy + self.R * dy / r, np.where(use1, self.top[1], self.bot[1]))
        self.vx, self.vy = cv.X - qx, cv.Y - qy
        self.dc = np.hypot(self.vx, self.vy)
        self.length = (2 * math.pi - 2 * ALPHA) * self.R
        self.along = np.clip((2 * math.pi - ALPHA - th) * self.R, 0, self.length)
        self.cv = cv
        self.cover = cv.cov(self.dc - self.hw)

    def terminals(self):
        return (self.top, self.bot)


if np is not None:
    LIGHT = np.array([-0.45, -0.65, 0.62])
    LIGHT /= np.linalg.norm(LIGHT)
    HALF = LIGHT + np.array([0, 0, 1.0])
    HALF /= np.linalg.norm(HALF)


def shade(vx, vy, rad, base, ambient=0.42, gain=0.78, spec_pow=40, spec=0.45):
    """Lambert + Blinn-Phong on a round cross-section (tube or sphere)."""
    u2 = np.clip((vx * vx + vy * vy) / (rad * rad), 0, 1)
    nx, ny, nz = vx / rad, vy / rad, np.sqrt(1 - u2)
    diff = np.clip(nx * LIGHT[0] + ny * LIGHT[1] + nz * LIGHT[2], 0, 1)
    sp = np.clip(nx * HALF[0] + ny * HALF[1] + nz * HALF[2], 0, 1) ** spec_pow
    return base * (ambient + gain * diff)[..., None] + (spec * sp)[..., None]


def flow_dots(T, rad=2.5, spacing=16.0):
    s = T.s
    n = np.clip(np.round(T.along / (spacing * s)), 0, math.floor(T.length / (spacing * s)))
    d = np.hypot(T.along - n * spacing * s, T.dc) - rad * s
    return T.cv.cov(d) * T.inside


def caps(cv, T, glow=0.0, flat=False):
    """The two white end beads with their dark rims."""
    s = T.s
    white = np.ones(3)
    for (tx, ty) in T.terminals():
        dx, dy = cv.X - tx, cv.Y - ty
        d = np.hypot(dx, dy)
        cv.over(col(DARK), cv.cov(d - 19 * s))
        bead = white if flat else np.clip(shade(dx, dy, 13 * s, white, 0.55, 0.55, 30, 0.35), 0, 1)
        cv.over(bead, cv.cov(d - 13 * s))
    if glow:
        m = sum(cv.cov(np.hypot(cv.X - tx, cv.Y - ty) - 13 * s) for tx, ty in T.terminals())
        cv.add(glow * cv.blur(np.clip(m, 0, 1)[..., None] * np.ones(3), 4 * s))


def knee(x, a=0.78):
    """Soft highlight roll-off so additive glow never clips hard."""
    over = np.maximum(x - a, 0)
    return np.where(x <= a, x, a + (1 - a) * np.tanh(over / (1 - a)))


def finish(cv, margin=0.07, seed=7):
    """Window to exact black at the border, tone-map, dither to 8 bit."""
    img = knee(np.clip(cv.img, 0, None))
    m = max(2.0, margin * min(cv.W, cv.H))
    xi = np.minimum(np.arange(cv.W), np.arange(cv.W)[::-1])[None, :] / m
    yi = np.minimum(np.arange(cv.H), np.arange(cv.H)[::-1])[:, None] / m
    e = np.clip(np.minimum(xi, yi), 0, 1)
    img = img * (e * e * (3 - 2 * e))[..., None]
    rng = np.random.default_rng(seed)
    noise = (rng.random(img.shape) - rng.random(img.shape)) / 255.0  # triangular, +-1 LSB
    out = np.clip(np.rint((img + noise) * 255.0), 0, 255).astype(np.uint8)
    out[(img * 255.0) < 0.02] = 0  # no speckle on the black
    return Image.fromarray(out, "RGB")


# --- geometric letters --------------------------------------------------------
# Centreline glyphs, cap height 1 (y from -0.5 to 0.5), drawn with round caps.
def _glyph(ch):
    a = math.radians(42)
    if ch == "C":
        return 0.9, [("arc", 0.5, 0, 0.5, a, 2 * math.pi - a)]
    if ch == "O":
        return 1.0, [("arc", 0.5, 0, 0.5, 0, 2 * math.pi - 1e-6)]
    if ch == "N":
        return 0.78, [("seg", 0, -.5, 0, .5), ("seg", .78, -.5, .78, .5), ("seg", 0, -.5, .78, .5)]
    if ch == "D":
        return 0.78, [("seg", 0, -.5, 0, .5), ("seg", 0, -.5, .28, -.5), ("seg", 0, .5, .28, .5),
                      ("arc", .28, 0, .5, -math.pi / 2, math.pi / 2)]
    if ch == "U":
        return 0.78, [("seg", 0, -.5, 0, .12), ("seg", .78, -.5, .78, .12),
                      ("arc", .39, .12, .39, 0, math.pi)]
    if ch == "I":
        return 0.0, [("seg", 0, -.5, 0, .5)]
    if ch == "T":
        return 0.82, [("seg", 0, -.5, .82, -.5), ("seg", .41, -.5, .41, .5)]
    raise ValueError(ch)


def wordmark_sd(cv, text, cx, cy, cap, stroke, track):
    """Signed distance to the letters; text is centred on (cx, cy)."""
    glyphs = [_glyph(c) for c in text]
    total = sum(w for w, _ in glyphs) + track * (len(text) - 1)
    x = -total / 2
    d = np.full((cv.H, cv.W), 1e9)
    for w, prims in glyphs:
        for p in prims:
            if p[0] == "seg":
                _, ax, ay, bx, by = p
                dp = sd_seg(cv, cx + (x + ax) * cap, cy + ay * cap, cx + (x + bx) * cap, cy + by * cap)
            else:
                _, ax, ay, rho, lo, hi = p
                dp = sd_arc(cv, cx + (x + ax) * cap, cy + ay * cap, rho * cap, lo, hi)
            d = np.minimum(d, dp)
        x += w + track
    return d - stroke / 2


# --- styles ---------------------------------------------------------------------
def style_aura(height=480):
    """The mark as a lit tube with a layered bloom."""
    cv = Cv(266, 266, height)
    T = Tube(cv, 8, 0, 1.0)
    g = hgrad(cv, 8 - 66, 8 + 68)
    layer = g * T.cover[..., None]
    cv.add(0.55 * cv.blur(layer, 5) + 0.42 * cv.blur(layer, 16) + 0.30 * cv.blur(layer, 34))
    # dim the ring where it turns away from the light and add a subtle inner edge
    pipe = np.clip(shade(T.vx, T.vy, T.hw, g, 0.46, 0.80, 36, 0.40), 0, 1)
    cv.over(pipe, T.cover)
    cv.over(np.ones(3), 0.55 * flow_dots(T))
    caps(cv, T, glow=0.55)
    return finish(cv)


def style_wordmark(height=480):
    """The mark above a geometric CONDUIT wordmark."""
    cv = Cv(330, 270, height)
    acx, acy = 8, -30
    T = Tube(cv, acx, acy, 1.0)
    g = hgrad(cv, acx - 66, acx + 68)
    layer = g * T.cover[..., None]
    cv.add(0.30 * cv.blur(layer, 4) + 0.18 * cv.blur(layer, 14))
    cv.over(np.clip(shade(T.vx, T.vy, T.hw, g, 0.62, 0.55, 40, 0.30), 0, 1), T.cover)
    cv.over(np.ones(3), 0.5 * flow_dots(T))
    caps(cv, T, glow=0.3)
    # wordmark, gradient across the letters, with a faint glow
    cap, stroke, track = 22.0, 3.4, 0.5
    d = wordmark_sd(cv, "CONDUIT", 0, 100, cap, stroke, track)
    wg = hgrad(cv, -105, 105)
    cv.add(0.25 * cv.blur(wg * cv.cov(d)[..., None], 4))
    cv.over(wg * 0.95 + 0.05, cv.cov(d))
    return finish(cv)


def style_flow(height=480):
    """Neon line art; trails of light run through the C like data through a conduit."""
    cv = Cv(612, 266, height)
    acx = 8
    T = Tube(cv, acx, 0, 1.0)
    g = hgrad(cv, acx - 66, acx + 68)
    ones = np.ones(3)

    # rails: a faint hairline per row plus comets, hidden behind the tube and the beads
    occl = np.maximum(cv.cov(T.dc - T.hw - 1.5),
                      sum(cv.cov(np.hypot(cv.X - tx, cv.Y - ty) - 20) for tx, ty in T.terminals()))
    occl = np.clip(occl, 0, 1)
    rails = np.zeros((cv.H, cv.W, 3))
    rows = (-49, -24.5, 0, 24.5, 49)
    packets = {  # per row: (head x, tail length, strength)
        -49: [(-250, 120, .9), (-95, 60, .6), (30, 70, .8), (170, 150, 1.0), (290, 70, .7)],
        -24.5: [(-190, 80, .6), (-30, 110, .75), (120, 55, .55), (255, 130, .8)],
        0: [(-270, 90, .65), (-140, 150, .9), (10, 80, .6), (95, 60, .7), (220, 110, .9)],
        24.5: [(-210, 130, .8), (-60, 70, .55), (80, 120, .8), (240, 60, .6)],
        49: [(-280, 60, .7), (-160, 110, .8), (-20, 90, .7), (140, 70, .9), (265, 110, .85)],
    }
    env = np.clip(1 - np.abs(cv.X) / 300.0, 0, 1) ** 1.3
    for y in rows:
        dy = np.abs(cv.Y - y)
        line = cv.cov(dy - 0.45)
        base = 0.38 * line
        lit = np.zeros_like(base)
        for head, tail, st in packets[y]:
            t = (head - cv.X) / tail  # 0 at the head, 1 at the end of the tail
            f = np.where((t >= 0) & (t <= 1), (1 - np.clip(t, 0, 1)) ** 2.2, 0)
            lit = np.maximum(lit, st * f * cv.cov(dy - 0.75))
            lit = np.maximum(lit, st * cv.cov(np.hypot(cv.X - head, dy) - 1.5))
        rails += (g * (base + 1.5 * lit)[..., None]) * env[..., None]
    rails = rails * (1 - occl)[..., None]
    cv.add(rails + 1.0 * cv.blur(rails, 2.5) + 0.8 * cv.blur(rails, 9))

    # neon tube: dark core, tinted fill, bright edges, a data packet running through
    cv.over(np.zeros(3), T.cover)
    cv.over(g * 0.07, T.cover)
    edge = cv.cov(np.abs(T.dc - T.hw) - 0.9)
    hot = g * 0.75 + 0.25
    cv.over(hot, edge)
    cv.add(0.55 * cv.blur(g * edge[..., None], 3) + 0.45 * cv.blur(g * edge[..., None], 11)
           + 0.20 * cv.blur(g * edge[..., None], 28))
    # comets along the centreline, flowing from the lower end round to the upper end
    from_bot = T.length - T.along
    cl = np.zeros_like(T.dc)
    for head, tail, st in ((18, 85, 1.0), (138, 60, .8), (222, 90, .9)):
        t = (head - from_bot) / tail
        f = np.where((t >= 0) & (t <= 1), np.exp(-3.0 * np.clip(t, 0, 1)) * (1 - np.clip(t, 0, 1)), 0)
        cl = np.maximum(cl, st * f)
        hx, hy = _arc_point(T, head)
        cl = np.maximum(cl, st * cv.cov(np.hypot(cv.X - hx, cv.Y - hy) - 3.4))
    cl = cl * T.inside * cv.cov(T.dc - 2.6) * 1.0
    cv.over(hot, cl * 0.9)
    cv.add(0.7 * cv.blur(g * cl[..., None], 4))
    # beads
    for tx, ty in T.terminals():
        d = np.hypot(cv.X - tx, cv.Y - ty)
        cv.over(np.zeros(3), cv.cov(d - 19))
        ring = cv.cov(np.abs(d - 17.5) - 0.9)
        cv.over(hot, ring)
        cv.over(ones, cv.cov(d - 8.5))
        cv.add(0.7 * cv.blur((cv.cov(np.abs(d - 17.5) - 1.5) * 0.7 + cv.cov(d - 8.5))[..., None] * g, 5))
    return finish(cv, margin=0.04)


def _arc_point(T, from_bot):
    """Point on the centreline `from_bot` units of arc away from the lower end."""
    ang = ALPHA + from_bot / T.R
    return T.acx + T.R * math.cos(ang), T.acy + T.R * math.sin(ang)


def style_emblem(height=480):
    """The mark inside an instrument-dial emblem: ticks, segmented rings, nodes."""
    cv = Cv(266, 266, height)
    ones = np.ones(3)
    g = hgrad(cv, -120, 120)
    r = np.hypot(cv.X, cv.Y)
    phi = np.arctan2(cv.Y, cv.X)

    def ring(rad, w, strength=1.0, gap=None):
        a = cv.cov(np.abs(r - rad) - w / 2)
        if gap is not None:  # list of (lo, hi) angle spans (degrees) that are drawn
            m = np.zeros_like(a)
            for lo, hi in gap:
                m = np.maximum(m, cv.cov(sd_arc(cv, 0, 0, rad, math.radians(lo), math.radians(hi)) - w / 2))
            a = m
        return a * strength

    # soft radial glow behind everything
    rw = np.clip((128.0 - r) / 30.0, 0, 1)  # the haze ends inside the image, no square edge
    rw = rw * rw * (3 - 2 * rw)
    cv.add((0.10 * np.exp(-(r / 78.0) ** 2) + 0.05 * np.exp(-(r / 110.0) ** 2))[..., None] * g * rw[..., None])

    # outer hairline and tick ring
    cv.over(g, ring(122, 0.9, 0.85))
    tick_step = math.radians(5)
    k = np.round(phi / tick_step)
    dphi = phi - k * tick_step
    tang = np.abs(r * np.sin(dphi))
    major = (np.mod(k, 6) == 0)
    r0 = np.where(major, 103.0, 109.0)
    rad_out = np.maximum(np.maximum(r0 - r, r - 117.0), 0)
    tick_d = np.hypot(np.maximum(tang - np.where(major, 0.9, 0.5), 0), rad_out)
    tick = cv.cov(tick_d) * np.where(major, 0.95, 0.5)
    cv.over(g, tick)

    # segmented middle ring with nodes at the segment ends
    spans = [(18, 102), (138, 222), (258, 342)]
    seg = ring(93, 2.4, 1.0, spans)
    cv.add(0.5 * cv.blur(g * seg[..., None], 3))
    cv.over(g, seg)
    for lo, hi in spans:
        for a in (lo, hi):
            x, y = 93 * math.cos(math.radians(a)), 93 * math.sin(math.radians(a))
            d = np.hypot(cv.X - x, cv.Y - y)
            cv.over(col(DARK), cv.cov(d - 4.6))
            cv.over(ones, cv.cov(d - 2.9))
    # a comet orbiting on the inner hairline ring
    cv.over(g, ring(80, 0.6, 0.5))
    head = math.radians(-32)
    tail = math.radians(150)
    dth = (head - phi) % (2 * math.pi)
    f = np.where(dth <= tail, np.exp(-3.2 * dth / tail) * (1 - dth / tail), 0)
    comet = cv.cov(np.abs(r - 80) - 1.5) * f
    cv.over(ones * 0.8 + g * 0.2, comet)
    cv.add(0.6 * cv.blur(g * comet[..., None], 3))
    hx, hy = 80 * math.cos(head), 80 * math.sin(head)
    d = np.hypot(cv.X - hx, cv.Y - hy)
    cv.over(ones, cv.cov(d - 3.2))
    cv.add(0.6 * cv.blur(cv.cov(d - 3.2)[..., None] * g, 4))
    # spokes tie the dial to the mark
    for a in (90, 270, 180):
        c, s_ = math.cos(math.radians(a)), math.sin(math.radians(a))
        sdl = sd_seg(cv, 56 * c, 56 * s_, 79 * c, 79 * s_)
        cv.over(g, cv.cov(sdl - 0.5) * 0.7)

    # the mark
    sc = 0.74
    T = Tube(cv, 8 * sc, 0, sc)
    layer = g * T.cover[..., None]
    cv.add(0.45 * cv.blur(layer, 3) + 0.30 * cv.blur(layer, 10))
    cv.over(np.clip(shade(T.vx, T.vy, T.hw, g, 0.46, 0.80, 36, 0.40), 0, 1), T.cover)
    cv.over(ones, 0.5 * flow_dots(T))
    caps(cv, T, glow=0.5)
    return finish(cv, margin=0.03)


def style_premium(height=420):
    """Minimal and premium: conic gradient, hairline highlight, floor reflection."""
    cv = Cv(250, 250, height)
    sc, acx, acy = 1.0, 8.0, -40.0
    T = Tube(cv, acx, acy, sc)
    t = T.along / T.length
    g = lgrad(t)
    # the tube: clean gradient, a hairline highlight on its upper-left, soft shading
    lit = np.clip((T.vx * LIGHT[0] + T.vy * LIGHT[1]) / T.hw, -1, 1)
    body = g * (0.82 - 0.18 * lit)[..., None]
    cv.over(np.clip(body, 0, 1), T.cover)
    # hairline highlight: a thin bright line inset toward the light
    off = (T.vx * LIGHT[0] + T.vy * LIGHT[1]) / np.hypot(LIGHT[0], LIGHT[1]) / T.hw
    hair = np.exp(-((off - 0.55) / 0.07) ** 2) * T.cover * (T.dc < T.hw)
    cv.over(np.ones(3), 0.38 * np.clip(hair, 0, 1))
    cv.add(0.22 * cv.blur(g * T.cover[..., None], 6))
    cv.over(np.ones(3), 0.40 * flow_dots(T, rad=2.2))
    caps(cv, T, glow=0.35)
    cv.img = np.clip(cv.img, 0, 1)

    # floor: mirror image below the baseline, fading out, plus a thin light pool
    base_y = acy + R * sc + STROKE / 2 * sc + 3
    yb = base_y * cv.k + cv.H / 2 - 0.5
    rows = np.arange(cv.H)
    src = np.rint(2 * yb - rows).astype(int)
    ok = (rows > yb) & (src >= 0)
    refl = np.zeros_like(cv.img)
    refl[ok] = cv.img[src[ok]]
    depth = np.clip((rows - yb) / (cv.k * 74.0), 0, 1)
    fade = (0.30 * (1 - depth) ** 2.2) * ok
    refl = cv.blur(refl, 1.3) * fade[:, None, None]
    pool = np.exp(-(((cv.Y - base_y) / 2.2) ** 2)) * np.exp(-((cv.X / 82.0) ** 4))
    cv.add(refl + 0.20 * pool[..., None] * lgrad((cv.X + 80) / 160.0))
    return finish(cv, margin=0.05)


STYLES = {
    "classic": (classic, "flat gradient C ring with two white dots (as before)"),
    "aura": (style_aura, "lit tube with layered bloom and depth"),
    "wordmark": (style_wordmark, "mark above a geometric CONDUIT wordmark"),
    "flow": (style_flow, "neon line art; light trails thread through the C"),
    "emblem": (style_emblem, "mark inside a detailed instrument-dial emblem"),
    "premium": (style_premium, "conic gradient, hairline highlight, floor reflection"),
}


def render(style, height=None):
    fn = STYLES[style][0]
    if style != "classic" and np is None:
        sys.exit("make-logo.py: style '%s' needs numpy (pip install numpy)" % style)
    return fn(height) if height else fn()


# ---------------------------------------------------------------------------
# previews
# ---------------------------------------------------------------------------
def mock_frame(logo, w, h):
    """The logo centred on black, with a stand-in for Windows' boot spinner (a ring
    of five dots about three quarters of the way down, centred)."""
    frame = Image.new("RGB", (w, h), (0, 0, 0))
    frame.paste(logo, ((w - logo.width) // 2, (h - logo.height) // 2))
    d = ImageDraw.Draw(frame)
    cx, cy, rad = w / 2, h * 0.75, h * 0.022
    for i in range(5):
        a = -math.pi / 2 + i * 2 * math.pi / 5 * 0.82
        x, y, rr = cx + rad * math.cos(a), cy + rad * math.sin(a), h * (0.0075 - 0.0009 * i)
        d.ellipse([x - rr, y - rr, x + rr, y + rr], fill=(255, 255, 255))
    return frame


def preview(outdir, styles):
    from PIL import ImageFont
    os.makedirs(outdir, exist_ok=True)
    try:
        font = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 26)
    except OSError:
        font = ImageFont.load_default()
    wide, hd, crops = [], [], []
    for name in styles:
        logo = render(name)
        for (w, h), tag, acc in (((5120, 1440), "5120x1440", wide), ((1920, 1080), "1920x1080", hd)):
            fr = mock_frame(logo, w, h)
            fr.save(os.path.join(outdir, "%s-%s.png" % (name, tag)))
            acc.append((name, fr))
        wfr = wide[-1][1]
        crop = wfr.crop((5120 // 2 - 700, 1440 // 2 - 520, 5120 // 2 + 700, 1440 // 2 + 520))
        crop.save(os.path.join(outdir, "%s-crop.png" % name))
        print("%-9s %4dx%-4d BMP %7d bytes" % (name, logo.width, logo.height, logo.width * logo.height * 3 + 54))

    def sheet(items, cols, cell_w, name):
        ch = round(cell_w * items[0][1].height / items[0][1].width)
        rows = -(-len(items) // cols)
        gap, cap = 14, 38
        sh = Image.new("RGB", (cols * cell_w + (cols + 1) * gap, rows * (ch + cap) + (rows + 1) * gap), (24, 24, 28))
        d = ImageDraw.Draw(sh)
        for i, (n, fr) in enumerate(items):
            x = gap + (i % cols) * (cell_w + gap)
            y = gap + (i // cols) * (ch + cap + gap)
            sh.paste(fr.resize((cell_w, ch), Image.LANCZOS), (x, y + cap))
            d.text((x + 4, y + 6), n, fill=(230, 230, 235), font=font)
        sh.save(os.path.join(outdir, name))

    sheet(wide, 2, 1280, "sheet.png")
    sheet(hd, 3, 800, "sheet-1080p.png")
    sheet([(n, Image.open(os.path.join(outdir, "%s-crop.png" % n))) for n, _ in wide], 3, 800, "sheet-crops.png")


def main():
    ap = argparse.ArgumentParser(description="Render the Conduit firmware boot logo.")
    ap.add_argument("out", nargs="?", help="output .bmp")
    ap.add_argument("height", nargs="?", type=int, help="image height in pixels (default per style)")
    ap.add_argument("--style", action="append", choices=list(STYLES), help="design (default: classic)")
    ap.add_argument("--list", action="store_true", help="list the styles")
    ap.add_argument("--preview", metavar="DIR", help="write mock boot screens and contact sheets to DIR")
    a = ap.parse_args()
    if a.list:
        for n, (_, desc) in STYLES.items():
            print("%-9s %s" % (n, desc))
        return
    if a.preview:
        preview(a.preview, a.style or list(STYLES))
        return
    if not a.out:
        ap.error("OUT.bmp is required")
    style = (a.style or ["classic"])[0]
    render(style, a.height).save(a.out, format="BMP")


if __name__ == "__main__":
    main()
