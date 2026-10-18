"""Renders the Sonar Deck icon: icon.png (256), icon.ico (16-256) and tray.rgba (32x32 raw RGBA).

Run with any Python that has Pillow:  python assets/make_icon.py
"""
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

OUT = Path(__file__).resolve().parent
S = 1024  # drawn at 4x and downsampled for smooth edges


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def render(heights=(0.34, 0.60, 0.82, 0.60, 0.34), bar_w=108, gap=44) -> Image.Image:
    # Diagonal orange gradient, clipped to a rounded square.
    top, bottom = (255, 74, 28), (255, 170, 60)
    grad = Image.new("RGBA", (S, S))
    px = grad.load()
    for y in range(S):
        for x in range(S):
            t = (x * 0.35 + y * 0.65) / S
            px[x, y] = (*lerp(top, bottom, t), 255)

    pad = 40
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle((pad, pad, S - pad, S - pad), radius=230, fill=255)

    icon = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    icon.paste(grad, (0, 0), mask)

    # Soft highlight across the top half.
    gloss = Image.new("L", (S, S), 0)
    ImageDraw.Draw(gloss).ellipse((-S * 0.4, -S * 0.9, S * 1.4, S * 0.5), fill=46)
    gloss = ImageChops.multiply(gloss.filter(ImageFilter.GaussianBlur(160)), mask)
    icon = Image.composite(Image.new("RGBA", (S, S), (255, 255, 255, 255)), icon, gloss)
    icon.putalpha(mask)

    # Waveform bars with a subtle drop shadow.
    total = len(heights) * bar_w + (len(heights) - 1) * gap
    x0 = (S - total) // 2
    bars = Image.new("L", (S, S), 0)
    d = ImageDraw.Draw(bars)
    for i, h in enumerate(heights):
        x = x0 + i * (bar_w + gap)
        half = h * (S - 2 * pad) / 2
        d.rounded_rectangle((x, S / 2 - half, x + bar_w, S / 2 + half), radius=bar_w // 2, fill=255)

    shadow = bars.filter(ImageFilter.GaussianBlur(18)).point(lambda v: v * 0.35)
    shadow = ImageChops.offset(shadow, 0, 14)
    icon = Image.composite(Image.new("RGBA", (S, S), (120, 30, 0, 255)), icon, ImageChops.multiply(shadow, mask))
    icon = Image.composite(Image.new("RGBA", (S, S), (255, 255, 255, 255)), icon, bars)
    icon.putalpha(mask)
    return icon


def main():
    big = render()
    # Small sizes get three bold bars so they stay readable in the tray and taskbar.
    small = render(heights=(0.44, 0.78, 0.44), bar_w=170, gap=90)
    size = lambda img, n: img.resize((n, n), Image.LANCZOS)
    size(big, 256).save(OUT / "icon.png")
    frames = [size(small, n) for n in (16, 24, 32)] + [size(big, n) for n in (48, 64, 128, 256)]
    frames[-1].save(OUT / "icon.ico", format="ICO", sizes=[f.size for f in frames], append_images=frames[:-1])
    (OUT / "tray.rgba").write_bytes(size(small, 32).tobytes())
    print("wrote icon.png, icon.ico, tray.rgba")


if __name__ == "__main__":
    main()
