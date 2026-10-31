#!/usr/bin/env python3
"""Exercise Linux screen capture presets with a private X11 desktop and portal.

The optional real-portal mode uses the isolated Sway fixture's color-control file.
"""

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

from linux_interactive_effects_e2e import wheel
from linux_interval_e2e import saved_images, verify_image
from linux_load_e2e import window_info
from linux_shortcuts_e2e import X11Driver, stop, wait_for
from linux_timed_capture_e2e import TimedPortal
from linux_webcam_e2e import TextReader, press, scroll, trace_window


PRESETS = "linux_screen_capture_presets"


class Fixture:
    def __init__(self, binary, runtime, work, portal, text, env, app_log):
        self.binary = binary
        self.runtime = runtime
        self.work = work
        self.portal = portal
        self.text = text
        self.env = env
        self.app_log = app_log
        self.app = None
        self.driver = None
        self.settings = work / "config" / "Jiffshot" / "settings.json"
        self.captures = work / "captures"
        self.original = {
            "theme": "Light",
            "capture_directory": str(self.captures),
            "capture_delay_seconds": 2,
            "capture_cursor": False,
            "recording_cursor": True,
            "copy_to_clipboard": False,
            "other": {"keep": [1, "two"]},
        }

    def preferences(self):
        return json.loads(self.settings.read_text())

    async def close_app(self):
        if self.driver is not None:
            self.driver.close()
            self.driver = None
        stop(self.app)
        self.app = None
        await wait_for(lambda: window_info(self.env) is None, "the previous editor to close")

    async def launch(self, preferences=None):
        if self.app is not None:
            await self.close_app()
        if preferences is not None:
            self.settings.write_text(json.dumps(preferences))
        self.app = subprocess.Popen(
            [str(self.binary)], env=self.env, cwd=self.work, stdin=subprocess.DEVNULL,
            stdout=self.app_log, stderr=self.app_log,
        )
        await wait_for(lambda: window_info(self.env), "the editor", 15)
        window, origin = window_info(self.env)
        self.driver = X11Driver(self.env["DISPLAY"], window, origin)
        await asyncio.sleep(0.4)

    async def label(self, label):
        position = self.text.find_label(self.driver, label)
        if position is not None:
            return position
        scroll(self.driver, "up")
        await asyncio.sleep(0.2)
        for _ in range(25):
            position = self.text.find_label(self.driver, label)
            if position is not None:
                return position
            wheel(self.driver, "down", 2)
            await asyncio.sleep(0.12)
        trace_window(self.driver, "screen-presets-label-not-found")
        observed = self.text._recognize(self.driver, 0, 0, 800, 270, 6)
        raise AssertionError(f"could not find {label!r}; OCR saw {observed!r}")

    async def click_label(self, label):
        self.driver.click(*await self.label(label))
        await asyncio.sleep(0.2)

    async def show_presets(self):
        position = await self.label("Screen presets")
        trace_window(self.driver, "screen-presets-before-toggle")
        self.driver.click(*position)
        await asyncio.sleep(0.3)
        trace_window(self.driver, "screen-presets-after-toggle")
        await self.label("Save screen preset")
        trace_window(self.driver, "screen-presets-controls")

    async def fill(self, x, value):
        # TextReader can merge the textbox border into the preceding label. Anchor
        # the row by its save button and use the established 800-pixel X11 layout.
        _, y = await self.label("Save screen preset")
        self.driver.click(x, y)
        self.driver.select_all()
        press(self.driver, "BackSpace")
        for character in value:
            key = {" ": "space", "-": "minus", ".": "period"}.get(character, character)
            press(self.driver, key)

    async def fill_name(self, value):
        await self.fill(165, value)

    async def fill_delay(self, value):
        await self.fill(390, value)

    async def wait_preferences(self, expected):
        await wait_for(lambda: self.preferences() == expected, "the expected saved preferences")
        assert self.preferences() == expected

    def set_clipboard(self, text):
        subprocess.run(
            ["xclip", "-selection", "clipboard"], input=text.encode(), env=self.env,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True, timeout=5,
        )

    def clipboard_text(self):
        return subprocess.check_output(
            ["xclip", "-selection", "clipboard", "-o"], env=self.env, timeout=5,
        ).decode()


async def start_chooser(fixture, label):
    previous = len(fixture.portal.screen_starts)
    await fixture.click_label(label)
    await wait_for(
        lambda: len(fixture.portal.screen_starts) == previous + 1,
        f"the ScreenCast chooser for {label}",
    )
    options = fixture.portal.screen_selection_options[-1]
    assert options["types"].value == 3, "the chooser must offer monitors and windows"
    assert options["multiple"].value is False, "the chooser must select one source"
    return fixture.portal.screen_starts[-1], options


async def reject_chooser(fixture, session, request):
    fixture.portal.response(request, 1, {})
    await wait_for(lambda: session in fixture.portal.closes, "the cancelled portal session to close")
    await asyncio.sleep(0.2)


async def test_screen_preset_save_replacement_and_independent_preferences(fixture):
    await fixture.launch(fixture.original)
    await fixture.show_presets()
    _, row = await fixture.label("Save screen preset")
    name_label = fixture.text._recognize(fixture.driver, 0, row - 12, 92, 24, 7)
    delay_label = fixture.text._recognize(fixture.driver, 243, row - 18, 118, 22, 7)
    assert "presetname" in "".join(c for c in name_label.casefold() if c.isalnum()), name_label
    assert "delayseconds" in "".join(c for c in delay_label.casefold() if c.isalnum()), delay_label
    await fixture.fill_name("  work  ")
    await fixture.fill_delay("5")
    await fixture.click_label("Preset includes cursor")
    await fixture.click_label("Save screen preset")
    expected = {
        **fixture.original,
        PRESETS: [{"name": "work", "delay_seconds": 5, "include_cursor": True}],
    }
    await fixture.wait_preferences(expected)
    await fixture.label("Capture: work")
    await fixture.text.wait_for_status(fixture.driver, "preset")

    await fixture.fill_name(" work ")
    await fixture.fill_delay("60")
    await fixture.click_label("Preset includes cursor")
    await fixture.click_label("Save screen preset")
    expected[PRESETS] = [{"name": "work", "delay_seconds": 60, "include_cursor": False}]
    await fixture.wait_preferences(expected)
    fixture.portal.hold_start = False
    remote_opens = fixture.portal.remote_opens
    (session, _request), options = await start_chooser(fixture, "Capture: work")
    assert options["cursor_mode"].value == 1, "the replaced preset must use its new cursor"
    await wait_for(lambda: fixture.portal.remote_opens == remote_opens + 1, "the replaced stream")
    await fixture.text.wait_for_status(fixture.driver, "Capturing in 60 seconds")
    await fixture.click_label("Cancel delayed capture")
    await wait_for(lambda: session in fixture.portal.closes, "the replaced session to close")
    await fixture.text.wait_for_status(fixture.driver, "Delayed capture cancelled")
    assert not list(fixture.captures.iterdir())
    assert fixture.preferences() == expected


async def test_screen_preset_invalid_input_does_not_overwrite(fixture):
    initial = {
        **fixture.original,
        PRESETS: [{"name": "keep", "delay_seconds": 1, "include_cursor": False}],
    }
    await fixture.launch(initial)
    await fixture.show_presets()
    before = fixture.settings.read_bytes()
    previous_requests = fixture.portal.capture_requests
    for name, delay in [
        ("", "1"),
        ("   ", "1"),
        ("valid", ""),
        ("valid", "-1"),
        ("valid", "1.5"),
        ("valid", "61"),
    ]:
        await fixture.fill_name("missing")
        await fixture.click_label("Delete screen preset")
        await fixture.text.wait_for_status(fixture.driver, "Could not delete")
        assert fixture.settings.read_bytes() == before, "missing deletion must not write"
        await fixture.fill_name(name)
        await fixture.fill_delay(delay)
        await fixture.click_label("Save screen preset")
        await fixture.text.wait_for_status(fixture.driver, "Could not save")
        assert fixture.settings.read_bytes() == before, (name, delay)
        assert fixture.portal.capture_requests == previous_requests, (name, delay)
    assert fixture.preferences() == initial


async def test_screen_preset_survives_restart_and_can_be_deleted(fixture):
    await fixture.launch(fixture.original)
    await fixture.show_presets()
    await fixture.fill_name("persist")
    await fixture.fill_delay("4")
    await fixture.click_label("Save screen preset")
    saved = {
        **fixture.original,
        PRESETS: [{"name": "persist", "delay_seconds": 4, "include_cursor": False}],
    }
    await fixture.wait_preferences(saved)
    await fixture.close_app()
    await fixture.launch()
    await fixture.show_presets()
    await fixture.label("Capture: persist")
    assert fixture.preferences() == saved, "restart must retain all preset options"

    before = fixture.settings.read_bytes()
    await fixture.fill_name("missing")
    await fixture.click_label("Delete screen preset")
    await fixture.text.wait_for_status(fixture.driver, "Could not delete")
    assert fixture.settings.read_bytes() == before, "missing deletion must not rewrite settings"

    await fixture.fill_name(" persist ")
    await fixture.click_label("Delete screen preset")
    deleted = {**fixture.original, PRESETS: []}
    await fixture.wait_preferences(deleted)
    await fixture.text.wait_for_status(fixture.driver, "deleted")
    assert fixture.text.find_label(fixture.driver, "Capture: persist") is None
    await fixture.close_app()
    await fixture.launch()
    await fixture.show_presets()
    assert fixture.preferences() == deleted
    assert fixture.text.find_label(fixture.driver, "Capture: persist") is None


async def test_screen_preset_cursor_mode_and_normal_capture_are_independent(fixture):
    fixture.portal.hold_start = True
    presets = [
        {"name": "with", "delay_seconds": 60, "include_cursor": True},
        {"name": "without", "delay_seconds": 60, "include_cursor": False},
    ]
    for normal_cursor, preset_label, preset_mode, normal_mode in [
        (False, "Capture: with", 2, 1),
        (True, "Capture: without", 1, 2),
    ]:
        initial = {
            **fixture.original,
            "capture_delay_seconds": 30,
            "capture_cursor": normal_cursor,
            PRESETS: presets,
        }
        await fixture.launch(initial)
        await fixture.show_presets()
        before = fixture.settings.read_bytes()
        (session, request), options = await start_chooser(fixture, preset_label)
        assert options["cursor_mode"].value == preset_mode, options
        await reject_chooser(fixture, session, request)
        (session, request), options = await start_chooser(fixture, "Capture screen or window")
        assert options["cursor_mode"].value == normal_mode, options
        await reject_chooser(fixture, session, request)
        assert fixture.settings.read_bytes() == before, "running presets must not change preferences"
        assert not list(fixture.captures.iterdir()), "cancelled choosers must not save captures"


async def test_screen_preset_chooser_cancellation_closes_session_without_saving(fixture):
    fixture.portal.hold_start = True
    initial = {
        **fixture.original,
        "copy_to_clipboard": True,
        PRESETS: [{"name": "cancel", "delay_seconds": 60, "include_cursor": True}],
    }
    await fixture.launch(initial)
    fixture.set_clipboard("preset chooser sentinel")
    await fixture.show_presets()
    before = fixture.settings.read_bytes()
    remote_opens = fixture.portal.remote_opens
    (session, request), options = await start_chooser(fixture, "Capture: cancel")
    assert options["cursor_mode"].value == 2
    await reject_chooser(fixture, session, request)
    assert fixture.portal.remote_opens == remote_opens, "rejected chooser must not open PipeWire"
    assert not list(fixture.captures.iterdir()), "chooser cancellation must not save a capture"
    assert fixture.clipboard_text() == "preset chooser sentinel"
    assert fixture.settings.read_bytes() == before

    (session, request), _options = await start_chooser(fixture, "Capture: cancel")
    await reject_chooser(fixture, session, request)
    assert not list(fixture.captures.iterdir()), "another cancelled request must also save nothing"


async def test_screen_preset_cancel_during_chooser_is_honored_after_selection(fixture):
    fixture.portal.hold_start = True
    initial = {
        **fixture.original,
        "copy_to_clipboard": True,
        PRESETS: [{"name": "pending", "delay_seconds": 60, "include_cursor": False}],
    }
    await fixture.launch(initial)
    fixture.set_clipboard("pending preset sentinel")
    await fixture.show_presets()
    before = fixture.settings.read_bytes()
    remote_opens = fixture.portal.remote_opens
    (session, request), options = await start_chooser(fixture, "Capture: pending")
    assert options["cursor_mode"].value == 1
    await fixture.click_label("Cancel delayed capture")
    await fixture.text.wait_for_status(fixture.driver, "Cancellation requested")
    assert fixture.portal.remote_opens == remote_opens
    fixture.portal.release_start(request)
    await wait_for(lambda: fixture.portal.remote_opens == remote_opens + 1, "the selected source")
    await wait_for(lambda: session in fixture.portal.closes, "the cancelled pending session to close")
    await fixture.text.wait_for_status(fixture.driver, "Delayed capture cancelled")
    assert not list(fixture.captures.iterdir())
    assert fixture.clipboard_text() == "pending preset sentinel"
    assert fixture.settings.read_bytes() == before


async def test_screen_preset_countdown_uses_saved_delay_and_can_be_cancelled(fixture):
    fixture.portal.hold_start = False
    initial = {
        **fixture.original,
        "capture_delay_seconds": 30,
        "copy_to_clipboard": True,
        PRESETS: [{"name": "slow", "delay_seconds": 60, "include_cursor": True}],
    }
    await fixture.launch(initial)
    fixture.set_clipboard("preset countdown sentinel")
    await fixture.show_presets()
    before = fixture.settings.read_bytes()
    remote_opens = fixture.portal.remote_opens
    (session, _request), options = await start_chooser(fixture, "Capture: slow")
    assert options["cursor_mode"].value == 2
    await wait_for(lambda: fixture.portal.remote_opens == remote_opens + 1, "the preset stream")
    await fixture.text.wait_for_status(fixture.driver, "Capturing in 60 seconds")
    assert session not in fixture.portal.closes, "the preset must wait before finishing"
    requests = fixture.portal.capture_requests
    await fixture.click_label("Capture: slow")
    await fixture.text.wait_for_status(fixture.driver, "already in progress")
    assert fixture.portal.capture_requests == requests, "an active countdown must block another"
    await fixture.click_label("Cancel delayed capture")
    await wait_for(lambda: session in fixture.portal.closes, "the cancelled preset session to close")
    await fixture.text.wait_for_status(fixture.driver, "Delayed capture cancelled")
    assert not list(fixture.captures.iterdir()), "countdown cancellation must not save a capture"
    assert fixture.clipboard_text() == "preset countdown sentinel"
    assert fixture.settings.read_bytes() == before

    remote_opens = fixture.portal.remote_opens
    (session, _request), options = await start_chooser(fixture, "Capture screen or window")
    assert options["cursor_mode"].value == 1
    await wait_for(lambda: fixture.portal.remote_opens == remote_opens + 1, "the normal stream")
    await fixture.text.wait_for_status(fixture.driver, "Capturing in 30 seconds")
    await fixture.click_label("Cancel delayed capture")
    await wait_for(lambda: session in fixture.portal.closes, "the cancelled normal session to close")
    await fixture.text.wait_for_status(fixture.driver, "Delayed capture cancelled")
    assert not list(fixture.captures.iterdir())
    assert fixture.clipboard_text() == "preset countdown sentinel"
    assert fixture.settings.read_bytes() == before


async def test_screen_preset_invalid_stored_values_remain_unchanged(fixture):
    cases = [
        {"not": "an array"},
        [
            {"name": "dup", "delay_seconds": 0, "include_cursor": False},
            {"name": "dup", "delay_seconds": 2, "include_cursor": True},
        ],
        [{"name": "bad", "delay_seconds": 61, "include_cursor": True}],
    ]
    for invalid in cases:
        initial = {**fixture.original, PRESETS: invalid}
        await fixture.launch(initial)
        before = fixture.settings.read_bytes()
        previous_requests = fixture.portal.capture_requests
        await fixture.text.wait_for_status(fixture.driver, "Could not load screen capture presets")
        await fixture.show_presets()
        await fixture.fill_name("new")
        await fixture.fill_delay("1")
        await fixture.click_label("Save screen preset")
        await fixture.text.wait_for_status(fixture.driver, "Could not save")
        assert fixture.settings.read_bytes() == before, invalid
        await fixture.click_label("Delete screen preset")
        await fixture.text.wait_for_status(fixture.driver, "Could not delete")
        assert fixture.settings.read_bytes() == before, invalid
        assert fixture.portal.capture_requests == previous_requests
        assert not list(fixture.captures.iterdir())


async def test_screen_preset_removed_externally_cannot_start_capture(fixture):
    initial = {
        **fixture.original,
        PRESETS: [{"name": "gone", "delay_seconds": 0, "include_cursor": True}],
    }
    await fixture.launch(initial)
    await fixture.show_presets()
    await fixture.label("Capture: gone")
    fixture.settings.write_text(json.dumps({**fixture.original, PRESETS: []}))
    before = fixture.settings.read_bytes()
    previous_requests = fixture.portal.capture_requests
    await fixture.click_label("Capture: gone")
    await fixture.text.wait_for_status(fixture.driver, "Could not run screen capture preset")
    assert fixture.portal.capture_requests == previous_requests
    assert fixture.settings.read_bytes() == before
    assert not list(fixture.captures.iterdir())


async def test_real_saved_preset_delivers_delayed_png_and_clipboard(fixture):
    color_file = os.environ.get("JIFFSHOT_DELAY_COLOR_FILE")
    if not color_file:
        raise RuntimeError("real portal mode requires JIFFSHOT_DELAY_COLOR_FILE")
    initial = {
        **fixture.original,
        "capture_delay_seconds": 30,
        "copy_to_clipboard": True,
    }
    await fixture.launch(initial)
    fixture.set_clipboard("real preset sentinel")
    await fixture.show_presets()
    await fixture.fill_name("real")
    await fixture.fill_delay("3")
    await fixture.click_label("Save screen preset")
    saved = {
        **initial,
        PRESETS: [{"name": "real", "delay_seconds": 3, "include_cursor": False}],
    }
    await fixture.wait_preferences(saved)
    await fixture.close_app()
    await fixture.launch()
    await fixture.show_presets()
    assert fixture.preferences() == saved, "the real capture must use the persisted preset"

    started = time.monotonic()
    await fixture.click_label("Capture: real")
    await fixture.text.wait_for_status(fixture.driver, "Capturing in 3 seconds", 25)
    assert not saved_images(fixture.captures), "the countdown must precede saving"
    Path(color_file).write_text("e02040")
    await wait_for(lambda: len(saved_images(fixture.captures)) == 1, "the real preset PNG", 30)
    elapsed = time.monotonic() - started
    assert elapsed >= 2.0, f"the three-second preset completed too early: {elapsed:.2f}s"
    image = saved_images(fixture.captures)[0]
    verify_image(image, fixture.work, (0xE0, 0x20, 0x40))
    await fixture.text.wait_for_status(fixture.driver, "Screen captured", 20)
    await wait_for(
        lambda: fixture.driver.red_canvas_pixels() > 500,
        "the captured image to appear in the editor",
        15,
    )
    clipboard = subprocess.check_output(
        ["xclip", "-selection", "clipboard", "-t", "image/png", "-o"],
        env=fixture.env, timeout=10,
    )
    clipboard_image = fixture.work / "preset-clipboard.png"
    clipboard_image.write_bytes(clipboard)
    verify_image(clipboard_image, fixture.work, (0xE0, 0x20, 0x40))
    assert fixture.preferences() == saved, "capture must not change normal or preset options"


async def test_real_saved_preset_respects_disabled_clipboard(fixture):
    color_file = os.environ.get("JIFFSHOT_DELAY_COLOR_FILE")
    if not color_file:
        raise RuntimeError("real portal mode requires JIFFSHOT_DELAY_COLOR_FILE")
    Path(color_file).write_text("e02040")
    initial = {
        **fixture.original,
        "capture_delay_seconds": 30,
        "capture_cursor": True,
        "copy_to_clipboard": False,
    }
    await fixture.launch(initial)
    fixture.set_clipboard("clipboard disabled sentinel")
    await fixture.show_presets()
    await fixture.fill_name("nocopy")
    await fixture.fill_delay("0")
    await fixture.click_label("Preset includes cursor")
    await fixture.click_label("Save screen preset")
    saved = {
        **initial,
        PRESETS: [{"name": "nocopy", "delay_seconds": 0, "include_cursor": False}],
    }
    await fixture.wait_preferences(saved)
    previous = set(saved_images(fixture.captures))
    await fixture.click_label("Capture: nocopy")
    await wait_for(
        lambda: len(set(saved_images(fixture.captures)) - previous) == 1,
        "the real preset PNG with clipboard disabled",
        30,
    )
    image = next(iter(set(saved_images(fixture.captures)) - previous))
    verify_image(image, fixture.work, (0xE0, 0x20, 0x40))
    await fixture.text.wait_for_status(fixture.driver, "Screen captured", 20)
    assert fixture.clipboard_text() == "clipboard disabled sentinel"
    assert fixture.preferences() == saved


async def run_cases(cases, fixture):
    if not cases:
        raise AssertionError("no screen preset GUI tests were selected")
    names = [case.__name__ for case in cases]
    if len(set(names)) != len(names) or any(
        not name.isascii() or not name.startswith("test_") or not name.isidentifier()
        for name in names
    ):
        raise AssertionError(f"unsafe or duplicate GUI case names: {names!r}")
    for index, case in enumerate(cases):
        try:
            await case(fixture)
        except BaseException:
            print(
                f"FAIL {case.__name__}; {len(cases) - index - 1} case(s) not run",
                file=sys.stderr,
                flush=True,
            )
            raise
        print(f"PASS {case.__name__}", flush=True)
    print(f"GUI test result: ok. {len(cases)} passed", flush=True)


async def main(binary, runtime, real_portal):
    portal = None
    if not real_portal:
        portal = TimedPortal()
        await portal.start()
    server = fixture = text = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-screen-presets-ui-") as directory:
        work = Path(directory)
        runtime_dir = work / "runtime"
        runtime_dir.mkdir(mode=0o700)
        (work / "config" / "Jiffshot").mkdir(parents=True)
        (work / "captures").mkdir()
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
                text = TextReader()
                fixture = Fixture(binary, runtime, work, portal, text, env, app_log)
                cases = (
                    [
                        test_real_saved_preset_delivers_delayed_png_and_clipboard,
                        test_real_saved_preset_respects_disabled_clipboard,
                    ]
                    if real_portal else [
                        test_screen_preset_save_replacement_and_independent_preferences,
                        test_screen_preset_invalid_input_does_not_overwrite,
                        test_screen_preset_survives_restart_and_can_be_deleted,
                        test_screen_preset_cursor_mode_and_normal_capture_are_independent,
                        test_screen_preset_chooser_cancellation_closes_session_without_saving,
                        test_screen_preset_cancel_during_chooser_is_honored_after_selection,
                        test_screen_preset_countdown_uses_saved_delay_and_can_be_cancelled,
                        test_screen_preset_invalid_stored_values_remain_unchanged,
                        test_screen_preset_removed_externally_cannot_start_capture,
                    ]
                )
                await run_cases(cases, fixture)
        except BaseException:
            if fixture is not None and fixture.driver is not None:
                trace_window(fixture.driver, "screen-presets-failure")
            for name in ("app.log", "xvfb.log"):
                path = work / name
                if path.exists():
                    print(path.read_text(errors="replace")[-12000:], file=sys.stderr)
            raise
        finally:
            if fixture is not None:
                if fixture.driver is not None:
                    fixture.driver.close()
                stop(fixture.app)
            if text is not None:
                text.close()
            stop(server)
            if read_fd is not None:
                os.close(read_fd)
            if write_fd is not None:
                os.close(write_fd)
            if portal is not None:
                portal.close()


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
