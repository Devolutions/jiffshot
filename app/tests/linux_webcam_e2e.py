#!/usr/bin/env python3
"""Verify webcam preferences in a private Xvfb with a mock portal."""

import argparse
import asyncio
import ctypes
import ctypes.util
import csv
import io
import json
import os
from pathlib import Path
import select
import struct
import subprocess
import sys
import tempfile
import zlib

from linux_load_e2e import window_info
from linux_shortcuts_e2e import MockPortal, X11Driver, stop, wait_for


def pixels(driver, x, y, width, height):
    image = driver.x11.XGetImage(
        driver.display, driver.window, x, y, width, height, ctypes.c_ulong(-1).value, 2
    )
    if not image:
        raise RuntimeError("could not read the app window")
    try:
        frame = image.contents
        if (frame.bits_per_pixel, frame.byte_order, frame.red_mask,
                frame.green_mask, frame.blue_mask) != (32, 0, 0xFF0000, 0xFF00, 0xFF):
            raise RuntimeError("unexpected X11 pixel format")
        data = ctypes.string_at(frame.data, frame.bytes_per_line * frame.height)
        return frame.bytes_per_line, data
    finally:
        driver.x11.XDestroyImage(image)


class TextReader:
    """Read visible UI text from the private X11 window using installed Tesseract."""

    def __init__(self):
        library = ctypes.util.find_library("tesseract")
        if not library:
            raise RuntimeError("Tesseract is required to verify visible UI text")
        self.api = ctypes.CDLL(library)
        self.api.TessBaseAPICreate.restype = ctypes.c_void_p
        self.api.TessBaseAPIInit3.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p]
        self.api.TessBaseAPISetPageSegMode.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.api.TessBaseAPISetImage.argtypes = [
            ctypes.c_void_p, ctypes.POINTER(ctypes.c_ubyte),
            ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int,
        ]
        self.api.TessBaseAPIRecognize.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
        self.api.TessBaseAPIGetUTF8Text.argtypes = [ctypes.c_void_p]
        self.api.TessBaseAPIGetUTF8Text.restype = ctypes.c_void_p
        self.api.TessBaseAPIGetTsvText.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.api.TessBaseAPIGetTsvText.restype = ctypes.c_void_p
        self.api.TessDeleteText.argtypes = [ctypes.c_void_p]
        self.api.TessBaseAPIDelete.argtypes = [ctypes.c_void_p]
        self.instance = self.api.TessBaseAPICreate()
        if not self.instance or self.api.TessBaseAPIInit3(self.instance, None, b"eng"):
            raise RuntimeError("could not initialize UI-text OCR")

    def _recognize(self, driver, x, y, width, height, page_mode, tsv=False):
        stride, data = pixels(driver, x, y, width, height)
        scale = 3
        gray = bytearray()
        for row_index in range(height):
            row = bytearray()
            for column in range(width):
                offset = row_index * stride + column * 4
                value = sum(data[offset:offset + 3]) // 3
                row.extend([value] * scale)
            gray.extend(row * scale)
        image = (ctypes.c_ubyte * len(gray)).from_buffer(gray)
        self.api.TessBaseAPISetPageSegMode(self.instance, page_mode)
        self.api.TessBaseAPISetImage(
            self.instance, image, width * scale, height * scale, 1, width * scale,
        )
        if self.api.TessBaseAPIRecognize(self.instance, None):
            raise RuntimeError("UI-text OCR failed")
        result = (
            self.api.TessBaseAPIGetTsvText(self.instance, 0)
            if tsv else self.api.TessBaseAPIGetUTF8Text(self.instance)
        )
        if not result:
            raise RuntimeError("UI-text OCR returned no text")
        try:
            return ctypes.string_at(result).decode("utf-8", errors="replace")
        finally:
            self.api.TessDeleteText(result)

    def status(self, driver):
        return self._recognize(driver, 0, 480, 800, 20, 7).strip()

    async def wait_for_status(self, driver, expected, timeout=12):
        seen = ""

        def matches():
            nonlocal seen
            seen = self.status(driver)
            return expected.casefold() in seen.casefold()

        try:
            await wait_for(matches, f"visible status containing {expected!r}", timeout)
        except AssertionError as error:
            raise AssertionError(f"{error}; last visible text: {seen!r}") from error

    def find_label(self, driver, label):
        scale = 3
        data = self._recognize(driver, 0, 0, 800, 270, 6, tsv=True)
        columns = (
            "level", "page_num", "block_num", "par_num", "line_num", "word_num",
            "left", "top", "width", "height", "conf", "text",
        )
        rows = csv.DictReader(io.StringIO(data), delimiter="\t", fieldnames=columns)
        lines = {}
        for row in rows:
            text = (row["text"] or "").strip()
            if row["level"] == "level":
                continue
            if not text:
                continue
            key = (row["block_num"], row["par_num"], row["line_num"])
            lines.setdefault(key, []).append(row)

        def normalize(text):
            return "".join(character for character in text.casefold() if character.isalnum())

        target = normalize(label)
        for words in lines.values():
            for start in range(len(words)):
                combined = ""
                for end in range(start, min(len(words), start + len(label.split()) + 2)):
                    combined += normalize(words[end]["text"])
                    if combined == target:
                        first, last = words[start], words[end]
                        left = int(first["left"])
                        right = int(last["left"]) + int(last["width"])
                        top = min(int(word["top"]) for word in words[start:end + 1])
                        bottom = max(
                            int(word["top"]) + int(word["height"])
                            for word in words[start:end + 1]
                        )
                        return ((left + right) // (2 * scale), (top + bottom) // (2 * scale))
                    if len(combined) > len(target):
                        break
        return None

    def close(self):
        if self.instance:
            self.api.TessBaseAPIDelete(self.instance)
            self.instance = None


def trace_window(driver, label):
    directory = os.environ.get("JIFFSHOT_GUI_TRACE")
    if not directory:
        return
    width, height = 800, 500
    stride, data = pixels(driver, 0, 0, width, height)
    rows = bytearray()
    for y in range(height):
        rows.append(0)
        for x in range(width):
            offset = y * stride + x * 4
            rows.extend((data[offset + 2], data[offset + 1], data[offset]))

    def chunk(kind, value):
        return (
            struct.pack(">I", len(value)) + kind + value
            + struct.pack(">I", zlib.crc32(kind + value))
        )

    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(bytes(rows)))
        + chunk(b"IEND", b"")
    )
    (Path(directory) / f"webcam-{label}.png").write_bytes(png)


def press(driver, key):
    driver.key(key, True)
    driver.key(key, False)
    driver.x11.XSync(driver.display, 0)


def replace(driver, x, y, text):
    driver.click(x, y)
    driver.select_all()
    for character in text:
        key = "slash" if character == "/" else character
        press(driver, key)


def scroll(driver, direction):
    driver.xtst.XTestFakeMotionEvent(
        driver.display, driver.screen, 750 + driver.origin[0], 150 + driver.origin[1], 0
    )
    button = 5 if direction == "down" else 4
    for _ in range(25):
        driver.xtst.XTestFakeButtonEvent(driver.display, button, 1, 0)
        driver.xtst.XTestFakeButtonEvent(driver.display, button, 0, 0)
    driver.x11.XSync(driver.display, 0)


async def save_webcam_preferences(driver, settings, description):
    previous = settings.stat().st_ino
    driver.click(100, 183)
    await wait_for(lambda: settings.stat().st_ino != previous, description)
    return json.loads(settings.read_text())


async def main(binary, runtime):
    portal = MockPortal()
    await portal.start()
    app = server = driver = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-webcam-ui-") as directory:
        work = Path(directory)
        xdg_runtime = work / "runtime"
        xdg_runtime.mkdir(mode=0o700)
        config = work / "config" / "Jiffshot"
        config.mkdir(parents=True)
        settings = config / "settings.json"
        original = {
            "theme": "Light",
            "capture_directory": str(work / "captures"),
            "webcam_device": "/dev/video7",
            "webcam_corner": "top-left",
            "webcam_size_percent": 40,
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
                await asyncio.sleep(0.5)
                scroll(driver, "down")
                await asyncio.sleep(0.5)
                trace_window(driver, "initial")

                initial = await save_webcam_preferences(
                    driver, settings, "saving legacy preferences"
                )
                assert initial == {**original, "webcam_shape": "rectangle"}, (
                    "legacy settings must save as a rectangle without losing other preferences",
                    initial,
                )

                replace(driver, 185, 150, "/dev/video9")
                replace(driver, 600, 150, "50")
                choices = [
                    ("rectangle", "top-left"),
                    ("rectangle", "top-right"),
                    ("rectangle", "bottom-left"),
                    ("rectangle", "bottom-right"),
                    ("circle", "top-left"),
                    ("circle", "top-right"),
                    ("circle", "bottom-left"),
                    ("circle", "bottom-right"),
                ]
                for index in (0, 1, 2, 3, 4, 5, 7, 3, 6):
                    shape, corner = choices[index]
                    driver.click(420, 150)
                    press(driver, "Home")
                    for _ in range(index):
                        press(driver, "Down")
                    press(driver, "Return")
                    saved = await save_webcam_preferences(
                        driver, settings, f"saving {shape} at {corner}"
                    )
                    assert saved == {
                        **original,
                        "webcam_device": "/dev/video9",
                        "webcam_corner": corner,
                        "webcam_size_percent": 50,
                        "webcam_shape": shape,
                    }, (index, saved)
                trace_window(driver, "edited")

                driver.close()
                driver = None
                stop(app)
                app = subprocess.Popen(
                    [str(binary)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                    stdout=app_log, stderr=app_log,
                )
                await wait_for(lambda: window_info(env), "the reopened editor", 12)
                window, origin = window_info(env)
                driver = X11Driver(display_name, window, origin)
                await asyncio.sleep(0.5)
                scroll(driver, "down")
                await asyncio.sleep(0.5)
                reloaded = await save_webcam_preferences(
                    driver, settings, "saving the reloaded circle selection"
                )
                assert reloaded == saved, (
                    "reopening the editor must restore the circle, corner, device, and size",
                    reloaded,
                )
                trace_window(driver, "reloaded")
                await asyncio.sleep(0.3)
                before_status = pixels(driver, 0, 480, 800, 20)[1]

                replace(driver, 600, 150, "99")
                driver.click(100, 183)
                await wait_for(
                    lambda: pixels(driver, 0, 480, 800, 20)[1] != before_status,
                    "the invalid-size error",
                )
                assert json.loads(settings.read_text()) == saved, (
                    "an invalid webcam size must not overwrite saved preferences"
                )
                trace_window(driver, "invalid")

                settings.write_text(json.dumps({**saved, "webcam_size_percent": 99}))
                scroll(driver, "up")
                await asyncio.sleep(0.5)
                trace_window(driver, "top")
                driver.click(420, 45)
                driver.click(300, 45)
                await asyncio.sleep(0.8)
                assert portal.capture_requests == 0, (
                    "invalid webcam preferences must prevent a recording portal request"
                )
                trace_window(driver, "blocked")

                for invalid_shape in ("oval", None):
                    invalid = {**saved, "webcam_shape": invalid_shape}
                    settings.write_text(json.dumps(invalid))
                    before_status = pixels(driver, 0, 480, 800, 20)[1]
                    driver.click(300, 45)
                    await wait_for(
                        lambda: pixels(driver, 0, 480, 800, 20)[1] != before_status,
                        f"the invalid-shape error for {invalid_shape!r}",
                    )
                    assert portal.capture_requests == 0, (
                        f"invalid webcam shape {invalid_shape!r} must gate recording"
                    )
                    assert json.loads(settings.read_text()) == invalid, (
                        "invalid webcam settings must not be rewritten"
                    )

                settings.write_text(json.dumps(saved))
                driver.click(300, 45)
                await wait_for(
                    lambda: portal.capture_requests == 1,
                    "the recording portal request after restoring valid preferences",
                )
                print(
                    "GUI saved all webcam positions, reloaded a circle, "
                    "rejected invalid values, and gated recording"
                )
        except BaseException:
            if driver is not None:
                trace_window(driver, "failure")
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
