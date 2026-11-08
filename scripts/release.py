#!/usr/bin/env python3
"""Create a local release tag or update the unified workspace version."""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SEMVER = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\Z")


def packages():
    output = subprocess.check_output(["cargo", "metadata", "--no-deps", "--offline", "--format-version", "1"], cwd=ROOT)
    data = json.loads(output)
    return [p for p in data["packages"] if p["id"] in data["workspace_members"]]


def tag():
    pkgs = packages()
    versions = {p["version"] for p in pkgs}
    if len(versions) != 1:
        raise ValueError("workspace 包版本不一致")
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT).strip():
        raise ValueError("请先提交工作区改动，再创建发布 tag")
    version = next(iter(versions))
    name = version
    subprocess.run(["git", "tag", "-a", name, "-m", "Release " + name], cwd=ROOT, check=True)
    print(f"已创建本地 tag {name}；确认远端后执行 git push <remote> refs/tags/{name}。")


def bump(version):
    prerelease = version.split("+", 1)[0].partition("-")[2]
    if not SEMVER.fullmatch(version) or any(x.isdigit() and len(x) > 1 and x.startswith("0") for x in prerelease.split(".") if x):
        raise ValueError("版本必须是合法的 SemVer，例如 0.2.0 或 1.0.0-rc.1")
    pkgs = packages()
    files = [ROOT / "Cargo.toml"] + [Path(p["manifest_path"]) for p in pkgs]
    lock = ROOT / "Cargo.lock"
    originals = {p: p.read_bytes() for p in files + ([lock] if lock.exists() else [])}
    try:
        p = files[0]
        text, count = re.subn(r'(?ms)(^\[workspace\.package\]\s*\n(?:(?!^\[).)*?^version\s*=\s*)"[^"]+"', lambda m: m.group(1) + '"' + version + '"', p.read_text(), count=1)
        if count != 1:
            raise ValueError("无法定位 workspace.package.version")
        p.write_text(text)
        subprocess.run(["cargo", "update", "--workspace", "--offline"], cwd=ROOT, check=True)
        if any(p["version"] != version for p in packages()):
            raise ValueError("workspace 包版本不一致")
    except (Exception, KeyboardInterrupt):
        for p, content in originals.items():
            p.write_bytes(content)
        raise
    print(f"已更新全部 workspace crate 版本至 {version}，并更新 Cargo.lock。")


parser = argparse.ArgumentParser(description=__doc__)
sub = parser.add_subparsers(dest="command", required=True)
sub.add_parser("tag", help="为当前统一版本创建本地附注 tag，不推送")
version_parser = sub.add_parser("version", help="统一更新 workspace 版本和锁文件")
version_parser.add_argument("version")
args = parser.parse_args()
try:
    tag() if args.command == "tag" else bump(args.version)
except (subprocess.CalledProcessError, ValueError) as exc:
    print(f"错误: {exc}", file=sys.stderr)
    sys.exit(1)
