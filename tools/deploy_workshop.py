#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
deploy_workshop.py —— 把 release 构建发布到本地 workshop 目录 C:\\workshop\\aura-<版本>。

逻辑（与 aw-qtui / flowtary 的 deploy_workshop 保持一致）：

    0. 【构建前】justfile 先调 `--bump-only`：读 Cargo.toml 里 [package] 段的 version，
       把 patch 位 +1（如 1.2.0 -> 1.2.1）并写回，让接下来的 cargo build 带上新版本号；
    1. `--no-bump` 模式把 target\\release\\aura.exe 拷进 C:\\workshop\\aura-<ver>\\；
    2. 自检「目录名 == exe 内编译进去的 CARGO_PKG_VERSION」，不一致就报警。

为什么版本号必须在构建**之前**推进：

    exe 里的版本号来自 env!("CARGO_PKG_VERSION")（见 src/version.rs），是编译期常量。
    如果「先构建、再 bump、再拷贝」，那么 aura-<ver> 目录里的 exe 其实带着 <ver-1> 的号
    —— 目录名与二进制内部版本对不上，回过头压根分不清哪个目录是哪个构建。
    （aw-qtui 那边同一个坑还额外引发了单实例交接无声失败，见其 deploy_workshop.py 顶部。）

为什么 bump 之后不能再加 --locked：

    Cargo.lock 里记着根包自己的版本号（`name = "aura"` 那一条）。改了 Cargo.toml 的
    version 而 lock 没跟着变时，`cargo build --locked` 会直接报
    「the lock file ... needs to be updated but --locked was passed」。
    这里只是版本号变动、不涉及依赖解析，让 cargo 自己更新 lock 即可。

数据与环境分离：

    二进制放 C:\\workshop\\aura-<ver>\\；用户数据（cache/、state.json、aura-debug.log）
    仍然由程序自己写在 %LOCALAPPDATA%\\aura\\（src/config.rs 里硬编码 app_dir = data_local_dir/aura）。
    两者互不影响，替换构建不会动到数据。

用法（由 justfile 的 deploy-workshop 按此顺序调用）：

    python tools/deploy_workshop.py --bump-only   # 1) 先推进版本号并写回 Cargo.toml
    cargo build --release                         # 2) 用新版本号构建
    python tools/deploy_workshop.py --no-bump     # 3) 拷贝 + 自检
"""
import argparse
import os
import re
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

CARGO_TOML = os.path.join(ROOT, "Cargo.toml")
BINARY = os.path.join(ROOT, "target", "release", "aura.exe")

WORKSHOP = r"C:\workshop"
APP_NAME = "aura"
EXE_NAME = "aura.exe"


# ---------------- 版本号读写（只动 [package] 段，且逐字节保留其余内容） ----------------

def package_version_span(text):
    """定位 [package] 段里 version 的值区间，返回 (version, start, end)。

    必须限定在 [package] 段内：Cargo.toml 的依赖项（如 anyhow = "1.0"、hcl = {...}）
    也带 version 字段，全局搜会抓错地方。
    """
    section = re.search(r"(?ms)^\[package\][ \t]*$(.*?)(?=^\[|\Z)", text)
    if not section:
        raise SystemExit("Cargo.toml 里找不到 [package] 段")

    body_start = section.start(1)
    found = re.search(r'(?m)^[ \t]*version[ \t]*=[ \t]*"([^"]+)"', section.group(1))
    if not found:
        raise SystemExit("Cargo.toml 的 [package] 段里找不到 version")

    return found.group(1), body_start + found.start(1), body_start + found.end(1)


def bump_patch(version):
    """'1.2.0' -> '1.2.1'。只接受纯 x.y.z（带预发布后缀的一律拒绝，避免瞎猜）。"""
    parts = version.split(".")
    if len(parts) != 3 or not all(p.isdigit() for p in parts):
        raise SystemExit(f"版本号不是 x.y.z 形式，无法推进 patch：{version!r}")
    parts[2] = str(int(parts[2]) + 1)
    return ".".join(parts)


def read_file_raw(path):
    """按原样读（newline='' 关闭换行转换），避免写回时把 CRLF 变成 LF。"""
    with open(path, "r", encoding="utf-8", newline="") as fh:
        return fh.read()


def write_file_raw(path, text):
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)


def bump_in_file():
    """把 Cargo.toml 的 patch 位 +1 并写回，返回 (旧版本, 新版本)。"""
    text = read_file_raw(CARGO_TOML)
    base, start, end = package_version_span(text)
    new = bump_patch(base)
    write_file_raw(CARGO_TOML, text[:start] + new + text[end:])
    return base, new


# ---------------- 部署后自检 ----------------

def exe_reports_version(exe_path, version):
    """扫 exe 里是否含该版本串（UTF-8 字面量 / UTF-16 的 VERSIONINFO 资源）。

    只做「读文件 + 扫字节」，不执行 exe：aura 是个托盘壁纸程序，跑起来会直接换壁纸。
    """
    with open(exe_path, "rb") as fh:
        data = fh.read()

    # 数字边界包一层，免得 1.2.1 命中 11.2.10 这类子串
    if re.search(rb"(?<!\d)" + re.escape(version.encode("utf-8")) + rb"(?!\d)", data):
        return True
    return re.search(re.escape(version.encode("utf-16-le")), data) is not None


def running_instances():
    """正在运行的 aura.exe 的 pid 列表（只读探测，不打扰目标进程）。

    直接对 tasklist 的**原始字节**做匹配，不做文本解码：中文系统上 tasklist 输出是 GBK，
    而 subprocess 的 text=True 会按 UTF-8 去解（本机 python 开了 UTF-8 模式）——
    读线程当场抛 UnicodeDecodeError、stdout 变成 None。这里要提取的
    `"aura.exe","1234"` 本来就是纯 ASCII，扫字节最稳。

    探测失败一律返回空表：它只是个提示，绝不该因为探测不到就让整个部署报错。
    """
    try:
        proc = subprocess.run(
            ["tasklist", "/FI", "IMAGENAME eq " + EXE_NAME, "/FO", "CSV", "/NH"],
            capture_output=True, timeout=20,
        )
        return sorted(
            int(pid)
            for pid in re.findall(
                rb'"' + re.escape(EXE_NAME.encode("ascii")) + rb'","(\d+)"',
                proc.stdout,
            )
        )
    except Exception:
        return []


# ---------------- 主流程 ----------------

def main() -> None:
    ap = argparse.ArgumentParser(
        description="把 aura 的 release 构建发布到 C:\\workshop\\aura-<版本>")
    ap.add_argument("--bump-only", action="store_true",
                    help="只把 Cargo.toml 的 patch +1 并写回（构建前调用），不做拷贝")
    ap.add_argument("--no-bump", action="store_true",
                    help="按 Cargo.toml 当前版本部署、不再 +1（版本号已由 --bump-only 推进）")
    args = ap.parse_args()

    if args.bump_only and args.no_bump:
        raise SystemExit("--bump-only 与 --no-bump 不能同时使用")

    if args.bump_only:
        base, new = bump_in_file()
        print(f"[ver] 构建前先推进版本号：{base} -> {new}")
        return

    version, _, _ = package_version_span(read_file_raw(CARGO_TOML))
    if args.no_bump:
        print(f"[ver] 版本号: {version}（已由 --bump-only 提前推进，本次不再 +1）")
    else:
        base, version = bump_in_file()
        print(f"[ver] 版本号: {base} -> {version}（已写回 Cargo.toml）")

    if not os.path.isfile(BINARY):
        raise SystemExit(f"构建产物不存在：{BINARY}\n请先执行 cargo build --release")

    target = os.path.join(WORKSHOP, f"{APP_NAME}-{version}")
    if os.path.isdir(target):
        shutil.rmtree(target)
    os.makedirs(target, exist_ok=True)

    dest_exe = os.path.join(target, EXE_NAME)
    shutil.copy2(BINARY, dest_exe)

    print("[done] deploy-workshop 完成:")
    print(f"  {dest_exe}")

    # 自检 1：构建产物是否比 Cargo.toml 新（防止「根本没重新构建」）
    if os.path.getmtime(BINARY) < os.path.getmtime(CARGO_TOML):
        print("[warn] target\\release\\aura.exe 比 Cargo.toml 还旧 —— 这次很可能没有真正重建。")
        print("       先跑 cargo build --release，再执行 deploy-workshop。")

    # 自检 2：目录名必须等于 exe 内编译进去的版本号
    if not exe_reports_version(dest_exe, version):
        print(f"[warn] exe 内没扫到版本串 {version} —— 目录名与二进制版本对不上，")
        print("       大概率是「先构建后 bump」留下的旧产物，回头分不清哪个目录是哪个构建。")

    pids = running_instances()
    if pids:
        print(f"[note] 检测到 {len(pids)} 个 aura.exe 还在运行（pid {', '.join(map(str, pids))}）——")
        print("       它跑的是旧目录里的那份。先从托盘退出，再启动新版：")
        print(f"       {dest_exe}")


if __name__ == "__main__":
    sys.exit(main())
