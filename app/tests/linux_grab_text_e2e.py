#!/usr/bin/env python3
"""Verify screen text capture through a mock Screenshot portal and real OCR."""

import argparse
import asyncio
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile

from dbus_next import Variant
from linux_load_e2e import draw_arrow, solid_png, window_info
from linux_shortcuts_e2e import MockPortal, X11Driver, stop, wait_for
from linux_webcam_e2e import TextReader, pixels, trace_window


def clipboard_text(env):
    result = subprocess.run(
        ["xclip", "-selection", "clipboard", "-o"], env=env,
        capture_output=True, timeout=5,
    )
    return result.stdout.decode("utf-8", errors="replace") if result.returncode == 0 else ""


def set_clipboard(env, text):
    subprocess.run(
        ["xclip", "-selection", "clipboard"], input=text.encode(), env=env, check=True,
    )


def test_grab_text_never_creates_automatic_copy(destination):
    assert not list(destination.iterdir()), (
        "screen text recognition must not create an automatic capture copy"
    )


async def main(binary, runtime, fixture):
    portal = MockPortal()
    portal.screenshot_mode = "hold"
    await portal.start()
    app = server = driver = text = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-grab-text-ui-") as directory:
        work = Path(directory)
        runtime_dir = work / "runtime"
        runtime_dir.mkdir(mode=0o700)
        config = work / "config" / "Jiffshot"
        config.mkdir(parents=True)
        captures = work / "captures"
        captures.mkdir()
        automatic_copies = work / "automatic-copies"
        automatic_copies.mkdir()
        (config / "settings.json").write_text(json.dumps({
            "theme": "Light", "capture_directory": str(captures),
            "linux_auto_copy_directory": str(automatic_copies),
        }))
        empty_image = work / "empty.png"
        empty_image.write_bytes(solid_png(255, 255, 255))
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
                for mode in ("text", "empty", "cancel", "invalid"):
                    sentinel = f"clipboard-{mode}-unchanged"
                    set_clipboard(env, sentinel)
                    expected_requests = len(portal.screenshot_requests) + 1
                    app = subprocess.Popen(
                        [str(binary), "--grab-text"], env=env, cwd=work,
                        stdin=subprocess.DEVNULL, stdout=app_log, stderr=app_log,
                    )
                    await wait_for(lambda: window_info(env), f"the editor for {mode}", 12)
                    window, origin = window_info(env)
                    driver = X11Driver(display, window, origin)
                    await wait_for(
                        lambda: len(portal.screenshot_requests) == expected_requests,
                        f"the screenshot request for {mode}",
                    )
                    request, options = portal.screenshot_requests[-1]
                    assert options["interactive"].value is True
                    assert options["modal"].value is True
                    await asyncio.sleep(0.5)
                    if mode == "text":
                        draw_arrow(driver)
                        await wait_for(lambda: driver.red_canvas_pixels() > 40, "an editor annotation")
                        before_pixels = driver.red_canvas_pixels()
                    before_status = pixels(driver, 0, 480, 800, 20)[1]
                    if mode == "cancel":
                        portal.response(request, 1, {})
                    else:
                        uri = (
                            fixture.as_uri() if mode == "text" else empty_image.as_uri()
                            if mode == "empty" else "https://example.invalid/screenshot.png"
                        )
                        portal.response(request, 0, {"uri": Variant("s", uri)})
                    if mode == "text":
                        await wait_for(
                            lambda: "capture" in clipboard_text(env).lower(),
                            "recognized screenshot text in the clipboard", 25,
                        )
                        after_pixels = driver.red_canvas_pixels()
                        assert before_pixels * 0.8 <= after_pixels <= before_pixels * 1.5 + 50, (
                            "screen OCR must not replace the edited document", before_pixels, after_pixels
                        )
                    else:
                        await wait_for(
                            lambda: pixels(driver, 0, 480, 800, 20)[1] != before_status,
                            f"the {mode} result",
                        )
                        assert clipboard_text(env) == sentinel, (
                            f"{mode} must leave the clipboard unchanged"
                        )
                    assert not list(captures.iterdir()), "grabbing text must not save a capture"
                    test_grab_text_never_creates_automatic_copy(automatic_copies)
                    assert portal.capture_requests == 0, "grabbing text must use Screenshot, not ScreenCast"
                    driver.close()
                    driver = None
                    stop(app)
                    app = None
                app = subprocess.Popen(
                    [str(binary)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                    stdout=app_log, stderr=app_log,
                )
                await wait_for(lambda: window_info(env), "the editor for the Grab text control", 12)
                window, origin = window_info(env)
                driver = X11Driver(display, window, origin)
                await asyncio.sleep(0.5)
                text = TextReader()
                position = await text.find_scrolling_label(driver, "Grab text")
                trace_window(driver, "grab-text-control")
                set_clipboard(env, "from-gui-before")
                expected_requests = len(portal.screenshot_requests) + 1
                driver.click(*position)
                await wait_for(
                    lambda: len(portal.screenshot_requests) == expected_requests,
                    "the Grab text control's screenshot chooser",
                )
                request, options = portal.screenshot_requests[-1]
                assert options["interactive"].value is True
                portal.response(request, 0, {"uri": Variant("s", fixture.as_uri())})
                await wait_for(
                    lambda: "capture" in clipboard_text(env).lower(),
                    "recognized text from the Grab text control", 25,
                )
                assert not list(captures.iterdir())
                test_grab_text_never_creates_automatic_copy(automatic_copies)
                print("PASS test_grab_text_never_creates_automatic_copy", flush=True)
                print("Screen text OCR copied selected text from CLI and GUI, preserved the editor, and left the clipboard unchanged for empty, cancelled and invalid selections")
        except BaseException:
            if driver is not None:
                trace_window(driver, "grab-text-failure")
            for name in ("app.log", "xvfb.log"):
                path = work / name
                if path.exists():
                    print(path.read_text(errors="replace")[-12000:], file=sys.stderr)
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
            portal.bus.disconnect()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--runtime-dir", type=Path, required=True)
    parser.add_argument("--ocr-fixture", type=Path)
    arguments = parser.parse_args()
    fixture = arguments.ocr_fixture or Path(__file__).resolve().parents[2] / "docs" / "images" / "editor.png"
    asyncio.run(main(
        arguments.binary.resolve(strict=True),
        arguments.runtime_dir.resolve(strict=True),
        fixture.resolve(strict=True),
    ))
