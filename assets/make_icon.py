#!/usr/bin/env python3
"""Generate the app icon + menubar template icon (requires Pillow).

  python3 assets/make_icon.py
  -> assets/icon-1024.png   app icon source (feed to `npx tauri icon`)
  -> assets/tray.png        64x64 monochrome template glyph (preview)
  -> tauri-app/src-tauri/icons/tray.rgba  raw RGBA for the Tauri tray
"""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent
SS = 4  # supersampling factor


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(len(a)))


def route_glyph(draw, s, scale, ink, active, inactive_fill=None, inactive_ink=None):
    """Rail-switch glyph: one source track forking into two gateway tracks."""
    k = s / 1024 * scale
    off = s / 2 - 512 * k  # keep glyph centered when scaled
    P = lambda x, y: (off + x * k, off + y * k)
    w = 56 * k
    r_line = w / 2

    def disc(c, r, fill, outline=None, ow=0):
        draw.ellipse([c[0]-r, c[1]-r, c[0]+r, c[1]+r], fill=fill, outline=outline, width=int(ow))

    def stroke(pts):
        # stamp discs densely along the path -> seamless round-capped stroke
        for (x0, y0), (x1, y1) in zip(pts, pts[1:]):
            n = max(2, int(((x1-x0)**2 + (y1-y0)**2) ** 0.5 / (r_line / 4)))
            for i in range(n + 1):
                t = i / n
                disc((x0 + (x1-x0)*t, y0 + (y1-y0)*t), r_line, ink)

    def s_curve(a, b, n=60):
        # smooth S between two horizontal tangents
        pts = []
        for i in range(n + 1):
            t = i / n
            e = t * t * (3 - 2 * t)
            pts.append((a[0] + (b[0]-a[0]) * t, a[1] + (b[1]-a[1]) * e))
        return pts

    src, fork = P(268, 512), P(430, 512)
    top_mid, bot_mid = P(620, 372), P(620, 652)
    top, bot = P(756, 372), P(756, 652)

    stroke([src, fork])
    stroke(s_curve(fork, top_mid) + [top])
    stroke(s_curve(fork, bot_mid) + [bot])

    disc(src, 84 * k, ink)
    disc(top, 84 * k, active)
    disc(bot, 78 * k, inactive_fill or ink, outline=inactive_ink or ink, ow=w * 0.9)


def app_icon():
    S = 1024 * SS
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))

    # macOS icon grid: 824px body, 100px margin, ~185px corner radius
    m, rad = 100 * SS, 185 * SS
    body = Image.new("RGBA", (S, S))
    top_c, bot_c = (38, 52, 88), (12, 18, 34)
    grad = Image.new("RGBA", (1, S))
    for y in range(S):
        grad.putpixel((0, y), lerp(top_c, bot_c, y / S) + (255,))
    body = grad.resize((S, S))
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle([m, m, S-m, S-m], rad, fill=255)

    # soft drop shadow
    shadow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    sm = mask.filter(ImageFilter.GaussianBlur(18 * SS))
    shadow.putalpha(sm.point(lambda v: int(v * 0.45)))
    img.alpha_composite(shadow, (0, 10 * SS))
    img.paste(body, (0, 0), mask)

    # inner hairline ring
    ring = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(ring).rounded_rectangle([m, m, S-m, S-m], rad, outline=(255, 255, 255, 28), width=3 * SS)
    img.alpha_composite(ring)

    glyph = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    route_glyph(ImageDraw.Draw(glyph), S, 0.86, ink=(244, 246, 250, 255),
                active=(245, 166, 35, 255), inactive_fill=(24, 34, 60, 255),
                inactive_ink=(244, 246, 250, 255))
    img.alpha_composite(glyph)
    return img.resize((1024, 1024), Image.LANCZOS)


def tray_icon(px=64):
    """Template image: black glyph on transparent; macOS tints it for light/dark menubars."""
    S = px * SS * 4
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    black = (0, 0, 0, 255)
    route_glyph(ImageDraw.Draw(img), S, 1.5, ink=black, active=black, inactive_fill=(0, 0, 0, 0))
    return img.resize((px, px), Image.LANCZOS)


if __name__ == "__main__":
    app_icon().save(ROOT / "icon-1024.png")
    t = tray_icon()
    t.save(ROOT / "tray.png")
    (ROOT.parent / "tauri-app/src-tauri/icons/tray.rgba").write_bytes(t.tobytes())
    print("ok")
