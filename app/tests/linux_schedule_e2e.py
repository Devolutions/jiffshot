#!/usr/bin/env python3
"""Verify daily capture scheduling with a mock or real desktop portal."""

import argparse
import asyncio
import datetime
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile

from linux_interval_e2e import saved_images, verify_image, wait_for_identical_copy
from linux_load_e2e import solid_png, window_info
from linux_shortcuts_e2e import MockPortal, X11Driver, stop, wait_for
from linux_webcam_e2e import press, replace, scroll, trace_window


async def safe_current_minute():
    while True:
        now = datetime.datetime.now().astimezone()
        if now.second < 35:
            return now
        await asyncio.sleep(1)


def replace_time(driver, value):
    driver.click(195, 182)
    driver.select_all()
    for character in value:
        if character == ":":
            driver.key("Shift_L", True)
            press(driver, "semicolon")
            driver.key("Shift_L", False)
        else:
            press(driver, character)
    driver.x11.XSync(driver.display, 0)


async def show_schedule_options(driver):
    scroll(driver, "down")
    await asyncio.sleep(0.5)
    driver.click(215, 182)
    await asyncio.sleep(0.3)
    scroll(driver, "down")
    await asyncio.sleep(0.5)


async def test_real_scheduled_capture_is_copied_byte_identically(source, destination):
    await wait_for_identical_copy(source, destination)


async def main(binary, runtime, real_portal):
    portal = None
    if not real_portal:
        portal = MockPortal()
        portal.screen_mode = "hold-start"
        await portal.start()
    app = server = driver = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-schedule-ui-") as directory:
        work = Path(directory)
        runtime_dir = work / "runtime"
        runtime_dir.mkdir(mode=0o700)
        config = work / "config" / "Jiffshot"
        config.mkdir(parents=True)
        captures = work / "captures"
        captures.mkdir()
        automatic_copies = work / "automatic-copies"
        if real_portal:
            automatic_copies.mkdir()
        settings = config / "settings.json"
        image = work / "red.png"
        image.write_bytes(solid_png(220, 20, 20))
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
                    "XDG_RUNTIME_DIR": os.environ["XDG_RUNTIME_DIR"] if real_portal else str(runtime_dir),
                    "XDG_SESSION_TYPE": "x11",
                    "RUSTOLONIA_HOST_LIB": str(runtime / "librustolonia_host.so"),
                    "LD_LIBRARY_PATH": os.pathsep.join(
                        filter(None, (str(runtime), os.environ.get("LD_LIBRARY_PATH")))
                    ),
                }
                env.pop("WAYLAND_DISPLAY", None)
                now = await safe_current_minute()
                scheduled = now.strftime("%H:%M")
                today = now.strftime("%Y-%j")
                original = {
                    "theme": "Light",
                    "capture_directory": str(captures),
                    "copy_to_clipboard": True,
                    "capture_cursor": False,
                    "schedule": scheduled,
                    "schedule_default": scheduled,
                    "other": {"keep": True},
                }
                if real_portal:
                    original["linux_auto_copy_directory"] = str(automatic_copies)
                settings.write_text(json.dumps(original))
                if real_portal:
                    subprocess.run(
                        ["xclip", "-selection", "clipboard"],
                        input=b"clipboard must remain unchanged", env=env, check=True,
                    )

                def start_app():
                    return subprocess.Popen(
                        [str(binary), str(image)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                        stdout=app_log, stderr=app_log,
                    )

                app = start_app()
                await wait_for(lambda: window_info(env), "the editor", 12)
                window, origin = window_info(env)
                driver = X11Driver(display, window, origin)
                if real_portal:
                    await wait_for(lambda: len(saved_images(captures)) == 1, "daily screenshot", 25)
                    verify_image(saved_images(captures)[0], work, (0x20, 0x40, 0x60))
                    await test_real_scheduled_capture_is_copied_byte_identically(
                        saved_images(captures)[0], automatic_copies,
                    )
                    print("PASS test_real_scheduled_capture_is_copied_byte_identically", flush=True)
                    await wait_for(lambda: driver.red_canvas_pixels() > 2000, "original editor image")
                    clipboard = subprocess.run(
                        ["xclip", "-selection", "clipboard", "-o"], env=env,
                        capture_output=True, check=True, timeout=10,
                    ).stdout
                    assert clipboard == b"clipboard must remain unchanged"
                    await asyncio.sleep(2)
                    assert len(saved_images(captures)) == 1, "daily capture must not repeat"
                    print("Real portal saved one image without changing the editor or clipboard")
                else:
                    await wait_for(lambda: len(portal.screen_starts) == 1, "scheduled chooser")
                    assert portal.screen_selection_options[-1]["cursor_mode"].value == 1
                    assert json.loads(settings.read_text()) == {
                        **original, "linux_schedule_last_fired": today,
                    }
                    driver.click(80, 15)
                    driver.click(300, 15)
                    driver.click(300, 45)
                    await asyncio.sleep(0.5)
                    assert portal.capture_requests == 1, (
                        "another screenshot, scrolling capture, or recording must not start "
                        "while the scheduled chooser is open"
                    )
                    session, request = portal.screen_starts[-1]
                    portal.response(request, 1, {})
                    await wait_for(lambda: session in portal.closes, "cancelled scheduled session")
                    await asyncio.sleep(1.5)
                    assert portal.capture_requests == 1, "a cancelled attempt must not repeat"
                    assert not saved_images(captures), "a cancelled scheduled capture must not save"
                    driver.close()
                    driver = None
                    stop(app)
                    app = start_app()
                    await wait_for(lambda: window_info(env), "the restarted editor", 12)
                    window, origin = window_info(env)
                    driver = X11Driver(display, window, origin)
                    await asyncio.sleep(1.5)
                    assert portal.capture_requests == 1, "a restart must not repeat today's capture"
                    await show_schedule_options(driver)
                    trace_window(driver, "schedule-enabled-options")
                    unchanged = json.loads(settings.read_text())
                    replace(driver, 195, 182, "2500")
                    driver.click(480, 182)
                    await asyncio.sleep(0.3)
                    assert json.loads(settings.read_text()) == unchanged, (
                        "invalid time must not overwrite saved preferences"
                    )

                    other_time = "00:00" if scheduled != "00:00" else "23:59"
                    replace_time(driver, other_time)
                    driver.click(245, 182)
                    driver.click(480, 182)
                    await wait_for(
                        lambda: json.loads(settings.read_text()).get("schedule") is None,
                        "disabling the daily schedule",
                    )
                    disabled = json.loads(settings.read_text())
                    assert disabled == {
                        **original, "schedule": None, "schedule_default": other_time,
                        "linux_schedule_last_fired": today,
                    }, disabled
                    driver.click(245, 182)
                    driver.click(480, 182)
                    await wait_for(
                        lambda: json.loads(settings.read_text()).get("schedule") == other_time,
                        "re-enabling the daily schedule",
                    )
                    assert json.loads(settings.read_text()) == {
                        **disabled, "schedule": other_time,
                    }
                    await asyncio.sleep(1)
                    assert portal.capture_requests == 1, "changing a schedule must not duplicate it"

                    driver.close()
                    driver = None
                    stop(app)
                    now = await safe_current_minute()
                    scheduled = now.strftime("%H:%M")
                    today = now.strftime("%Y-%j")
                    busy_settings = {
                        **original, "schedule": None, "schedule_default": scheduled,
                    }
                    settings.write_text(json.dumps(busy_settings))
                    app = start_app()
                    await wait_for(lambda: window_info(env), "the editor for a busy schedule", 12)
                    window, origin = window_info(env)
                    driver = X11Driver(display, window, origin)
                    await asyncio.sleep(0.5)
                    driver.click(300, 45)
                    await wait_for(lambda: len(portal.screen_starts) == 2, "recording chooser")
                    await show_schedule_options(driver)
                    driver.click(245, 182)
                    driver.click(480, 182)
                    await wait_for(
                        lambda: json.loads(settings.read_text()).get("linux_schedule_last_fired") == today,
                        "skipping the scheduled capture while recording",
                    )
                    assert portal.capture_requests == 2, (
                        "a scheduled capture must not open another chooser during recording"
                    )
                    scroll(driver, "up")
                    await asyncio.sleep(0.4)
                    driver.click(560, 140)
                    recording_session, _ = portal.screen_starts[-1]
                    await wait_for(lambda: recording_session in portal.closes, "cancelled recording")
                    await asyncio.sleep(1.5)
                    assert portal.capture_requests == 2, "a busy skipped capture must not retry"
                    print(
                        "Scheduled chooser, mutual exclusion, cancellation, restart, "
                        "settings validation and busy-schedule behavior verified"
                    )
        except BaseException:
            if driver is not None:
                trace_window(driver, "schedule-failure")
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
