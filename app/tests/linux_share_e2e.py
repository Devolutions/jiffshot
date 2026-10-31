#!/usr/bin/env python3
"""Verify sharing selected saved captures with private desktop portal mocks."""

import argparse
import asyncio
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import time

from dbus_next import Message, MessageType, Variant
from dbus_next.aio import MessageBus
from linux_load_e2e import draw_arrow, solid_png, window_info
from linux_shortcuts_e2e import DESKTOP, MockPortal, X11Driver, stop, wait_for
from linux_webcam_e2e import TextReader, pixels, scroll, trace_window


def enter_search_query(driver, query):
    driver.click(80, 214)
    driver.select_all()
    driver.key("Delete", True)
    driver.key("Delete", False)
    for character in query:
        shifted = character == ":"
        name = {":": "semicolon", ".": "period", " ": "space"}.get(character, character)
        if shifted:
            driver.key("Shift_L", True)
        driver.key(name, True)
        driver.key(name, False)
        if shifted:
            driver.key("Shift_L", False)
    driver.x11.XSync(driver.display, 0)


class SharePortal(MockPortal):
    def __init__(self, destination):
        super().__init__()
        self.destination = destination
        self.open_requests = []
        self.email_requests = []
        self.print_requests = []
        self.folder_requests = []
        self.folder_parents = []
        self.response_code = 0

    async def start(self):
        self.bus = await MessageBus(negotiate_unix_fd=True).connect()
        await self.bus.request_name("org.freedesktop.portal.Desktop")
        self.bus.add_message_handler(self.message)

    @staticmethod
    def file_bytes(message, index):
        descriptor = message.unix_fds[index]
        try:
            with os.fdopen(descriptor, "rb") as stream:
                return stream.read()
        finally:
            message.unix_fds[index] = -1

    def reply(self, message, options, result=None):
        sender = message.sender.lstrip(":").replace(".", "_")
        request = f"{DESKTOP}/request/{sender}/{options['handle_token'].value}"
        result = (result or {}) if self.response_code == 0 else {}
        asyncio.get_running_loop().call_later(
            0.1, self.response, request, self.response_code, result
        )
        return Message.new_method_return(message, "o", [request])

    def message(self, message):
        if message.message_type != MessageType.METHOD_CALL:
            return None
        if (
            message.interface == "org.freedesktop.DBus.Properties"
            and message.member == "Get"
            and message.body[0] == "org.freedesktop.portal.FileChooser"
        ):
            return Message.new_method_return(message, "v", [Variant("u", 4)])
        if message.interface == "org.freedesktop.portal.OpenURI" and message.member == "OpenFile":
            _parent, index, options = message.body
            self.open_requests.append((options, self.file_bytes(message, index)))
            return self.reply(message, options)
        if message.interface == "org.freedesktop.portal.Email" and message.member == "ComposeEmail":
            _parent, options = message.body
            attachments = [
                self.file_bytes(message, index)
                for index in options["attachment_fds"].value
            ]
            self.email_requests.append((options, attachments))
            return self.reply(message, options)
        if message.interface == "org.freedesktop.portal.Print" and message.member == "Print":
            _parent, title, index, options = message.body
            self.print_requests.append((title, options, self.file_bytes(message, index)))
            return self.reply(message, options)
        if (
            message.interface == "org.freedesktop.portal.FileChooser"
            and message.member == "OpenFile"
        ):
            parent, _title, options = message.body
            self.folder_requests.append(options)
            self.folder_parents.append(parent)
            return self.reply(
                message, options, {"uris": Variant("as", [self.destination.as_uri()])}
            )
        return super().message(message)


async def main(binary, runtime):
    app = server = driver = portal = reader = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-share-ui-") as directory:
        work = Path(directory)
        captures = work / "captures"
        captures.mkdir()
        source = captures / "saved-capture.png"
        image = solid_png(30, 120, 190)
        source.write_bytes(image)
        source.with_suffix(".json").write_text(json.dumps({
            "process": "Browser",
            "url": "https://docs.example.test/guide",
            "tags": ["Planning"],
        }))
        old_time = time.time() - 3600
        os.utime(source, (old_time, old_time))
        decoy = captures / "newer-decoy.png"
        decoy.write_bytes(solid_png(240, 40, 40))
        decoy.with_suffix(".json").write_text(json.dumps({
            "window_title": "Browser Planning docs.example.test",
            "process": "Editor",
            "url": "https://other.test/docs.example.test",
            "tags": ["Reference"],
        }))
        destination = work / "destination"
        destination.mkdir()
        existing = destination / source.name
        existing.write_bytes(b"existing file")
        config = work / "config" / "Jiffshot"
        config.mkdir(parents=True)
        (config / "settings.json").write_text(json.dumps({
            "capture_directory": str(captures), "theme": "Light",
        }))
        runtime_dir = work / "runtime"
        runtime_dir.mkdir(mode=0o700)
        portal = SharePortal(destination)
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
                    "GTK_USE_PORTAL": "1",
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
                driver = X11Driver(display, window, origin)
                await asyncio.sleep(0.5)
                scroll(driver, "down")
                await asyncio.sleep(0.5)
                trace_window(driver, "share-before")

                previous_status = pixels(driver, 0, 480, 800, 20)[1]
                enter_search_query(
                    driver, "app:browser site:example.test tag:planning age:day"
                )
                driver.click(230, 214)
                await wait_for(
                    lambda: pixels(driver, 0, 480, 800, 20)[1] != previous_status,
                    "the qualified library search",
                )
                await asyncio.sleep(0.5)
                driver.click(558, 214)
                await asyncio.sleep(0.5)
                scroll(driver, "down")
                await asyncio.sleep(0.5)
                trace_window(driver, "share-controls")
                reader = TextReader()
                buttons = {}
                for name, label in (
                    ("open", "Open saved capture with"),
                    ("email", "Email saved capture"),
                    ("copy", "Copy saved capture to folder"),
                    ("print", "Print saved image"),
                    ("capture", "saved-capture.png"),
                ):
                    position = reader.find_label(driver, label)
                    assert position is not None, f"could not find {label!r} in the library"
                    buttons[name] = position

                previous_status = pixels(driver, 0, 480, 800, 20)[1]
                driver.click(*buttons["open"])
                await wait_for(
                    lambda: pixels(driver, 0, 480, 800, 20)[1] != previous_status,
                    "sharing without a selected capture",
                )
                await reader.wait_for_status(driver, "Select a saved capture before sharing")
                assert not portal.open_requests, "sharing must require a saved selection"
                driver.click(*buttons["copy"])
                driver.click(*buttons["print"])
                await asyncio.sleep(0.3)
                assert not portal.folder_requests and not portal.print_requests, (
                    "copying and printing must require a saved selection"
                )

                driver.click(*buttons["capture"])
                await asyncio.sleep(1)
                trace_window(driver, "share-selected")
                draw_arrow(driver)
                await wait_for(lambda: driver.red_canvas_pixels() > 40, "an unsaved annotation")

                driver.click(*buttons["open"])
                await wait_for(lambda: len(portal.open_requests) == 1, "the open-with portal")
                options, contents = portal.open_requests[-1]
                assert options["ask"].value, "open-with must ask the desktop for an application"
                assert contents == image, "open-with must receive the saved file, not editor changes"

                driver.click(*buttons["email"])
                await wait_for(lambda: len(portal.email_requests) == 1, "the email portal")
                options, attachments = portal.email_requests[-1]
                assert options["subject"].value == "Jiffshot capture", options
                assert attachments == [image], "email must attach the selected saved file"

                driver.click(*buttons["print"])
                await wait_for(lambda: len(portal.print_requests) == 1, "the print portal")
                title, _options, output = portal.print_requests[-1]
                assert title == "Jiffshot saved capture", title
                assert output.startswith(b"%PDF-1.4") and output.endswith(b"%%EOF\n"), (
                    "the print portal must receive a complete PDF"
                )
                assert b"/Width 240 /Height 140" in output, (
                    "the printed page must contain the selected saved image"
                )

                driver.click(*buttons["copy"])
                await wait_for(lambda: len(portal.folder_requests) == 1, "the folder picker")
                assert portal.folder_requests[-1]["directory"].value, "the picker must choose a folder"
                assert int(portal.folder_parents[-1].removeprefix("x11:"), 16) == driver.window
                copied = destination / "saved-capture (2).png"
                await wait_for(lambda: copied.exists(), "the copied capture")
                assert copied.read_bytes() == image
                assert existing.read_bytes() == b"existing file", "copy must not overwrite an existing file"

                portal.response_code = 1
                driver.click(*buttons["open"])
                await wait_for(lambda: len(portal.open_requests) == 2, "a cancelled open-with request")
                await asyncio.sleep(0.3)
                driver.click(*buttons["copy"])
                await wait_for(lambda: len(portal.folder_requests) == 2, "a cancelled folder picker")
                await asyncio.sleep(0.3)
                files_after_cancel = sorted(path.name for path in destination.iterdir())
                assert files_after_cancel == [
                    "saved-capture (2).png", "saved-capture.png",
                ], f"cancelled folder selection created unexpected files: {files_after_cancel}"
                driver.click(*buttons["print"])
                await wait_for(lambda: len(portal.print_requests) == 2, "a cancelled print request")
                await asyncio.sleep(0.3)
                portal.response_code = 2
                driver.click(*buttons["email"])
                await wait_for(lambda: len(portal.email_requests) == 2, "a rejected email request")
                await asyncio.sleep(0.3)
                previous_status = pixels(driver, 0, 480, 800, 20)[1]
                driver.click(*buttons["copy"])
                await wait_for(lambda: len(portal.folder_requests) == 3, "a failed folder picker")
                await wait_for(
                    lambda: pixels(driver, 0, 480, 800, 20)[1] != previous_status,
                    "reporting the failed folder picker",
                )
                assert sorted(path.name for path in destination.iterdir()) == files_after_cancel
                portal.response_code = 0
                driver.click(*buttons["copy"])
                await wait_for(lambda: len(portal.folder_requests) == 4, "a folder picker after errors")
                copied_after_errors = destination / "saved-capture (3).png"
                await wait_for(lambda: copied_after_errors.exists(), "a copy after picker errors")
                assert copied_after_errors.read_bytes() == image

                source.write_bytes(b"not an image")
                previous_status = pixels(driver, 0, 480, 800, 20)[1]
                driver.click(*buttons["print"])
                await wait_for(
                    lambda: pixels(driver, 0, 480, 800, 20)[1] != previous_status,
                    "reporting a non-image print source",
                )
                assert len(portal.print_requests) == 2, "invalid image data must not reach print"

                source.unlink()
                await asyncio.sleep(0.3)
                previous_status = pixels(driver, 0, 480, 800, 20)[1]
                driver.click(*buttons["open"])
                await wait_for(
                    lambda: pixels(driver, 0, 480, 800, 20)[1] != previous_status,
                    "reporting a missing saved capture",
                )
                assert len(portal.open_requests) == 2, "a missing file must not reach the portal"
                print("GUI qualified library search selected the matching saved file for sharing and printing")
        except BaseException:
            if driver is not None:
                trace_window(driver, "share-failure")
            for name in ("app.log", "xvfb.log"):
                path = work / name
                if path.exists():
                    print(path.read_text(errors="replace")[-12000:], file=sys.stderr)
            raise
        finally:
            if reader is not None:
                reader.close()
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
