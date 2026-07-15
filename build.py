#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Release 构建包装脚本（MinGW 目标）。

MinGW 链接时会把自己的默认 manifest 和我们的 app.manifest 一起合并进 PE，
Windows 可能因此选错清单，导致 GetWindowSubclass 等入口点缺失。
本脚本在 cargo build 之后调用 fix_pe_manifest.py 清理默认清单，
确保最终 exe 只保留 comctl32 v6 清单。
"""
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).parent.resolve()
MINGW_BIN = Path.home() / "mingw64" / "mingw64" / "bin"
CARGO_BIN = Path.home() / ".cargo" / "bin"


def main():
    env = os.environ.copy()
    env["PATH"] = f"{MINGW_BIN}{os.pathsep}{CARGO_BIN}{os.pathsep}{env.get('PATH', '')}"

    print("==> 设置默认 toolchain 为 stable-x86_64-pc-windows-gnu")
    subprocess.run([str(CARGO_BIN / "rustup"), "default", "stable-x86_64-pc-windows-gnu"], check=True, env=env)

    print("==> cargo build --release")
    subprocess.run([str(CARGO_BIN / "cargo"), "build", "--release"], cwd=ROOT, check=True, env=env)

    exe = ROOT / "target" / "release" / "SingBoxCommander.exe"
    print(f"==> 清理 PE 默认清单：{exe}")
    subprocess.run([sys.executable, str(ROOT / "fix_pe_manifest.py"), str(exe)], check=True)

    print(f"\n构建完成：{exe}")
    print(f"大小：{exe.stat().st_size / 1024:.1f} KB")


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as e:
        print(f"构建失败：{e}")
        sys.exit(1)
