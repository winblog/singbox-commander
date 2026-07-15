#!/usr/bin/env python3
# 生成托盘图标 green.ico / red.ico（多尺寸 32 位 RGBA，带正确 alpha 遮罩）。
# 纯标准库实现，不依赖 Pillow。
# 托盘图标必须用 .ico 而非裸 PNG：native-windows-gui 的 Icon::source_bin(PNG) 走 WIC→
# CreateIconIndirect 时把同一张位图同时当 hbmMask/hbmColor，导致 Win11 任务栏显示黑方块；
# 改用 Icon::source_file(.ico) 走 LoadImageW 原生加载器，Win11 下 alpha 正常。
import struct, os

def make_rgba(size, color):
    """生成 size×size 的 RGBA 圆形图标像素（圆内不透明，圆外透明）。"""
    w = h = size
    r, g, b = color
    cx = cy = (size - 1) / 2.0
    radius = size * 0.44
    buf = bytearray()
    for y in range(h):
        for x in range(w):
            dx, dy = x - cx, y - cy
            d = (dx * dx + dy * dy) ** 0.5
            if d <= radius:
                # 边缘 2px 渐暗带，增加立体感
                if d > radius - 2:
                    rr, gg, bb = int(r * 0.7), int(g * 0.7), int(b * 0.7)
                else:
                    rr, gg, bb = r, g, b
                a = 255
            else:
                rr = gg = bb = 0
                a = 0
            buf += bytes([rr, gg, bb, a])
    return w, h, bytes(buf)

def pack_ico(images):
    """images: list of (w, h, rgba_bytes) -> 标准 .ico 文件字节。"""
    entries = []
    blocks = []
    offset = 6 + 16 * len(images)
    for (w, h, rgba) in images:
        # XOR 位图：自底向上、BGRA、32bpp
        xor = bytearray()
        for y in range(h - 1, -1, -1):
            row = rgba[y * w * 4:(y + 1) * w * 4]
            for i in range(w):
                rr, gg, bb, aa = row[i * 4:i * 4 + 4]
                xor += bytes([bb, gg, rr, aa])  # BGRA
        # AND 遮罩：1bpp、自底向上、每行 4 字节对齐；全 0（透明度交给 alpha 通道）
        mask_row = ((w + 31) // 32) * 4
        and_mask = bytearray(mask_row * h)
        dib = bytearray()
        bi_size_image = len(xor) + len(and_mask)
        dib += struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, bi_size_image, 0, 0, 0, 0)
        dib += xor
        dib += and_mask
        entries.append((w, h, len(dib), offset))
        blocks.append(bytes(dib))
        offset += len(dib)
    out = struct.pack("<HHH", 0, 1, len(images))
    for (w, h, size, off) in entries:
        bw = 0 if w >= 256 else w
        bh = 0 if h >= 256 else h
        out += struct.pack("<BBBBHHII", bw, bh, 0, 0, 1, 32, size, off)
    for blk in blocks:
        out += blk
    return out

def main():
    sizes = [16, 32, 48]
    os.makedirs("src", exist_ok=True)
    for name, color in (("green", (46, 204, 113)), ("red", (231, 76, 60))):
        imgs = [make_rgba(s, color) for s in sizes]
        data = pack_ico(imgs)
        path = f"src/{name}.ico"
        with open(path, "wb") as f:
            f.write(data)
        print(f"wrote {path} ({len(data)} bytes, sizes={sizes})")

if __name__ == "__main__":
    main()
