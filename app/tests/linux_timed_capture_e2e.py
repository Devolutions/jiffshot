#!/usr/bin/env python3
"""Verify delayed capture preferences and cancellation through a mock portal."""

import argparse
import asyncio
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile

from dbus_next import Message, MessageType, Variant
from dbus_next.aio import MessageBus
from linux_load_e2e import window_info
from linux_shortcuts_e2e import SCREENCAST, MockPortal, X11Driver, stop, wait_for
from linux_webcam_e2e import TextReader, pixels, replace, trace_window


class TimedPortal(MockPortal):
    def __init__(self):
        super().__init__()
        self.remote_opens = 0
        self.remote_fds = []
        self.screen_mode = "fake-remote"
        self.hold_start = False

    async def start(self):
        self.bus = await MessageBus(negotiate_unix_fd=True).connect()
        await self.bus.request_name("org.freedesktop.portal.Desktop")
        self.bus.add_message_handler(self.message)

    def message(self, message):
        if message.message_type == MessageType.METHOD_CALL and message.interface == SCREENCAST:
            if message.member == "Start":
                reply = super().message(message)
                _session, request = self.screen_starts[-1]
                if not self.hold_start:
                    asyncio.get_running_loop().call_later(0.1, self.release_start, request)
                return reply
            if message.member == "OpenPipeWireRemote":
                self.remote_opens += 1
                read_fd, write_fd = os.pipe()
                self.remote_fds.extend((read_fd, write_fd))
                return Message.new_method_return(message, "h", [0], unix_fds=[read_fd])
        return super().message(message)

    def release_start(self, request):
        result = {"streams": Variant("a(ua{sv})", [[42, {}]])}
        self.response(request, 0, result)

    def close(self):
        self.bus.disconnect()
        for fd in self.remote_fds:
            os.close(fd)


async def main(binary, runtime):
    portal = TimedPortal()
    await portal.start()
    app = server = driver = text = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-timed-ui-") as directory:
        work = Path(directory)
        xdg_runtime = work / "runtime"
        xdg_runtime.mkdir(mode=0o700)
        config = work / "config" / "Jiffshot"
        config.mkdir(parents=True)
        captures = work / "captures"
        captures.mkdir()
        settings = config / "settings.json"
        original = {
            "theme": "Light",
            "capture_directory": str(captures),
            "copy_to_clipboard": False,
            "capture_delay_seconds": 3,
            "capture_cursor": False,
            "recording_cursor": True,
        }
        settings.write_text(json.dumps(original))
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
                app = subprocess.Popen(
                    [str(binary)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                    stdout=app_log, stderr=app_log,
                )
                await wait_for(lambda: window_info(env), "the editor", 12)
                window, origin = window_info(env)
                driver = X11Driver(display_name, window, origin)
                text = TextReader()
                await asyncio.sleep(0.5)

                async def click_label(label):
                    driver.click(*await text.find_scrolling_label(driver, label))

                async def replace_delay(value):
                    x, y = await text.find_scrolling_label(driver, "Save capture delay")
                    # The 60-pixel delay textbox is immediately before the save button.
                    replace(driver, x - 110, y, value)

                previous = settings.stat().st_ino
                await click_label("Save cursor preferences")
                await wait_for(lambda: settings.stat().st_ino != previous, "saving loaded cursors")
                assert json.loads(settings.read_text()) == original, (
                    "the cursor controls must load their separate saved values"
                )
                await click_label("Include cursor in captures")
                await click_label("Include cursor in recordings")
                previous = settings.stat().st_ino
                await click_label("Save cursor preferences")
                await wait_for(lambda: settings.stat().st_ino != previous, "saving cursor changes")
                assert json.loads(settings.read_text()) == {
                    **original, "capture_cursor": True, "recording_cursor": False,
                }, "capture and recording cursors must change independently"
                await click_label("Include cursor in captures")
                await click_label("Include cursor in recordings")
                previous = settings.stat().st_ino
                await click_label("Save cursor preferences")
                await wait_for(lambda: settings.stat().st_ino != previous, "restoring cursors")
                assert json.loads(settings.read_text()) == original

                previous = settings.stat().st_ino
                await click_label("Save capture delay")
                await wait_for(lambda: settings.stat().st_ino != previous, "saving the loaded delay")
                assert json.loads(settings.read_text()) == original, (
                    "saving must retain the loaded delay and other preferences"
                )
                await replace_delay("5")
                previous = settings.stat().st_ino
                await click_label("Save capture delay")
                await wait_for(lambda: settings.stat().st_ino != previous, "saving a new delay")
                saved = {**original, "capture_delay_seconds": 5}
                assert json.loads(settings.read_text()) == saved

                before_status = pixels(driver, 0, 480, 800, 20)[1]
                await replace_delay("61")
                await click_label("Save capture delay")
                await wait_for(
                    lambda: pixels(driver, 0, 480, 800, 20)[1] != before_status,
                    "the invalid-delay error",
                )
                assert json.loads(settings.read_text()) == saved, (
                    "an invalid delay must not overwrite saved preferences"
                )

                settings.write_text(json.dumps({**saved, "capture_delay_seconds": -1}))
                await click_label("Capture screen or window")
                await asyncio.sleep(0.5)
                assert portal.capture_requests == 0, (
                    "an invalid delay must prevent the screen capture portal request"
                )

                settings.write_text(json.dumps(saved))
                portal.hold_start = True
                await click_label("Capture screen or window")
                await wait_for(lambda: len(portal.screen_starts) == 1, "the held source chooser")
                session, request = portal.screen_starts[-1]
                await click_label("Cancel delayed capture")
                portal.release_start(request)
                await wait_for(lambda: portal.remote_opens == 1, "the selected source")
                assert portal.screen_selection_options[-1]["cursor_mode"].value == 1, (
                    "the saved capture setting must hide the portal cursor"
                )
                await wait_for(
                    lambda: session in portal.closes,
                    "closing a capture cancelled during source selection",
                    3,
                )
                assert not list(captures.iterdir()), "cancellation must not save a capture"
                portal.hold_start = False
                for expected in (2, 3):
                    await click_label("Capture screen or window")
                    await wait_for(
                        lambda: portal.remote_opens == expected,
                        "the selected screen's PipeWire remote",
                    )
                    session = portal.screen_sessions[-1]
                    await click_label("Capture screen or window")
                    await asyncio.sleep(0.3)
                    assert portal.capture_requests == expected, (
                        "another delayed capture must not start during the countdown"
                    )
                    await click_label("Cancel delayed capture")
                    await wait_for(
                        lambda: session in portal.closes,
                        "closing the cancelled delayed capture",
                        3,
                    )
                    assert not list(captures.iterdir()), "cancellation must not save a capture"
                print("GUI persisted and validated the capture delay and cancelled active countdowns")
        except BaseException:
            if driver is not None:
                trace_window(driver, "timer-failure")
            print((work / "xvfb.log").read_text(errors="replace")[-12000:], file=sys.stderr)
            print((work / "app.log").read_text(errors="replace")[-12000:], file=sys.stderr)
            raise
        finally:
            if driver is not None:
                driver.close()
            if text is not None:
                text.close()
            stop(app)
            stop(server)
            if read_fd is not None:
                os.close(read_fd)
            if write_fd is not None:
                os.close(write_fd)
            portal.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--runtime-dir", type=Path, required=True)
    arguments = parser.parse_args()
    asyncio.run(main(arguments.binary.resolve(strict=True), arguments.runtime_dir.resolve(strict=True)))
