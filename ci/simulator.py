"""选择已安装的最新 iOS 运行时，逐台验证 iPhone 和 iPad。"""
import json
import plistlib
import re
import sqlite3
import subprocess
import sys
import time
from pathlib import Path


def sim(*args):
    return subprocess.check_output(["xcrun", "simctl", *map(str, args)], text=True).strip()


def select_devices():
    devices = json.loads(sim("list", "devices", "available", "-j"))["devices"]
    result = {}
    for runtime, entries in devices.items():
        match = re.search(r"\.iOS-(\d+(?:-\d+)*)$", runtime)
        if not match:
            continue
        version = tuple(map(int, match[1].split("-")))
        for entry in entries:
            kind = entry["name"].split(" ")[0]
            if kind in ("iPhone", "iPad") and (kind not in result or version > result[kind][0]):
                result[kind] = (version, entry)
    return [result[kind][1] for kind in ("iPhone", "iPad")]


def check_app(device, app, evidence):
    bundle = plistlib.loads((app / "Info.plist").read_bytes())["CFBundleIdentifier"]
    sim("install", device, app)
    output = sim("launch", device, bundle)
    pid = int(output.rsplit(":", 1)[1])
    support = Path(sim("get_app_container", device, bundle, "data")) / "Library/Application Support"
    for _ in range(60):
        for database in support.rglob("park.db"):
            with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True) as conn:
                if conn.execute("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='works'").fetchone()[0]:
                    subprocess.run(["kill", "-0", str(pid)], check=True)
                    sim("io", device, "screenshot", evidence)
                    return
        time.sleep(2)
    raise RuntimeError("书库未初始化")


def main():
    mode, = sys.argv[1:]
    if mode not in ("check", "launch"):
        raise ValueError(mode)
    app = Path("test/derived/Build/Products/Release-iphonesimulator/HentaiPark.app").resolve()
    for entry in select_devices():
        device, name = entry["udid"], entry["name"]
        print(name, flush=True)
        try:
            if entry["state"] != "Booted":
                sim("boot", device)
            sim("bootstatus", device, "-b")
            if mode == "launch":
                check_app(device, app, Path("test/evidence") / (name + ".png"))
        finally:
            subprocess.run(["xcrun", "simctl", "shutdown", device], check=False)


if __name__ == "__main__":
    main()
