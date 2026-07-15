#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
修复 MinGW 构建 exe 中「多个非默认清单」冲突。

MinGW 的启动文件会自带一份默认 manifest（不含 comctl32 v6），而 embed.rc 又嵌入
我们需要的 app.manifest。两者在 .rsrc 节中并存，导致 Windows 可能选错清单，
从而出现 GetWindowSubclass 等入口点缺失错误。

本脚本解析 PE 资源目录，找出所有 RT_MANIFEST(24) 资源，保留包含
"Microsoft.Windows.Common-Controls" 的清单，其余清零并设 size=0。
"""
import os
import sys
import struct
from pathlib import Path

RT_MANIFEST = 24

def parse_pe(data: bytes) -> dict:
    """解析 PE 头，返回 {'rsrc_rva': int, 'rsrc_size': int, 'sections': [...]}"""
    if data[:2] != b'MZ':
        raise ValueError('不是 PE 文件（缺少 MZ 头）')
    e_lfanew = struct.unpack_from('<I', data, 0x3C)[0]
    if data[e_lfanew:e_lfanew+4] != b'PE\x00\x00':
        raise ValueError('不是 PE 文件（缺少 PE 签名）')

    pe32plus = None
    magic = struct.unpack_from('<H', data, e_lfanew + 24)[0]
    if magic == 0x20B:
        pe32plus = True
    elif magic == 0x10B:
        pe32plus = False
    else:
        raise ValueError(f'未知 PE magic: 0x{magic:x}')

    size_of_optional_header = struct.unpack_from('<H', data, e_lfanew + 20)[0]
    num_sections = struct.unpack_from('<H', data, e_lfanew + 6)[0]

    if pe32plus:
        data_dir_offset = e_lfanew + 24 + 112
    else:
        data_dir_offset = e_lfanew + 24 + 96

    # DataDirectory[2] = Resource directory
    rsrc_rva = struct.unpack_from('<I', data, data_dir_offset + 2 * 8)[0]
    rsrc_size = struct.unpack_from('<I', data, data_dir_offset + 2 * 8 + 4)[0]

    sections = []
    section_headers_offset = e_lfanew + 24 + size_of_optional_header
    for i in range(num_sections):
        off = section_headers_offset + i * 40
        name = data[off:off+8].rstrip(b'\x00').decode('ascii', errors='ignore')
        virtual_size = struct.unpack_from('<I', data, off + 8)[0]
        virtual_addr = struct.unpack_from('<I', data, off + 12)[0]
        size_of_raw_data = struct.unpack_from('<I', data, off + 16)[0]
        pointer_to_raw_data = struct.unpack_from('<I', data, off + 20)[0]
        sections.append({
            'name': name,
            'virtual_size': virtual_size,
            'virtual_addr': virtual_addr,
            'size_of_raw_data': size_of_raw_data,
            'pointer_to_raw_data': pointer_to_raw_data,
        })

    return {'rsrc_rva': rsrc_rva, 'rsrc_size': rsrc_size, 'sections': sections}


def rva_to_file_offset(rva: int, sections: list) -> int:
    for s in sections:
        if s['virtual_addr'] <= rva < s['virtual_addr'] + s['virtual_size']:
            return rva - s['virtual_addr'] + s['pointer_to_raw_data']
    raise ValueError(f'RVA {rva:x} 不在任何节内')


def read_dir_entry(data: bytes, offset: int) -> tuple:
    """返回 (name_or_id, is_subdirectory, offset_to_data_or_subdir)"""
    name_or_id = struct.unpack_from('<I', data, offset)[0]
    second = struct.unpack_from('<I', data, offset + 4)[0]
    is_subdir = (second & 0x80000000) != 0
    target = second & 0x7FFFFFFF
    return name_or_id, is_subdir, target


def parse_resource_dir(data: bytes, file_offset: int, sections: list, rsrc_file_offset: int, depth: int = 0, path: list = None) -> list:
    """递归遍历资源目录，返回所有 RT_MANIFEST 数据项列表。"""
    if path is None:
        path = []
    results = []
    if depth >= 3:
        return results

    num_named = struct.unpack_from('<H', data, file_offset + 12)[0]
    num_id = struct.unpack_from('<H', data, file_offset + 14)[0]
    entry_offset = file_offset + 16

    for i in range(num_named + num_id):
        name_or_id, is_subdir, target = read_dir_entry(data, entry_offset + i * 8)
        current_path = path + [name_or_id]
        if is_subdir:
            # target 是相对于资源目录起始的偏移，不是 RVA
            sub_file_off = rsrc_file_offset + target
            results.extend(parse_resource_dir(data, sub_file_off, sections, rsrc_file_offset, depth + 1, current_path))
        else:
            # data entry: 4 bytes DataRVA, 4 bytes Size, 4 bytes CodePage, 4 bytes Reserved
            # target 是相对于资源目录起始的偏移
            entry_off = rsrc_file_offset + target
            data_rva = struct.unpack_from('<I', data, entry_off)[0]
            size = struct.unpack_from('<I', data, entry_off + 4)[0]
            codepage = struct.unpack_from('<I', data, entry_off + 8)[0]
            data_file_off = rva_to_file_offset(data_rva, sections)
            results.append({
                'type': current_path[0] if len(current_path) > 0 else None,
                'id': current_path[1] if len(current_path) > 1 else None,
                'lang': current_path[2] if len(current_path) > 2 else None,
                'rva': data_rva,
                'size': size,
                'codepage': codepage,
                'data_file_offset': data_file_off,
                'entry_file_offset': entry_off,
            })
    return results


def fix_manifest(exe_path: Path):
    data = bytearray(exe_path.read_bytes())
    pe = parse_pe(data)
    if pe['rsrc_rva'] == 0:
        print('未找到资源目录，跳过')
        return

    rsrc_file_offset = rva_to_file_offset(pe['rsrc_rva'], pe['sections'])
    entries = parse_resource_dir(data, rsrc_file_offset, pe['sections'], rsrc_file_offset)
    manifest_entries = [e for e in entries if e['type'] == RT_MANIFEST]

    if not manifest_entries:
        print('未找到 RT_MANIFEST 资源')
        return

    kept = 0
    removed = 0
    for e in manifest_entries:
        content = bytes(data[e['data_file_offset']:e['data_file_offset'] + e['size']])
        if b'Microsoft.Windows.Common-Controls' in content:
            kept += 1
            print(f'保留 manifest id={e["id"]} lang={e["lang"]} size={e["size"]}')
        else:
            removed += 1
            print(f'清零默认 manifest id={e["id"]} lang={e["lang"]} size={e["size"]}')
            # 清零数据内容
            for i in range(e['size']):
                data[e['data_file_offset'] + i] = 0
            # 将数据项 size 设为 0
            struct.pack_into('<I', data, e['entry_file_offset'] + 4, 0)

    if removed:
        exe_path.write_bytes(data)
        print(f'完成：保留 {kept} 个，清零 {removed} 个默认清单')
    else:
        print(f'无需处理，已保留 {kept} 个清单')


if __name__ == '__main__':
    if len(sys.argv) > 1:
        p = Path(sys.argv[1])
    else:
        p = Path(__file__).parent / 'target' / 'release' / 'SingBoxCommander.exe'
    if not p.exists():
        print(f'文件不存在：{p}')
        sys.exit(1)
    fix_manifest(p)
