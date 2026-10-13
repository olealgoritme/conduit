"""Render the Conduit firmware boot logo packaging/bios/Logo.bmp.

The mark (the geometry of packaging/common/icons/conduit.svg, without the tile)
as a lit tube with a soft bloom, above a geometric CONDUIT wordmark. The letters
are drawn from lines and arcs, so no font is needed.

The firmware (OVMF, see build.sh) centers the image on its black screen and
publishes it as the ACPI BGRT logo, which Windows keeps above its boot spinner.
What shapes the image:

  * 24-bit uncompressed BMP, no alpha: the background is exactly (0,0,0) so the
    image blends into the screen, and every glow fades to exact black before the
    image border.
  * The firmware centers the image (LogoDxe: EdkiiPlatformLogoDisplayAttributeCenter)
    and Windows draws its spinner at a fixed place below the screen centre (about
    three quarters of the way down), so the logo stays within 480 px of height.
  * The image sits in the firmware volume (about 9 MB free uncompressed, about
    1.8 MB free after the volume's LZMA compression); this one is 0.85 MB raw.
  * It is static; no animation is possible.

The build uses the committed Logo.bmp, so building the firmware needs neither
numpy nor Pillow. Regenerate it after changing this script:

    pip install numpy pillow
    packaging/bios/make-logo.py packaging/bios/Logo.bmp

Usage: make-logo.py OUT.bmp [HEIGHT]
"""
import argparse
import math

import numpy as np
from PIL import Image

R = 68.0
STROKE = 30.0
CYAN = (0x22, 0xD3, 0xEE)
VIOLET = (0xA7, 0x8B, 0xFA)
DARK = (0x0F, 0x17, 0x2A)


# ---------------------------------------------------------------------------
# Analytic distance fields in floating point, so edges are exactly anti-aliased
# at any size and glows/gradients are dithered to 8 bit only once, at the end.
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


def hgrad(cv, x0, x1):
    t = np.clip((cv.X - x0) / (x1 - x0), 0, 1)[..., None]
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


def caps(cv, T, glow=0.0):
    """The two white end beads with their dark rims."""
    s = T.s
    white = np.ones(3)
    for (tx, ty) in T.terminals():
        dx, dy = cv.X - tx, cv.Y - ty
        d = np.hypot(dx, dy)
        cv.over(col(DARK), cv.cov(d - 19 * s))
        bead = np.clip(shade(dx, dy, 13 * s, white, 0.55, 0.55, 30, 0.35), 0, 1)
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
def logo(height=480):
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


def main():
    ap = argparse.ArgumentParser(description="Render the Conduit firmware boot logo.")
    ap.add_argument("out", help="output .bmp")
    ap.add_argument("height", nargs="?", type=int, default=480, help="image height in pixels (default 480)")
    a = ap.parse_args()
    logo(a.height).save(a.out, format="BMP")


if __name__ == "__main__":
    main()
