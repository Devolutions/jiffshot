#!/usr/bin/env python3
"""Verify PDF export through the Linux save dialog."""

import argparse
import asyncio
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile

from dbus_next import Message, MessageType, Variant
from linux_load_e2e import solid_png, window_info
from linux_shortcuts_e2e import DESKTOP, MockPortal, X11Driver, stop, wait_for


class SavePortal(MockPortal):
    def __init__(self, destination):
        super().__init__()
        self.destination = destination
        self.save_requests = []

    def message(self, message):
        if (
            message.message_type == MessageType.METHOD_CALL
            and message.interface == "org.freedesktop.portal.FileChooser"
            and message.member == "SaveFile"
        ):
            _parent, _title, options = message.body
            sender = message.sender.lstrip(":").replace(".", "_")
            request = f"{DESKTOP}/request/{sender}/{options['handle_token'].value}"
            self.save_requests.append(options)
            asyncio.get_running_loop().call_later(
                0.1, self.response, request, 0,
                {"uris": Variant("as", [self.destination.as_uri()])},
            )
            return Message.new_method_return(message, "o", [request])
        return super().message(message)


async def main(binary, runtime):
    app = server = driver = portal = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-export-ui-") as directory:
        work = Path(directory)
        runtime_dir = work / "runtime"
        runtime_dir.mkdir(mode=0o700)
        image = work / "input.png"
        image.write_bytes(solid_png(220, 20, 20))
        destination = work / "export.pdf"
        portal = SavePortal(destination)
        await portal.start()
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
                    "XDG_RUNTIME_DIR": str(runtime_dir),
                    "XDG_SESSION_TYPE": "x11",
                    "RUSTOLONIA_HOST_LIB": str(runtime / "librustolonia_host.so"),
                    "LD_LIBRARY_PATH": os.pathsep.join(
                        filter(None, (str(runtime), os.environ.get("LD_LIBRARY_PATH")))
                    ),
                }
                env.pop("WAYLAND_DISPLAY", None)
                app = subprocess.Popen(
                    [str(binary), str(image)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                    stdout=app_log, stderr=app_log,
                )
                await wait_for(lambda: window_info(env), "the editor", 12)
                window, origin = window_info(env)
                driver = X11Driver(display, window, origin)
                await wait_for(lambda: driver.red_canvas_pixels() > 2000, "the image to export")
                driver.key("Control_L", True)
                driver.key("e", True)
                driver.key("e", False)
                driver.key("Control_L", False)
                driver.x11.XSync(driver.display, 0)
                await wait_for(lambda: portal.save_requests, "the export save dialog", 15)
                filters = portal.save_requests[-1].get("filters")
                assert filters is not None, "the save dialog must expose image and PDF filters"
                patterns = [
                    pattern for _label, entries in filters.value for _kind, pattern in entries
                ]
                assert "*.pdf" in patterns, patterns
                await wait_for(
                    lambda: destination.exists() and destination.read_bytes().endswith(b"%%EOF\n"),
                    "the exported PDF",
                    20,
                )
                output = destination.read_bytes()
                assert output.startswith(b"%PDF-1.4"), "the selected extension must produce a PDF"
                assert b"/Width 240 /Height 140" in output, "PDF must contain the selected image"
                print("GUI export offered PDF and saved the selected image as a one-page PDF")
        except BaseException:
            for name in ("app.log", "xvfb.log"):
                path = work / name
                if path.exists():
                    print(path.read_text(errors="replace")[-12000:], file=sys.stderr)
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
            if portal is not None:
                portal.bus.disconnect()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--runtime-dir", type=Path, required=True)
    arguments = parser.parse_args()
    asyncio.run(main(arguments.binary.resolve(strict=True), arguments.runtime_dir.resolve(strict=True)))
