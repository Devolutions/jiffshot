#!/usr/bin/env python3
"""Verify Linux interval capture controls with a mock or real desktop portal."""

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

from linux_load_e2e import window_info
from linux_shortcuts_e2e import MockPortal, X11Driver, stop, wait_for
from linux_webcam_e2e import TextReader, pixels, replace, scroll, trace_window


def saved_images(directory):
    images = []
    for path in directory.glob("Screenshot-*.png"):
        if path.read_bytes().endswith(b"IEND\xaeB`\x82"):
            images.append(path)
    return sorted(images)


def verify_image(path, work, expected):
    header = path.read_bytes()[:24]
    assert header[:8] == b"\x89PNG\r\n\x1a\n", f"{path} is not a PNG"
    width, height = struct.unpack(">II", header[16:24])
    assert (width, height) == (1280, 720), (width, height)
    raw = work / "screen.rgba"
    subprocess.run(
        ["gst-launch-1.0", "-q", "filesrc", f"location={path}", "!",
         "pngdec", "!", "videoconvert", "!", "video/x-raw,format=RGBA",
         "!", "filesink", f"location={raw}"],
        check=True,
    )
    data = raw.read_bytes()
    assert len(data) == width * height * 4
    offset = ((height // 2) * width + width // 2) * 4
    color = data[offset:offset + 3]
    assert all(abs(actual - wanted) <= 8 for actual, wanted in zip(color, expected)), (
        path, list(color), expected
    )


async def mock_checks(driver, portal, settings, original):
    scroll(driver, "down")
    await asyncio.sleep(0.5)
    driver.click(680, 115)
    await asyncio.sleep(0.2)
    scroll(driver, "down")
    await asyncio.sleep(0.5)
    trace_window(driver, "interval-settings")

    before_status = pixels(driver, 0, 480, 800, 20)[1]
    replace(driver, 220, 182, "0")
    driver.click(550, 182)
    await wait_for(
        lambda: pixels(driver, 0, 480, 800, 20)[1] != before_status,
        "invalid interval error",
    )
    assert portal.capture_requests == 0, "zero seconds must not request the portal"
    assert json.loads(settings.read_text()) == original, "invalid input must not alter settings"

    replace(driver, 220, 182, "2")
    driver.click(270, 182)
    previous = settings.stat().st_ino
    driver.click(550, 182)
    await wait_for(lambda: settings.stat().st_ino != previous, "saving interval preferences")
    expected = {**original, "interval_seconds": 2, "interval_skip_identical": False}
    assert json.loads(settings.read_text()) == expected
    await wait_for(lambda: len(portal.screen_starts) == 1, "interval source chooser")
    assert portal.screen_selection_options[-1]["cursor_mode"].value == 1, (
        "interval capture must request the saved hidden-cursor setting"
    )

    scroll(driver, "up")
    await asyncio.sleep(0.4)
    driver.click(300, 45)
    driver.click(300, 15)
    await asyncio.sleep(0.4)
    assert portal.capture_requests == 1, (
        "recording and scrolling capture must not start during interval capture"
    )
    scroll(driver, "down")
    await asyncio.sleep(0.4)
    driver.click(550, 182)
    session, request = portal.screen_starts[-1]
    portal.response(request, 1, {})
    await wait_for(lambda: session in portal.closes, "closing the stopped portal session")
    await asyncio.sleep(0.4)
    assert not list(Path(expected["capture_directory"]).iterdir())

    driver.click(550, 182)
    await wait_for(lambda: len(portal.screen_starts) == 2, "restarting interval capture")
    assert json.loads(settings.read_text()) == expected, "restarting must retain saved preferences"
    driver.click(550, 182)
    session, request = portal.screen_starts[-1]
    portal.response(request, 1, {})
    await wait_for(lambda: session in portal.closes, "closing the restarted portal session")
    await asyncio.sleep(0.3)

    scroll(driver, "up")
    await asyncio.sleep(0.4)
    driver.click(300, 45)
    await wait_for(lambda: len(portal.screen_starts) == 3, "recording source chooser")
    scroll(driver, "down")
    await asyncio.sleep(0.4)
    driver.click(550, 182)
    await asyncio.sleep(0.3)
    assert portal.capture_requests == 3, "an active recording must block interval capture"
    scroll(driver, "up")
    await asyncio.sleep(0.4)
    driver.click(550, 140)
    session, _request = portal.screen_starts[-1]
    await wait_for(lambda: session in portal.closes, "closing the cancelled recording")
    await asyncio.sleep(0.3)

    driver.click(300, 15)
    await wait_for(lambda: len(portal.screen_starts) == 4, "scrolling source chooser")
    scroll(driver, "down")
    await asyncio.sleep(0.4)
    driver.click(550, 182)
    await asyncio.sleep(0.3)
    assert portal.capture_requests == 4, "active scrolling must block interval capture"
    scroll(driver, "up")
    await asyncio.sleep(0.4)
    driver.click(510, 15)
    session, request = portal.screen_starts[-1]
    portal.response(request, 1, {})
    await wait_for(lambda: session in portal.closes, "closing the cancelled scrolling session")
    print("GUI validated interval settings, cursor mode, mutual exclusion and stop/restart")


async def wait_for_identical_copy(source, destination):
    copied = destination / source.name

    def complete_copy():
        return copied.exists() and copied.read_bytes() == source.read_bytes()

    await wait_for(complete_copy, f"the byte-identical copy of {source.name}", 15)
    assert copied.read_bytes() == source.read_bytes()


async def test_real_interval_auto_copy_matches_saved_images(source, destination):
    await wait_for_identical_copy(source, destination)


def replace_settings(settings, values):
    temporary = settings.with_name("settings-e2e-replacement.json")
    temporary.write_text(json.dumps(values))
    os.replace(temporary, settings)


async def test_real_interval_final_result_reports_copy_error(
    driver, text, original, missing_folder,
):
    # Progress says "Automatic copy failed:", whereas the final result
    # includes the number of failed copies before any potentially long path.
    await text.wait_for_status(driver, "Automatic copy failed for", 15)
    final_status = text.status(driver)
    assert "automatic copy failed for" in final_status.casefold(), (
        "the final interval result must visibly retain copy errors", final_status
    )
    assert original.exists(), "copy failures must preserve interval originals"
    assert not missing_folder.exists()


async def test_real_recording_auto_copy_is_byte_identical(driver, text, captures, destination):
    scroll(driver, "up")
    await asyncio.sleep(0.5)
    previous = set(captures.glob("Recording-*.mp4"))
    driver.click(300, 45)
    await text.wait_for_status(driver, "Screen recording is active", 25)
    # Allow actual PipeWire frames to reach the encoder before stopping.
    await asyncio.sleep(1)
    driver.click(300, 45)
    await text.wait_for_status(driver, "Screen recording saved", 30)
    added = set(captures.glob("Recording-*.mp4")) - previous
    assert len(added) == 1, f"expected one completed real recording, got {added}"
    recording = next(iter(added))
    assert recording.stat().st_size > 1000, "the original must contain encoded video"
    await wait_for_identical_copy(recording, destination)
    subprocess.run(
        ["gst-launch-1.0", "-q", "filesrc", f"location={recording}", "!",
         "qtdemux", "!", "h264parse", "!", "avdec_h264", "!", "fakesink"],
        check=True, timeout=20,
    )
    print("PASS test_real_recording_auto_copy_is_byte_identical", flush=True)


async def real_checks(driver, settings, original, work, captures, first_folder, second_folder):
    text = TextReader()
    try:
        await real_auto_copy_checks(
            driver, settings, original, work, captures, first_folder, second_folder, text,
        )
    finally:
        text.close()


async def real_auto_copy_checks(
    driver, settings, original, work, captures, first_folder, second_folder, text,
):
    await wait_for(lambda: len(saved_images(captures)) >= 1, "first interval screenshot", 20)
    first = saved_images(captures)[0]
    verify_image(first, work, (0x20, 0x40, 0x60))
    await test_real_interval_auto_copy_matches_saved_images(first, first_folder)
    await asyncio.sleep(2.4)
    assert len(saved_images(captures)) == 1, "identical frames should not be saved again"

    original["linux_auto_copy_directory"] = str(second_folder)
    replace_settings(settings, original)
    color = Path(os.environ["JIFFSHOT_DELAY_COLOR_FILE"])
    color.write_text("a02040")
    await wait_for(lambda: len(saved_images(captures)) >= 2, "changed interval screenshot", 8)
    second = saved_images(captures)[-1]
    verify_image(second, work, (0xA0, 0x20, 0x40))
    await test_real_interval_auto_copy_matches_saved_images(second, second_folder)
    assert {path.name for path in first_folder.glob("*.png")} == {first.name}, (
        "a running interval must use the changed destination for the next image"
    )
    print("PASS test_real_interval_auto_copy_matches_saved_images", flush=True)

    missing_folder = work / "removed-interval-destination"
    missing_folder.mkdir()
    replace_settings(settings, {**original, "linux_auto_copy_directory": str(missing_folder)})
    missing_folder.rmdir()
    color.write_text("605020")
    await wait_for(lambda: len(saved_images(captures)) >= 3, "interval image after folder removal", 8)
    failed_copy_original = saved_images(captures)[-1]
    verify_image(failed_copy_original, work, (0x60, 0x50, 0x20))
    scroll(driver, "down")
    await asyncio.sleep(0.5)
    trace_window(driver, "interval-real-active")
    driver.click(550, 182)
    await test_real_interval_final_result_reports_copy_error(
        driver, text, failed_copy_original, missing_folder,
    )
    print("PASS test_real_interval_final_result_reports_copy_error", flush=True)
    stopped = len(saved_images(captures))
    color.write_text("204060")
    await asyncio.sleep(2)
    assert len(saved_images(captures)) == stopped, "stopping must end interval capture"

    replace_settings(settings, original)
    driver.click(270, 182)
    driver.click(550, 182)
    await wait_for(
        lambda: json.loads(settings.read_text()).get("interval_skip_identical") is False,
        "saving repeat-identical preference",
    )
    await wait_for(
        lambda: len(saved_images(captures)) >= stopped + 2,
        "two repeated screenshots",
        10,
    )
    for path in saved_images(captures)[stopped:]:
        await test_real_interval_auto_copy_matches_saved_images(path, second_folder)
    assert json.loads(settings.read_text()) == {
        **original, "interval_skip_identical": False,
    }
    driver.click(550, 182)
    await asyncio.sleep(0.5)
    stopped = len(saved_images(captures))
    await asyncio.sleep(2)
    assert len(saved_images(captures)) == stopped, "restarted capture must also stop"
    print("Real portal saved changed and repeated images and stopped both sessions")
    await test_real_recording_auto_copy_is_byte_identical(driver, text, captures, second_folder)


async def main(binary, runtime, real_portal):
    portal = None
    if not real_portal:
        portal = MockPortal()
        portal.screen_mode = "hold-start"
        await portal.start()
    app = server = driver = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-interval-ui-") as directory:
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
            "capture_cursor": False,
            "interval_seconds": 1 if real_portal else 5,
            "interval_skip_identical": True,
            "other": {"keep": True},
        }
        first_folder = work / "interval-copy-first"
        second_folder = work / "interval-copy-second"
        if real_portal:
            first_folder.mkdir()
            second_folder.mkdir()
            original["linux_auto_copy_directory"] = str(first_folder)
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
                display = ":" + os.read(read_fd, 128).decode("ascii").strip()
                env = {
                    **os.environ,
                    "DISPLAY": display,
                    "HOME": str(work),
                    "XDG_CONFIG_HOME": str(work / "config"),
                    "XDG_DATA_HOME": str(work / "data"),
                    "XDG_CACHE_HOME": str(work / "cache"),
                    "XDG_RUNTIME_DIR": os.environ["XDG_RUNTIME_DIR"] if real_portal else str(xdg_runtime),
                    "XDG_SESSION_TYPE": "x11",
                    "RUSTOLONIA_HOST_LIB": str(runtime / "librustolonia_host.so"),
                    "LD_LIBRARY_PATH": os.pathsep.join(
                        filter(None, (str(runtime), os.environ.get("LD_LIBRARY_PATH")))
                    ),
                }
                env.pop("WAYLAND_DISPLAY", None)
                arguments = [str(binary)]
                if real_portal:
                    arguments.append("--interval")
                app = subprocess.Popen(
                    arguments, env=env, cwd=work, stdin=subprocess.DEVNULL,
                    stdout=app_log, stderr=app_log,
                )
                await wait_for(lambda: window_info(env), "the editor", 12)
                window, origin = window_info(env)
                driver = X11Driver(display, window, origin)
                await asyncio.sleep(0.5)
                if real_portal:
                    await real_checks(
                        driver, settings, original, work, captures, first_folder, second_folder,
                    )
                else:
                    await mock_checks(driver, portal, settings, original)
                    driver.close()
                    driver = None
                    stop(app)
                    app = None
                    invalid = {**original, "interval_seconds": 0}
                    settings.write_text(json.dumps(invalid))
                    previous = portal.capture_requests
                    app = subprocess.Popen(
                        [str(binary), "--interval"], env=env, cwd=work, stdin=subprocess.DEVNULL,
                        stdout=app_log, stderr=app_log,
                    )
                    await wait_for(lambda: window_info(env), "the interval CLI editor", 12)
                    await asyncio.sleep(0.8)
                    assert portal.capture_requests == previous, (
                        "--interval must reject invalid saved settings before requesting a source"
                    )
                    assert json.loads(settings.read_text()) == invalid
                    print("Interval CLI rejected invalid saved preferences")
        except BaseException:
            if driver is not None:
                trace_window(driver, "interval-failure")
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
    parser.add_argument("--real-portal", action="store_true")
    arguments = parser.parse_args()
    asyncio.run(main(
        arguments.binary.resolve(strict=True),
        arguments.runtime_dir.resolve(strict=True),
        arguments.real_portal,
    ))
