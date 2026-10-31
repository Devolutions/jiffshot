#!/usr/bin/env python3
"""Verify that the native file chooser works when the portal is unavailable."""

import argparse
import asyncio
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile

from dbus_next import Message, MessageType
from linux_load_e2e import solid_png, window_info
from linux_shortcuts_e2e import MockPortal, X11Driver, stop, wait_for


FILE_CHOOSER = "org.freedesktop.portal.FileChooser"
TITLE = "Open image, video, or document"


class NoFileChooserPortal(MockPortal):
    def __init__(self):
        super().__init__()
        self.missing_queries = 0
        self.open_requests = 0

    def message(self, message):
        if (
            message.message_type == MessageType.METHOD_CALL
            and message.interface == FILE_CHOOSER
            and message.member == "OpenFile"
        ):
            self.open_requests += 1
            return Message.new_error(
                message, "org.freedesktop.DBus.Error.UnknownMethod",
                f"Unknown interface {FILE_CHOOSER}",
            )
        if (
            message.message_type == MessageType.METHOD_CALL
            and message.interface == "org.freedesktop.DBus.Properties"
            and message.member == "Get"
            and message.body[0] == FILE_CHOOSER
        ):
            self.missing_queries += 1
            return Message.new_error(
                message, "org.freedesktop.DBus.Error.InvalidArgs",
                f"Unknown interface {FILE_CHOOSER}",
            )
        return super().message(message)


def dialog_window(env):
    tree = subprocess.run(
        ["xwininfo", "-root", "-tree"], env=env, capture_output=True, text=True, check=True
    ).stdout
    for line in tree.splitlines():
        if f'"{TITLE}"' not in line:
            continue
        window = int(line.split()[0], 16)
        info = subprocess.run(
            ["xwininfo", "-id", hex(window)], env=env, capture_output=True, text=True
        )
        if info.returncode == 0 and "Map State: IsViewable" in info.stdout:
            return window
    return None


def hotkey(driver, key):
    driver.key("Control_L", True)
    driver.key(key, True)
    driver.key(key, False)
    driver.key("Control_L", False)
    driver.x11.XSync(driver.display, 0)


def press(driver, key):
    driver.key(key, True)
    driver.key(key, False)
    driver.x11.XSync(driver.display, 0)


def type_path(driver, path):
    special = {"/": "slash", ".": "period", "-": "minus", "_": "minus"}
    for character in str(path):
        shifted = character.isupper() or character == "_"
        if shifted:
            driver.key("Shift_L", True)
        press(driver, special.get(character, character.lower()))
        if shifted:
            driver.key("Shift_L", False)
    driver.x11.XSync(driver.display, 0)


async def main(binary, runtime):
    portal = NoFileChooserPortal()
    await portal.start()
    app = server = driver = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-native-picker-") as directory:
        work = Path(directory)
        original = work / "original.png"
        original.write_bytes(solid_png(30, 120, 190))
        chosen = work / "chosen.png"
        chosen.write_bytes(solid_png(255, 0, 0))
        config = work / "config" / "Jiffshot"
        config.mkdir(parents=True)
        (config / "settings.json").write_text(json.dumps({"theme": "Light"}))
        xdg_runtime = work / "runtime"
        xdg_runtime.mkdir(mode=0o700)
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
                display = ":" + os.read(read_fd, 128).decode("ascii").strip()
                env = {
                    **os.environ,
                    "DISPLAY": display,
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
                env.pop("GTK_USE_PORTAL", None)
                app = subprocess.Popen(
                    [str(binary), str(original)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                    stdout=app_log, stderr=app_log,
                )
                await wait_for(lambda: window_info(env), "the editor")
                window, origin = window_info(env)
                driver = X11Driver(display, window, origin)
                await asyncio.sleep(0.5)

                async def open_dialog():
                    driver.x11.XSetInputFocus(driver.display, window, 2, 0)
                    hotkey(driver, "o")
                    await wait_for(lambda: dialog_window(env), "the native file chooser")
                    chooser = dialog_window(env)
                    driver.x11.XSetInputFocus(driver.display, chooser, 2, 0)
                    driver.x11.XSync(driver.display, 0)
                    await asyncio.sleep(0.5)
                    return chooser

                chooser = await open_dialog()
                press(driver, "Escape")
                await wait_for(lambda: dialog_window(env) != chooser, "native chooser cancellation")
                chooser = await open_dialog()
                hotkey(driver, "l")
                await asyncio.sleep(0.5)
                driver.select_all()
                type_path(driver, chosen)
                await asyncio.sleep(0.5)
                press(driver, "Return")
                await wait_for(lambda: dialog_window(env) != chooser, "native file selection")
                await wait_for(lambda: driver.red_canvas_pixels() > 5000, "the selected image")
                chooser = await open_dialog()
                press(driver, "Escape")
                await wait_for(lambda: dialog_window(env) != chooser, "a subsequent cancellation")
                assert portal.missing_queries >= 1, portal.missing_queries
                assert portal.open_requests >= 3, portal.open_requests
                assert "File chooser portal unavailable; using the native picker" in (
                    work / "app.log"
                ).read_text(errors="replace")
                print("GUI native chooser cancellation, file selection, and retry passed")
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
