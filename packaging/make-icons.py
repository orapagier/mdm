#!/usr/bin/env python3
"""Draw and resize MDM's icon into every size the app and the extension need.

The artwork of record is `icon-source.png` beside this script — a 512px "m" in
the Rye face, from the favicon set the app is branded with. Everything else is
derived from it here so the sizes can never drift apart.

Two variants, because one drawing cannot serve every size. Above `HAND_DRAWN`
the blackletter is simply resized: there are pixels enough for its flourishes.
At panel and toolbar sizes those same flourishes fall below the pixel grid and
average into a smudge, so the small icons are drawn instead — the same letter
reduced to what survives: three stems under a bar, laid out on whole pixels so
every edge is sharp. Same disc, same blue, no blur.

No image library: PNG in, box-filtered down, PNG out. Alpha is premultiplied
before averaging, which is what keeps the edges of the disc clean instead of
bleeding the transparent corners into the blue.
"""
import struct
import sys
import zlib
from pathlib import Path

SIZES = (16, 24, 32, 48, 64, 128, 256, 512)

# The largest size still drawn rather than resized. At 48 the blackletter
# survives the reduction; at 32 it does not.
HAND_DRAWN = 32

# Sampled from the artwork, so the two variants are the same blue.
DISC = (32, 156, 238)
GLYPH = (255, 255, 255)
SS = 4  # supersampling, for the disc's edge only


def read_png(path):
    """Return (width, height, RGBA bytes) for an 8-bit, non-interlaced PNG."""
    data = path.read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"{path} is not a PNG")

    pos, idat, header, palette, alpha = 8, bytearray(), None, None, None
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos : pos + 4])
        tag = data[pos + 4 : pos + 8]
        chunk = data[pos + 8 : pos + 8 + length]
        pos += 12 + length  # length, tag, data, CRC
        if tag == b"IHDR":
            header = struct.unpack(">IIBBBBB", chunk)
        elif tag == b"IDAT":
            idat += chunk
        elif tag == b"PLTE":
            palette = chunk
        elif tag == b"tRNS":
            alpha = chunk
        elif tag == b"IEND":
            break

    w, h, depth, colour, _, _, interlace = header
    if depth != 8 or interlace:
        raise SystemExit(f"{path}: only 8-bit, non-interlaced PNGs are handled")
    channels = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}.get(colour)
    if channels is None:
        raise SystemExit(f"{path}: colour type {colour} is not handled")

    raw = zlib.decompress(bytes(idat))
    stride = w * channels
    rows, prev, pos = [], bytearray(stride), 0
    for _ in range(h):
        kind = raw[pos]
        line = bytearray(raw[pos + 1 : pos + 1 + stride])
        pos += 1 + stride
        # Undo the per-line filter (PNG spec §9.2). Left and upper-left read as
        # zero off the edge of the image.
        if kind == 1:
            for i in range(channels, stride):
                line[i] = (line[i] + line[i - channels]) & 255
        elif kind == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 255
        elif kind == 3:
            for i in range(stride):
                left = line[i - channels] if i >= channels else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 255
        elif kind == 4:
            for i in range(stride):
                left = line[i - channels] if i >= channels else 0
                up = prev[i]
                corner = prev[i - channels] if i >= channels else 0
                pa = abs(up - corner)
                pb = abs(left - corner)
                pc = abs(left + up - 2 * corner)
                if pa <= pb and pa <= pc:
                    guess = left
                elif pb <= pc:
                    guess = up
                else:
                    guess = corner
                line[i] = (line[i] + guess) & 255
        elif kind:
            raise SystemExit(f"{path}: unknown filter {kind}")
        rows.append(bytes(line))
        prev = line

    # Widen whatever came in to straight RGBA.
    out = bytearray(w * h * 4)
    for y, line in enumerate(rows):
        for x in range(w):
            i = (y * w + x) * 4
            if colour == 6:
                out[i : i + 4] = line[x * 4 : x * 4 + 4]
            elif colour == 2:
                out[i : i + 3] = line[x * 3 : x * 3 + 3]
                out[i + 3] = 255
            elif colour == 3:
                idx = line[x]
                out[i : i + 3] = palette[idx * 3 : idx * 3 + 3]
                out[i + 3] = alpha[idx] if alpha and idx < len(alpha) else 255
            elif colour == 0:
                out[i] = out[i + 1] = out[i + 2] = line[x]
                out[i + 3] = 255
            else:  # 4: grey + alpha
                out[i] = out[i + 1] = out[i + 2] = line[x * 2]
                out[i + 3] = line[x * 2 + 1]
    return w, h, bytes(out)


def draw(size):
    """The letter reduced to three stems under a bar, on exact pixel edges.

    Everything is a whole number of pixels: a stem that lands on half a pixel
    is a grey stem, and grey stems at 16px are the smudge this exists to avoid.
    """
    unit = max(1, round(size / 8))     # stem width, gap width, bar height
    width = 5 * unit                   # three stems, two gaps
    height = 4 * unit                  # the bar, then the stems
    left = (size - width) // 2
    top = (size - height) // 2
    stems = [(left + i * 2 * unit, left + i * 2 * unit + unit) for i in range(3)]

    def is_glyph(px, py):
        if not (top <= py < top + height):
            return False
        if py < top + unit:            # the bar joins all three
            return left <= px < left + width
        return any(x0 <= px < x1 for x0, x1 in stems)

    radius = size / 2
    rows = []
    for py in range(size):
        row = bytearray()
        for px in range(size):
            if is_glyph(px, py):
                row += bytes(GLYPH) + b"\xff"
                continue
            # Only the disc's edge needs softening, and only it gets it.
            covered = 0
            for sy in range(SS):
                for sx in range(SS):
                    dx = px + (sx + 0.5) / SS - radius
                    dy = py + (sy + 0.5) / SS - radius
                    if dx * dx + dy * dy <= radius * radius:
                        covered += 1
            row += bytes(DISC) + bytes([covered * 255 // (SS * SS)])
        rows.append(bytes(row))
    return rows


def resize(src, sw, sh, size):
    """Average each destination pixel over the source pixels it covers."""
    rows = []
    for oy in range(size):
        y0, y1 = oy * sh // size, max(oy * sh // size + 1, (oy + 1) * sh // size)
        row = bytearray()
        for ox in range(size):
            x0, x1 = ox * sw // size, max(ox * sw // size + 1, (ox + 1) * sw // size)
            r = g = b = a = n = 0
            for y in range(y0, y1):
                base = y * sw * 4
                for x in range(x0, x1):
                    i = base + x * 4
                    weight = src[i + 3]
                    r += src[i] * weight
                    g += src[i + 1] * weight
                    b += src[i + 2] * weight
                    a += weight
                    n += 1
            # Colour is averaged over coverage, opacity over area: a pixel half
            # off the disc keeps the blue it has rather than fading towards it.
            row += bytes(4) if a == 0 else bytes((r // a, g // a, b // a, a // n))
        rows.append(bytes(row))
    return rows


def write_png(path, size, rows):
    raw = b"".join(b"\x00" + r for r in rows)

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    path.write_bytes(png)


def main():
    out = Path(sys.argv[1] if len(sys.argv) > 1 else ".")
    source = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(__file__).with_name("icon-source.png")
    out.mkdir(parents=True, exist_ok=True)
    w, h, pixels = read_png(source)
    for size in SIZES:
        rows = draw(size) if size <= HAND_DRAWN else resize(pixels, w, h, size)
        write_png(out / f"mdm-{size}.png", size, rows)
        print(f"  {out}/mdm-{size}.png  {'drawn' if size <= HAND_DRAWN else 'resized'}")


if __name__ == "__main__":
    main()
