"""Generate ClipVault's icon assets (run once at dev time; outputs are committed).

Greyscale rounded tile with a white "rewind ring" and a coloured status dot.
"""
from PIL import Image, ImageDraw
import math, os, sys

OUT = os.path.join(os.path.dirname(__file__), "..", "assets")
S = 1024

def tile(dot_rgb, ring=True):
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    pad = 40
    d.rounded_rectangle([pad, pad, S - pad, S - pad], radius=220, fill=(28, 28, 32, 255), outline=(70, 70, 78, 255), width=18)
    cx, cy, r = S / 2, S / 2, 300
    if ring:
        # Open ring (rewind arrow) from 40deg to 330deg.
        w = 64
        d.arc([cx - r, cy - r, cx + r, cy + r], start=-50, end=240, fill=(240, 240, 244, 255), width=w)
        # Arrow head at the end of the arc (angle 240deg -> pointing counter-clockwise).
        a = math.radians(-50)
        ex, ey = cx + r * math.cos(a), cy + r * math.sin(a)
        head = 95
        d.polygon([(ex - head, ey - 15), (ex + head * 0.55, ey - head * 0.95), (ex + head * 0.65, ey + head * 0.55)], fill=(240, 240, 244, 255))
    dr = 120
    d.ellipse([cx - dr, cy - dr, cx + dr, cy + dr], fill=dot_rgb + (255,))
    return img

def save_png(img, name, size):
    img.resize((size, size), Image.LANCZOS).save(os.path.join(OUT, name))

main = tile((229, 72, 77))
main.resize((256, 256), Image.LANCZOS).save(os.path.join(OUT, "icon.ico"), sizes=[(16, 16), (20, 20), (24, 24), (32, 32), (40, 40), (48, 48), (64, 64), (128, 128), (256, 256)])
save_png(main, "icon_256.png", 256)
save_png(tile((229, 72, 77)), "tray_rec.png", 32)
save_png(tile((150, 150, 156)), "tray_idle.png", 32)
save_png(tile((245, 166, 35)), "tray_warn.png", 32)
print("ok")
