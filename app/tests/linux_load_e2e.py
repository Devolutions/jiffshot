#!/usr/bin/env python3
"""Verify that opening a slow file does not block or overwrite the editor."""

import argparse
import asyncio
import errno
import json
import os
from pathlib import Path
import re
import select
import struct
import subprocess
import sys
import tempfile
import zlib

from linux_shortcuts_e2e import MockPortal, X11Driver, stop, wait_for


def solid_png(red, green, blue):
    def chunk(kind, data):
        return (
            struct.pack(">I", len(data)) + kind + data
            + struct.pack(">I", zlib.crc32(kind + data))
        )

    width, height = 240, 140
    pixels = (b"\0" + bytes((red, green, blue)) * width) * height
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(pixels))
        + chunk(b"IEND", b"")
    )


def window_info(env):
    tree = subprocess.run(
        ["xwininfo", "-root", "-tree"], env=env, capture_output=True, text=True, check=True
    ).stdout
    for line in tree.splitlines():
        if '"Jiffshot"' not in line:
            continue
        window = int(line.split()[0], 16)
        info = subprocess.run(
            ["xwininfo", "-id", hex(window)], env=env, capture_output=True, text=True, check=True
        ).stdout
        x = re.search(r"Absolute upper-left X:\s*(-?\d+)", info)
        y = re.search(r"Absolute upper-left Y:\s*(-?\d+)", info)
        if "Map State: IsViewable" in info and x and y:
            return window, (int(x.group(1)), int(y.group(1)))
    return None


def send_fifo(path, payload):
    try:
        fd = os.open(path, os.O_WRONLY | os.O_NONBLOCK)
    except OSError as error:
        if error.errno == errno.ENXIO:
            return False
        raise
    with os.fdopen(fd, "wb") as stream:
        stream.write(payload)
    return True


def draw_arrow(driver):
    def move(x, y):
        driver.xtst.XTestFakeMotionEvent(
            driver.display, driver.screen, x + driver.origin[0], y + driver.origin[1], 0
        )

    move(250, 350)
    driver.xtst.XTestFakeButtonEvent(driver.display, 1, 1, 0)
    move(520, 430)
    driver.xtst.XTestFakeButtonEvent(driver.display, 1, 0, 0)
    driver.x11.XSync(driver.display, 0)


async def main(binary, runtime):
    portal = MockPortal()
    await portal.start()
    app = server = driver = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-load-ui-") as directory:
        work = Path(directory)
        xdg_runtime = work / "runtime"
        xdg_runtime.mkdir(mode=0o700)
        config = work / "config" / "Jiffshot"
        config.mkdir(parents=True)
        (config / "settings.json").write_text(json.dumps({"theme": "Light"}))
        read_fd, write_fd = os.pipe()
        try:
            with (work / "xvfb.log").open("wb") as xvfb_log, (work / "app.log").open("wb") as app_log:
                server = subprocess.Popen(
                    ["Xvfb", "-displayfd", str(write_fd), "-screen", "0", "1280x900x24",
                     "-nolisten", "tcp"],
                    pass_fds=(write_fd,), stdout=xvfb_log, stderr=xvfb_log,
                )
                os.close(write_fd)
                write_fd = None
                if not select.select([read_fd], [], [], 10)[0]:
                    raise RuntimeError("private Xvfb did not start")
                display_name = ":" + os.read(read_fd, 128).decode("ascii").strip()
                env = {
                    **os.environ,
                    "DISPLAY": display_name,
                    "HOME": str(work),
                    "XDG_CONFIG_HOME": str(work / "config"),
                    "XDG_DATA_HOME": str(work / "data"),
                    "XDG_CACHE_HOME": str(work / "cache"),
                    "XDG_RUNTIME_DIR": str(xdg_runtime),
                    "XDG_SESSION_TYPE": "x11",
                    "RUSTOLONIA_HOST_LIB": str(runtime / "librustolonia_host.so"),
                    "LD_LIBRARY_PATH": os.pathsep.join(
                        filter(None, (str(runtime), os.environ.get("LD_LIBRARY_PATH")))
                    ),
                }
                env.pop("WAYLAND_DISPLAY", None)
                for edited in (False, True):
                    source = work / ("edited.png" if edited else "unchanged.png")
                    os.mkfifo(source)
                    app = subprocess.Popen(
                        [str(binary), str(source)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                        stdout=app_log, stderr=app_log,
                    )
                    await wait_for(lambda: window_info(env), "the editor while the file is blocked", 8)
                    window, origin = window_info(env)
                    driver = X11Driver(display_name, window, origin)
                    await asyncio.sleep(0.5)
                    if edited:
                        draw_arrow(driver)
                        await wait_for(
                            lambda: driver.red_canvas_pixels() > 40,
                            "an annotation while the file is blocked",
                        )
                        before = driver.red_canvas_pixels()
                        payload = solid_png(255, 255, 255)
                    else:
                        payload = solid_png(255, 0, 0)
                    await wait_for(lambda: send_fifo(source, payload), "the blocked file reader")
                    if edited:
                        await asyncio.sleep(2)
                        after = driver.red_canvas_pixels()
                        assert after >= before * 0.8, (
                            f"the completed open replaced a newer annotation: before={before}, after={after}"
                        )
                    else:
                        await wait_for(
                            lambda: driver.red_canvas_pixels() > 5000,
                            "the loaded image to reach the unchanged editor",
                        )
                    driver.close()
                    driver = None
                    stop(app)
                    app = None
                print("GUI stayed responsive while opening; late results preserved newer edits")
        except BaseException:
            print((work / "xvfb.log").read_text(errors="replace")[-12000:], file=sys.stderr)
            print((work / "app.log").read_text(errors="replace")[-12000:], file=sys.stderr)
            raise
        finally:
            if driver is not None:
                driver.close()
            stop(app)
            stop(server)
            if read_fd is not None:
                os.close(read_fd)
            if write_fd is not None:
                os.close(write_fd)
            portal.bus.disconnect()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--runtime-dir", type=Path, required=True)
    arguments = parser.parse_args()
    asyncio.run(main(arguments.binary.resolve(strict=True), arguments.runtime_dir.resolve(strict=True)))
