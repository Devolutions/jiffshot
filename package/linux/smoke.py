#!/usr/bin/env python3
"""Launch a packaged app in a private Xvfb display without user settings."""

import argparse
import os
from pathlib import Path
import select
import shutil
import struct
import subprocess
import tempfile
import time
import zlib


def stop(process):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--launcher", type=Path, required=True)
    args = parser.parse_args()
    for tool in ("Xvfb", "xwininfo"):
        if shutil.which(tool) is None:
            parser.error(f"missing smoke-test tool: {tool}")
    sockets = Path("/tmp/.X11-unix")
    if sockets.exists() and (
        os.statvfs(sockets).f_flag & os.ST_RDONLY or not os.access(sockets, os.W_OK)
    ):
        parser.error("X11 sockets are read-only; run this smoke test in an isolated container/rootfs")
    launcher = args.launcher.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="jiffshot-smoke-") as temporary:
        work = Path(temporary)
        image = work / "input image with spaces.png"
        image.write_bytes(
            b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", 2, 2, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress((b"\x00" + b"\xff\x00\x00\xff" * 2) * 2))
            + chunk(b"IEND", b"")
        )
        runtime = work / "runtime"
        runtime.mkdir(mode=0o700)
        server = app = None
        read_fd, write_fd = os.pipe()
        with (work / "xvfb.log").open("wb") as server_log, (work / "app.log").open("wb") as app_log:
            try:
                server = subprocess.Popen(
                    ["Xvfb", "-displayfd", str(write_fd), "-screen", "0", "1280x900x24", "-nolisten", "tcp"],
                    pass_fds=(write_fd,), stdout=server_log, stderr=server_log,
                )
                os.close(write_fd)
                write_fd = None
                if not select.select([read_fd], [], [], 10)[0]:
                    raise RuntimeError("private Xvfb did not start")
                display = os.read(read_fd, 128).decode("ascii").strip()
                if not display.isdecimal():
                    raise RuntimeError(f"Xvfb returned an invalid display: {display!r}")
                env = {
                    **os.environ, "DISPLAY": f":{display}", "HOME": str(work),
                    "XDG_CONFIG_HOME": str(work / "config"), "XDG_DATA_HOME": str(work / "data"),
                    "XDG_CACHE_HOME": str(work / "cache"), "XDG_RUNTIME_DIR": str(runtime),
                }
                env.pop("WAYLAND_DISPLAY", None)
                env.pop("DBUS_SESSION_BUS_ADDRESS", None)
                app = subprocess.Popen(
                    [str(launcher), str(image)], cwd=work, env=env,
                    stdin=subprocess.DEVNULL, stdout=app_log, stderr=app_log,
                )
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    if app.poll() is not None:
                        raise RuntimeError(f"packaged app exited during startup ({app.returncode})")
                    windows = subprocess.run(
                        ["xwininfo", "-root", "-tree"], env=env, capture_output=True, text=True, check=True,
                    ).stdout
                    maps = Path(f"/proc/{app.pid}/maps").read_text()
                    if '"Jiffshot"' in windows and all(
                        library in maps for library in ("librustolonia_host.so", "libSkiaSharp.so", "libHarfBuzzSharp.so")
                    ):
                        print(f"Packaged app displayed its window and loaded all private runtimes: {launcher}")
                        return
                    time.sleep(0.2)
                raise RuntimeError("packaged app did not display its window and load all private runtimes")
            except (OSError, RuntimeError, subprocess.CalledProcessError):
                print((work / "xvfb.log").read_text(errors="replace")[-16384:])
                print((work / "app.log").read_text(errors="replace")[-16384:])
                raise
            finally:
                os.close(read_fd)
                if write_fd is not None:
                    os.close(write_fd)
                stop(app)
                stop(server)


if __name__ == "__main__":
    main()
