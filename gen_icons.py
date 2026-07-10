import zlib, struct, os

def png_rgba(w, h, rgba):
    def chunk(typ, data):
        c = typ + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xffffffff)
    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        raw.extend(rgba[y*w*4:(y+1)*w*4])
    idat = zlib.compress(bytes(raw), 9)
    return sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")

def make_icon(color):
    w = h = 32
    r, g, b = color
    buf = bytearray()
    cx = cy = 16
    for y in range(h):
        for x in range(w):
            dx, dy = x - cx, y - cy
            d = (dx*dx + dy*dy) ** 0.5
            if d <= 14:
                rr, gg, bb = (int(r*0.7), int(g*0.7), int(b*0.7)) if d > 12 else (r, g, b)
                a = 255
            else:
                rr = gg = bb = 0
                a = 0
            buf += bytes([rr, gg, bb, a])
    return png_rgba(w, h, bytes(buf))

os.makedirs("src", exist_ok=True)
open("src/green.png", "wb").write(make_icon((46, 204, 113)))
open("src/red.png", "wb").write(make_icon((231, 76, 60)))
print("icons written: green.png, red.png")
