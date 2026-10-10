"""Draws the SimpleSTT icon and writes assets/icon.ico + assets/icon.res.

The .res is linked straight into the exe by build.rs, so normal builds need neither Python nor rc.exe.
Re-run this only when you change the artwork:  python tools/make_icon.py   (needs Pillow)
"""
import io
import os
import struct

from PIL import Image, ImageDraw

S = 1024  # supersampled canvas
BG, GREEN, WHITE = (0x1F, 0x1F, 0x26, 255), (0x30, 0xA4, 0x6C, 255), (255, 255, 255, 255)


def draw() -> Image.Image:
    im = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    d.rounded_rectangle((0, 0, S - 1, S - 1), radius=int(S * 0.22), fill=BG)
    c = S // 2
    r = int(S * 0.355)
    d.ellipse((c - r, c - r, c + r, c + r), fill=GREEN)
    # microphone: capsule, U-shaped holder, stem, base
    w = 66
    d.rounded_rectangle((c - w, 290, c + w, 560), radius=w, fill=WHITE)
    d.arc((c - 135, 420, c + 135, 650), start=0, end=180, fill=WHITE, width=36)
    d.rectangle((c - 18, 640, c + 18, 710), fill=WHITE)
    d.rounded_rectangle((c - 78, 695, c + 78, 731), radius=18, fill=WHITE)
    return im


SIZES = [16, 24, 32, 48, 256]


def png_bytes(im: Image.Image) -> bytes:
    b = io.BytesIO()
    # flat artwork: a small palette + optimised PNG is ~10x smaller than truecolour
    im.quantize(48, method=Image.Quantize.FASTOCTREE).save(b, "PNG", optimize=True)
    return b.getvalue()


def dib_bytes(im: Image.Image) -> bytes:
    """Classic 32-bit icon image: BITMAPINFOHEADER + bottom-up BGRA + 1-bit AND mask."""
    w, h = im.size
    px = im.convert("RGBA").load()
    rows = bytearray()
    for y in range(h - 1, -1, -1):
        for x in range(w):
            r, g, b, a = px[x, y]
            rows += bytes((b, g, r, a))
    mask = bytes(((w + 31) // 32 * 4) * h)
    hdr = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, len(rows) + len(mask), 0, 0, 0, 0)
    return hdr + bytes(rows) + mask


def res_entry(rtype: int, name: int, data: bytes) -> bytes:
    head = struct.pack("<IIHHHHIHHII", len(data), 32, 0xFFFF, rtype, 0xFFFF, name, 0, 0x0030, 0, 0, 0)
    return head + data + b"\0" * (-len(data) % 4)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    out = os.path.join(here, "..", "assets")
    os.makedirs(out, exist_ok=True)
    big = draw()
    imgs = {s: big.resize((s, s), Image.LANCZOS) for s in SIZES}
    data = {s: (png_bytes(imgs[s]) if s >= 256 else dib_bytes(imgs[s])) for s in SIZES}

    # --- .ico (handy for previews / other tools)
    ico = struct.pack("<HHH", 0, 1, len(SIZES))
    offset = 6 + 16 * len(SIZES)
    blobs = b""
    for s in SIZES:
        d = data[s]
        ico += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(d), offset + len(blobs))
        blobs += d
    open(os.path.join(out, "icon.ico"), "wb").write(ico + blobs)

    # --- .res: RT_ICON images + one RT_GROUP_ICON (id 1 = the exe's icon)
    res = struct.pack("<IIHHHHIHHII", 0, 32, 0xFFFF, 0, 0xFFFF, 0, 0, 0, 0, 0, 0)  # mandatory empty header entry
    grp = struct.pack("<HHH", 0, 1, len(SIZES))
    for i, s in enumerate(SIZES, start=1):
        res += res_entry(3, i, data[s])  # RT_ICON
        grp += struct.pack("<BBBBHHIH", s % 256, s % 256, 0, 0, 1, 32, len(data[s]), i)
    res += res_entry(14, 1, grp)  # RT_GROUP_ICON
    open(os.path.join(out, "icon.res"), "wb").write(res)

    big.resize((256, 256), Image.LANCZOS).save(os.path.join(out, "icon-preview.png"))
    print("wrote assets/icon.ico, assets/icon.res, assets/icon-preview.png")


if __name__ == "__main__":
    main()
