"""Draws the Shorui icon from the design system's colours and writes every size the app needs.

Run from the project root:  python assets/make_icon.py
"""
import os
from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
BG_100 = (0x0B, 0x0A, 0x0F, 255)
LINE = (0x2C, 0x28, 0x39, 255)
ACCENT = (0x7C, 0x3A, 0xED, 255)
ACCENT_BRIGHT = (0xA7, 0x8B, 0xFA, 255)
ON_ACCENT = (255, 255, 255, 235)


def draw(size: int) -> Image.Image:
    """The icon on a 256-unit grid, drawn at 4x and scaled down for clean edges."""
    s = 4 * size / 256
    img = Image.new("RGBA", (4 * size, 4 * size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    def rect(x0, y0, x1, y1, r, fill, outline=None, width=0):
        d.rounded_rectangle(
            [x0 * s, y0 * s, x1 * s, y1 * s], radius=r * s, fill=fill, outline=outline, width=max(1, round(width * s))
        )

    small = size <= 32
    # Tile: near-black with a hairline so it holds its shape on a dark taskbar.
    rect(8, 8, 248, 248, 54, BG_100, LINE, 3 if not small else 6)
    # Two sheets from the design system's cover: a page in front of another page.
    rect(124, 46, 206, 150, 16, ACCENT_BRIGHT)
    rect(56, 74, 168, 214, 18, ACCENT)
    if not small:
        for y, x1 in ((110, 146), (138, 146), (166, 120)):
            rect(78, y, x1, y + 12, 6, ON_ACCENT)
    return img.resize((size, size), Image.LANCZOS)


sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
images = {n: draw(n) for n in sizes}
images[256].save(os.path.join(HERE, "icon.ico"), format="ICO", append_images=[images[n] for n in sizes[:-1]],
                 sizes=[(n, n) for n in sizes])
images[256].save(os.path.join(HERE, "icon-256.png"))
# Raw RGBA for the window and taskbar icon, so the app needs no image decoder.
open(os.path.join(HERE, "icon-64.rgba"), "wb").write(images[64].tobytes())
print("wrote icon.ico, icon-256.png, icon-64.rgba")
