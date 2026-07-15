#!/usr/bin/env python3
# 生成程序文件图标 app.ico（绿色地球/绿星球，多尺寸 32 位 RGBA，带正确 alpha 遮罩）。
# 纯标准库实现，不依赖 Pillow。
# app.ico 经 embed.rc 的 `1 ICON "app.ico"` 内嵌进 exe，作为资源管理器中的 exe 文件图标
# （Windows 取资源内数字 ID 最小的图标组作为文件图标，故用 ID 1）。
import struct, os

def make_globe(size):
    """生成 size×size 的 RGBA 绿色地球图标像素（圆外透明）。"""
    w = h = size
    cx = cy = (size - 1) / 2.0
    radius = size * 0.46
    line_half = max(0.035, 1.1 / size)  # 经纬网格半宽（随尺寸收敛）
    buf = bytearray()
    for y in range(h):
        for x in range(w):
            dx = (x - cx) / radius
            dy = (y - cy) / radius
            d2 = dx * dx + dy * dy
            if d2 > 1.0:
                buf += bytes([0, 0, 0, 0])
                continue
            z = (1.0 - d2) ** 0.5  # 球面深度（朝前为 1，边缘为 0）
            # 基础绿色球体渐变（按 z 提亮，制造立体感）
            R = 34 + 72 * z
            G = 120 + 120 * z
            B = 60 + 70 * z
            # 左上高光（镜面反射）
            hl = max(0.0, 1.0 - ((dx + 0.45) ** 2 + (dy + 0.45) ** 2) / (2 * 0.33 ** 2))
            R += 150 * hl
            G += 150 * hl
            B += 150 * hl
            # 经纬网格（仅朝前半球 z>0.05，随朝向变暗以显纵深）
            on_lat = any(abs(dy - v) < line_half for v in (0.0, 0.5))
            on_lon = any(abs(dx - v) < line_half for v in (0.0, 0.5))
            if (on_lat or on_lon) and z > 0.05:
                R *= 0.5
                G *= 0.5
                B *= 0.5
            buf += bytes([min(255, int(R)), min(255, int(G)), min(255, int(B)), 255])
    return w, h, bytes(buf)

def pack_ico(images):
    """images: list of (w, h, rgba_bytes) -> 标准 .ico 文件字节。"""
    entries = []
    blocks = []
    offset = 6 + 16 * len(images)
    for (w, h, rgba) in images:
        xor = bytearray()
        for y in range(h - 1, -1, -1):  # 自底向上
            row = rgba[y * w * 4:(y + 1) * w * 4]
            for i in range(w):
                rr, gg, bb, aa = row[i * 4:i * 4 + 4]
                xor += bytes([bb, gg, rr, aa])  # BGRA
        mask_row = ((w + 31) // 32) * 4
        and_mask = bytearray(mask_row * h)  # 全 0：透明度交给 alpha 通道
        bi_size_image = len(xor) + len(and_mask)
        dib = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, bi_size_image, 0, 0, 0, 0)
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
    # 含 256 档，让资源管理器大图标视图也清晰
    sizes = [16, 32, 48, 256]
    data = pack_ico([make_globe(s) for s in sizes])
    path = "app.ico"
    with open(path, "wb") as f:
        f.write(data)
    print(f"wrote {path} ({len(data)} bytes, sizes={sizes})")

if __name__ == "__main__":
    main()
