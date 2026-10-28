#!/usr/bin/env python3
"""NDM icon set — bright blue gradient rounded square, white arrow
+ base bar, glass sheen; rendered supersampled for smooth edges."""
from PIL import Image, ImageDraw
import os

SS = 2048  # master render size
TOP = (96, 196, 255, 255)     # bright sky blue
BOTTOM = (28, 106, 208, 255)  # NDM blue
WHITE = (255, 255, 255, 255)

def master():
    # bright vertical-gradient rounded square (the old charcoal tile
    # read as dull and dark at small sizes — user feedback, 28 Oct)
    img = Image.new("RGBA", (SS, SS), (0, 0, 0, 0))
    grad = Image.new("RGBA", (1, SS))
    for y in range(SS):
        t = y / (SS - 1)
        grad.putpixel((0, y), tuple(
            round(TOP[i] + (BOTTOM[i] - TOP[i]) * t) for i in range(4)))
    tile = grad.resize((SS, SS))
    mask = Image.new("L", (SS, SS), 0)
    md = ImageDraw.Draw(mask)
    rad = SS * 0.18
    md.rounded_rectangle([0, 0, SS - 1, SS - 1], radius=rad, fill=255)
    img.paste(tile, (0, 0), mask)
    d = ImageDraw.Draw(img)
    # glass sheen: a soft white wash over the top third
    sheen = Image.new("RGBA", (SS, SS), (0, 0, 0, 0))
    sd = ImageDraw.Draw(sheen)
    sd.ellipse([-SS * 0.25, -SS * 0.62, SS * 1.25, SS * 0.34],
               fill=(255, 255, 255, 46))
    img.alpha_composite(sheen)
    d = ImageDraw.Draw(img)
    # the arrow + base bar, white, with a faint blue-grey shadow for lift
    cx = SS / 2
    shaft_w = SS * 0.17
    head_half = SS * 0.315
    for dy, col in [(SS * 0.012, (13, 62, 120, 110)), (0, WHITE)]:
        d.rectangle([cx - shaft_w / 2, SS * 0.16 + dy,
                     cx + shaft_w / 2, SS * 0.56 + dy], fill=col)
        d.polygon([(cx - head_half, SS * 0.47 + dy),
                   (cx + head_half, SS * 0.47 + dy),
                   (cx, SS * 0.79 + dy)], fill=col)
        d.rectangle([cx - head_half, SS * 0.855 + dy,
                     cx + head_half, SS * 0.855 + dy + SS * 0.062], fill=col)
    return img

def save(img, path, size):
    img.resize((size, size), Image.LANCZOS).save(path)
    print("wrote", path, size)

m = master()
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "icon-out")
os.makedirs(out, exist_ok=True)
save(m, f"{out}/icon.png", 512)
save(m, f"{out}/128x128@2x.png", 256)
save(m, f"{out}/128x128.png", 128)
save(m, f"{out}/64x64.png", 64)
save(m, f"{out}/32x32.png", 32)
save(m, f"{out}/icon128.png", 128)
save(m, f"{out}/icon48.png", 48)
save(m, f"{out}/icon16.png", 16)
# Windows .ico with the full size ladder embedded
m.save(f"{out}/icon.ico", format="ICO",
       sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
print("wrote icon.ico")
