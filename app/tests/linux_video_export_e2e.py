#!/usr/bin/env python3
"""Exercise GIF video export through the Linux editor and file chooser."""

import argparse
import asyncio
import json
import os
from pathlib import Path
import select
import struct
import subprocess
import sys
import tempfile

from dbus_next import Message, MessageType, Variant

from linux_interactive_effects_e2e import EffectsPortal, count_canvas_color, status_pixels
from linux_load_e2e import solid_png, window_info
from linux_shortcuts_e2e import DESKTOP, X11Driver, stop, wait_for
from linux_webcam_e2e import trace_window


class VideoPortal(EffectsPortal):
    def message(self, message):
        if (
            message.message_type == MessageType.METHOD_CALL
            and message.interface == "org.freedesktop.portal.FileChooser"
            and message.member == "SaveFile"
        ):
            parent, title, options = message.body
            sender = message.sender.lstrip(":").replace(".", "_")
            request = f"{DESKTOP}/request/{sender}/{options['handle_token'].value}"
            self.save_parents.append(parent)
            self.save_requests.append((title, options, request))
            return Message.new_method_return(message, "o", [request])
        return super().message(message)

    def finish_save(self, index, destination):
        request = self.save_requests[index][2]
        asyncio.get_running_loop().call_later(
            0.1, self.response, request, 0,
            {"uris": Variant("as", [destination.as_uri()])},
        )


def write_video(path):
    subprocess.run(
        [
            "gst-launch-1.0", "-q",
            "videotestsrc", "num-buffers=60", "!",
            "video/x-raw,width=32,height=24,framerate=60/1", "!",
            "jpegenc", "!", "avimux", "!", "filesink", f"location={path}",
        ],
        check=True,
        timeout=10,
    )


def gif_delays(path):
    data = path.read_bytes()
    assert data[:6] in (b"GIF87a", b"GIF89a"), "output must be a GIF"
    width, height, packed = struct.unpack_from("<HHB", data, 6)
    position = 13
    if packed & 0x80:
        position += 3 * (1 << ((packed & 7) + 1))
    delays = []
    delay = None

    def skip_blocks(index):
        while True:
            length = data[index]
            index += 1
            if length == 0:
                return index
            index += length

    while position < len(data):
        kind = data[position]
        position += 1
        if kind == 0x3B:
            return (width, height), delays
        if kind == 0x21:
            label = data[position]
            position += 1
            if label == 0xF9:
                assert data[position] == 4, "invalid GIF graphics control extension"
                delay = int.from_bytes(data[position + 2:position + 4], "little") * 10
            position = skip_blocks(position)
        elif kind == 0x2C:
            _x, _y, _width, _height, packed = struct.unpack_from("<HHHHB", data, position)
            position += 9
            if packed & 0x80:
                position += 3 * (1 << ((packed & 7) + 1))
            position = skip_blocks(position + 1)
            assert delay is not None, "every GIF frame must specify a delay"
            delays.append(delay)
            delay = None
        else:
            raise AssertionError(f"unexpected GIF block 0x{kind:02x}")
    raise AssertionError("GIF is missing its trailer")


async def choose_export(driver, portal, source):
    open_index = len(portal.open_requests)
    save_index = len(portal.save_requests)
    driver.click(670, 207)
    await wait_for(lambda: len(portal.open_requests) > open_index, "the video chooser")
    title, _options, _request = portal.open_requests[open_index]
    assert title == "Select video to export as animated GIF", title
    assert int(portal.open_parents[open_index].removeprefix("x11:"), 16) == driver.window
    portal.finish_open(open_index, 0, source.as_uri())
    await wait_for(lambda: len(portal.save_requests) > save_index, "the GIF save chooser")
    assert int(portal.save_parents[save_index].removeprefix("x11:"), 16) == driver.window
    assert portal.save_requests[save_index][0] == "Save animated GIF"
    return save_index


async def main(binary, runtime):
    portal = VideoPortal()
    await portal.start()
    app = server = driver = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-video-export-ui-") as directory:
        work = Path(directory)
        runtime_dir = work / "runtime"
        runtime_dir.mkdir(mode=0o700)
        source = work / "source.avi"
        write_video(source)
        original = source.read_bytes()
        alias = work / "source-alias.gif"
        os.link(source, alias)
        image = work / "image.png"
        image.write_bytes(solid_png(30, 120, 190))
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
                await wait_for(
                    lambda: count_canvas_color(driver, (30, 120, 190)) > 5000,
                    "the source image",
                )
                await asyncio.sleep(0.5)
                trace_window(driver, "video-export-controls")

                destination = work / "animation.gif"
                save_index = await choose_export(driver, portal, source)
                portal.finish_save(save_index, destination)
                await wait_for(destination.exists, "the completed GIF", 15)
                dimensions, delays = gif_delays(destination)
                assert dimensions == (32, 24), dimensions
                assert len(delays) == 60, f"expected 60 GIF frames, got {len(delays)}"
                assert set(delays) == {10, 20}, delays
                assert sum(delays) == 1_000, f"expected one second of GIF timing, got {sum(delays)}"

                save_index = await choose_export(driver, portal, source)
                before_status = status_pixels(driver)
                portal.finish_save(save_index, alias)
                await wait_for(
                    lambda: status_pixels(driver) != before_status,
                    "the source-overwrite error",
                )
                assert source.read_bytes() == original, "GIF export must not overwrite the source"
                assert alias.read_bytes() == original, "GIF export must not overwrite a source alias"
                print("GUI GIF export preserved 60 fps timing and rejected a source alias")
        except BaseException:
            if driver is not None:
                trace_window(driver, "video-export-failure")
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
            portal.bus.disconnect()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--runtime-dir", type=Path, required=True)
    arguments = parser.parse_args()
    asyncio.run(main(arguments.binary.resolve(strict=True), arguments.runtime_dir.resolve(strict=True)))
