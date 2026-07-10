"""纯标准库生成 SingBoxGUI 的 exe 图标（多尺寸 .ico）。

设计：绿色圆角方块（沿用托盘绿点配色 #2EC1A3? 这里用 (46,204,113)）
+ 白色「地球/代理」环 + 赤道线 + 一个节点点，表达「全局代理 / 节点网络」。
不依赖 Pillow，全部用 zlib/struct/math 手绘并做超采样抗锯齿。
"""
import struct, zlib, math, os

SS = 4  # 超采样倍数，用于抗锯齿

def ss(e0, e1, x):
    if e1 == e0:
        return 1.0 if x >= e0 else 0.0
    t = (x - e0) / (e1 - e0)
    t = 0.0 if t < 0 else (1.0 if t > 1 else t)
    return t * t * (3 - 2 * t)

def make_png_bytes(w, h, rgba):
    def chunk(typ, data):
        c = typ + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xffffffff)
    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        raw.extend(rgba[y * w * 4:(y + 1) * w * 4])
    idat = zlib.compress(bytes(raw), 9)
    return sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")

def write_png(path, w, h, rgba):
    open(path, "wb").write(make_png_bytes(w, h, rgba))

def render(size):
    S = float(size)
    R = size * SS
    green = (46, 204, 113)
    m = S * 0.12
    half = S / 2 - m
    rad = half * 0.42
    cx = S / 2; cy = S / 2
    rg = half * 0.66
    lw = S * 0.05
    aa = 1.4
    buf = bytearray(R * R * 4)
    for py in range(R):
        ny = (py + 0.5) / R * S
        for px in range(R):
            nx = (px + 0.5) / R * S
            # 背景：圆角矩形
            qx = abs(nx - cx) - (half - rad)
            qy = abs(ny - cy) - (half - rad)
            ox = max(qx, 0.0); oy = max(qy, 0.0)
            outside = math.hypot(ox, oy) - rad
            bg = ss(-aa, aa, -outside)
            gf = 1.10 - 0.20 * (ny / S)  # 顶部亮一点
            r = green[0] * gf
            g = green[1] * gf
            b = green[2] * gf
            a = bg
            # 白色地球特征（在背景内）
            gd = rg - math.hypot(nx - cx, ny - cy)  # >0 在圆内
            inside_outer = ss(-aa, aa, gd)
            inner_cover = ss(-aa, aa, gd - rg * 0.72)
            ring = inside_outer * inner_cover
            eq = (1 - ss(-aa, aa, abs(ny - cy) - lw)) * inside_outer
            # 节点点（右上）
            ddx = nx - (cx + rg * 0.5); ddy = ny - (cy - rg * 0.5)
            dot = ss(-aa, aa, rg * 0.20 - math.hypot(ddx, ddy))
            white = max(ring, eq, dot)
            if white > 0:
                r = g = b = 255.0
                a = max(a, white)
            o = (py * R + px) * 4
            buf[o] = max(0, min(255, int(r)))
            buf[o + 1] = max(0, min(255, int(g)))
            buf[o + 2] = max(0, min(255, int(b)))
            buf[o + 3] = max(0, min(255, int(a * 255)))
    # 降采样平均
    final = bytearray(size * size * 4)
    n = SS * SS
    for oy in range(size):
        for ox in range(size):
            rr = gg = bb = aa_ = 0
            for j in range(SS):
                for i in range(SS):
                    sx = ox * SS + i; sy = oy * SS + j
                    idx = (sy * R + sx) * 4
                    rr += buf[idx]; gg += buf[idx + 1]; bb += buf[idx + 2]; aa_ += buf[idx + 3]
            o = (oy * size + ox) * 4
            final[o] = rr // n; final[o + 1] = gg // n
            final[o + 2] = bb // n; final[o + 3] = aa_ // n
    return final

def bmp_ico_image(w, h, rgba_topdown):
    """构造经典 BMP 格式（非 PNG）的 32bpp 图标图像，供 rc.exe 3.00 格式消费。
    DIB 结构：BITMAPINFOHEADER(40) + XOR(BGRA, 自下而上) + AND 掩码(全 0, 自下而上)。
    """
    xor = bytearray()
    # 自下而上写入颜色行（row h-1 .. 0）
    for y in range(h - 1, -1, -1):
        row = rgba_topdown[y * w * 4:(y + 1) * w * 4]
        for x in range(w):
            o = x * 4
            r, g, b, a = row[o], row[o + 1], row[o + 2], row[o + 3]
            xor += bytes((b, g, r, a))  # BGRA
    # AND 掩码：每行 ceil(w/32)*4 字节，全 0（完全不透明）
    and_row = ((w + 31) // 32) * 4
    and_mask = b"\x00" * (and_row * h)
    dib_size = 40 + len(xor) + len(and_mask)
    dib = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, dib_size, 0, 0, 0, 0)
    return dib + xor + and_mask

def pack_ico(path, sizes):
    images = []
    for s in sizes:
        rgba = render(s)
        images.append((s, s, bmp_ico_image(s, s, rgba)))
    count = len(images)
    header = struct.pack("<HHH", 0, 1, count)  # ICONDIR: reserved WORD, type WORD(1), count WORD
    entries = b""
    offset = 6 + count * 16
    data = b""
    for (w, h, img) in images:
        bw = w if w < 256 else 0
        bh = h if h < 256 else 0
        entries += struct.pack("<BBBBHHII", bw, bh, 0, 0, 1, 32, len(img), offset)
        data += img
        offset += len(img)
    open(path, "wb").write(header + entries + data)
    print("ico written:", path, "sizes:", sizes)

if __name__ == "__main__":
    os.makedirs("src", exist_ok=True)
    pack_ico("app.ico", [16, 24, 32, 48, 64, 128, 256])
