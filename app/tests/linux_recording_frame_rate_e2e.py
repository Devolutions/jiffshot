#!/usr/bin/env python3
"""Verify Linux recording-rate controls and entry points with a mock portal."""

import argparse
import asyncio
import fcntl
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile

from linux_interactive_effects_e2e import wheel
from linux_load_e2e import window_info
from linux_screen_presets_e2e import Fixture, run_cases
from linux_shortcuts_e2e import X11Driver, stop, wait_for
from linux_timed_capture_e2e import TimedPortal
from linux_webcam_e2e import TextReader, press, trace_window


RATE_KEY = "linux_recording_frame_rate"


class RecordingRateFixture(Fixture):
    async def launch(self, preferences=None, arguments=()):
        if self.app is not None:
            await self.close_app()
        if preferences is not None:
            self.settings.write_text(json.dumps(preferences))
        self.app = subprocess.Popen(
            [str(self.binary), *arguments], env=self.env, cwd=self.work,
            stdin=subprocess.DEVNULL, stdout=self.app_log, stderr=self.app_log,
        )
        await wait_for(lambda: window_info(self.env), "the editor", 15)
        window, origin = window_info(self.env)
        self.driver = X11Driver(self.env["DISPLAY"], window, origin)
        await asyncio.sleep(0.4)

    async def choose_rate(self, index):
        x, y = await self.label("Save recording frame rate")
        if y > 210:
            wheel(self.driver, "down", 3)
            await asyncio.sleep(0.25)
            x, y = await self.label("Save recording frame rate")
        trace_window(self.driver, "frame-rate-before-selection")
        # Anchor on the save button: OCR can confuse the label with button text.
        self.driver.click(x - 185, y)
        press(self.driver, "Home")
        for _ in range(index):
            press(self.driver, "Down")
        press(self.driver, "Return")
        await asyncio.sleep(0.25)
        if self.text.find_label(self.driver, "Save recording frame rate") is None:
            trace_window(self.driver, "frame-rate-selection-missing")
            observed = self.text._recognize(self.driver, 0, 0, 800, 270, 6)
            raise AssertionError(f"frame-rate controls disappeared after selection: {observed!r}")

    async def save_rate(self, expected):
        await self.click_label("Save recording frame rate")
        await self.wait_preferences(expected)
        await self.text.wait_for_status(self.driver, "Recording frame rate saved")


async def cancel_pending_gui_recording(fixture):
    session, request = fixture.portal.screen_starts[-1]
    await fixture.click_label("Record screen")
    await wait_for(lambda: request in fixture.portal.closed_requests, "the pending request to close")
    await wait_for(lambda: session in fixture.portal.closes, "the recording session to close")


async def test_frame_rate_gui_saves_every_choice_and_restores_after_restart(fixture):
    initial = {
        **fixture.original,
        "record_fps": 48,
        "other": {"keep": [1, None, "two"]},
    }
    await fixture.launch(initial)
    for index, value in enumerate(("source", "15", "24", "30", "60")):
        await fixture.choose_rate(index)
        await fixture.save_rate({**initial, RATE_KEY: value})
    trace_window(fixture.driver, "frame-rate-saved")

    await fixture.close_app()
    await fixture.launch()
    assert fixture.preferences() == {**initial, RATE_KEY: "60"}
    # Change the file after the UI loaded; Save must use the restored selection,
    # not merely leave the existing file value untouched.
    fixture.settings.write_text(json.dumps({**initial, RATE_KEY: "15"}))
    await fixture.save_rate({**initial, RATE_KEY: "60"})


async def test_frame_rate_gui_save_respects_the_shared_settings_lock(fixture):
    initial = {**fixture.original, "record_fps": 144, RATE_KEY: "15"}
    await fixture.launch(initial)
    await fixture.choose_rate(2)
    original = fixture.settings.read_bytes()
    lock_path = fixture.settings.parent / "settings.json.schedule.lock"
    with lock_path.open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        await fixture.click_label("Save recording frame rate")
        await fixture.text.wait_for_status(fixture.driver, "Could not save recording frame rate")
        assert fixture.settings.read_bytes() == original, (
            "a competing settings lock must prevent every preference write"
        )
        fcntl.flock(lock, fcntl.LOCK_UN)
    await fixture.save_rate({**initial, RATE_KEY: "24"})


async def test_frame_rate_invalid_preference_blocks_normal_gui_recording(fixture):
    initial = {**fixture.original, "record_fps": 72, RATE_KEY: "30"}
    await fixture.launch(initial)
    for invalid in ("unknown", 30, None):
        bad = {**initial, RATE_KEY: invalid}
        fixture.settings.write_text(json.dumps(bad))
        before = fixture.settings.read_bytes()
        previous = fixture.portal.capture_requests
        await fixture.click_label("Record screen")
        await fixture.text.wait_for_status(fixture.driver, "Could not read the recording frame rate")
        assert fixture.portal.capture_requests == previous, (
            f"invalid saved rate {invalid!r} must prevent a portal request"
        )
        assert fixture.settings.read_bytes() == before

    fixture.settings.write_text(json.dumps(initial))
    previous = len(fixture.portal.screen_starts)
    await fixture.click_label("Record screen")
    await wait_for(
        lambda: len(fixture.portal.screen_starts) == previous + 1,
        "normal recording with a valid saved rate",
    )
    await cancel_pending_gui_recording(fixture)
    assert fixture.preferences() == initial


async def test_frame_rate_command_line_recording_reads_saved_preference(fixture):
    invalid = {**fixture.original, "record_fps": 90, RATE_KEY: "unsupported"}
    previous = fixture.portal.capture_requests
    await fixture.launch(invalid, ("--record",))
    await fixture.text.wait_for_status(fixture.driver, "Could not read the recording frame rate")
    assert fixture.portal.capture_requests == previous
    assert fixture.preferences() == invalid

    await fixture.close_app()
    valid = {**invalid, RATE_KEY: "24"}
    previous = len(fixture.portal.screen_starts)
    await fixture.launch(valid, ("--record",))
    await wait_for(
        lambda: len(fixture.portal.screen_starts) == previous + 1,
        "command-line recording with a valid saved rate",
    )
    await cancel_pending_gui_recording(fixture)
    assert fixture.preferences() == valid


async def test_frame_rate_global_shortcut_reads_saved_preference(fixture):
    initial = {
        **fixture.original,
        "record_fps": 120,
        RATE_KEY: "invalid",
        "global_capture_shortcut": "CTRL+SHIFT+X",
        "global_recording_shortcuts": True,
        "linux_global_recording_start": "silent",
        "linux_global_shortcuts_enabled": True,
    }
    previous_bindings = len(fixture.portal.bindings)
    await fixture.launch(initial)
    await wait_for(
        lambda: len(fixture.portal.bindings) == previous_bindings + 1,
        "the restored global recording shortcut",
    )
    session, shortcuts = fixture.portal.bindings[-1]
    assert "toggle-recording" in [shortcut[0] for shortcut in shortcuts], shortcuts
    await fixture.text.wait_for_status(fixture.driver, "Global shortcuts enabled")
    saved = fixture.preferences()
    assert saved[RATE_KEY] == "invalid" and saved["record_fps"] == 120

    previous = fixture.portal.capture_requests
    fixture.portal.activate(session, "toggle-recording")
    await fixture.text.wait_for_status(fixture.driver, "Could not read the recording frame rate")
    assert fixture.portal.capture_requests == previous

    valid = {**saved, RATE_KEY: "60"}
    fixture.settings.write_text(json.dumps(valid))
    previous_starts = len(fixture.portal.screen_starts)
    fixture.portal.activate(session, "toggle-recording")
    await wait_for(
        lambda: len(fixture.portal.screen_starts) == previous_starts + 1,
        "global-shortcut recording with a valid saved rate",
    )
    recording_session, request = fixture.portal.screen_starts[-1]
    fixture.portal.activate(session, "toggle-recording")
    await wait_for(lambda: request in fixture.portal.closed_requests, "shortcut stop to close request")
    await wait_for(
        lambda: recording_session in fixture.portal.closes,
        "shortcut stop to close recording session",
    )
    assert fixture.preferences() == valid


async def finish_real_recording(fixture):
    await fixture.text.wait_for_status(fixture.driver, "Screen recording is active", 25)
    await asyncio.sleep(1.3)
    await fixture.click_label("Record screen")
    await fixture.text.wait_for_status(fixture.driver, "Screen recording saved", 30)


async def test_real_frame_rate_gui_and_command_line_recording_after_save(fixture):
    initial = {**fixture.original, "record_fps": 48, RATE_KEY: "15"}
    expected = {**initial, RATE_KEY: "30"}
    await fixture.launch(initial)
    await fixture.choose_rate(3)
    await fixture.save_rate(expected)
    await fixture.click_label("Record screen")
    await finish_real_recording(fixture)
    assert len(list(fixture.captures.glob("Recording-*.mp4"))) == 1
    await fixture.close_app()

    await fixture.launch(arguments=("--record",))
    await finish_real_recording(fixture)
    recordings = list(fixture.captures.glob("Recording-*.mp4"))
    assert len(recordings) == 2, f"expected two completed real recordings, got {recordings}"
    assert all(path.stat().st_size > 1000 for path in recordings), (
        "both real recordings must contain media"
    )
    assert fixture.preferences() == expected


async def main(binary, runtime, selected_case, real_portal, capture_dir):
    portal = None
    if not real_portal:
        portal = TimedPortal()
        portal.hold_start = True
        await portal.start()
    server = fixture = text = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-recording-rate-ui-") as directory:
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
                    "XDG_RUNTIME_DIR": (
                        os.environ["XDG_RUNTIME_DIR"] if real_portal else str(runtime_dir)
                    ),
                    "XDG_SESSION_TYPE": "x11",
                    "RUSTOLONIA_HOST_LIB": str(runtime / "librustolonia_host.so"),
                    "LD_LIBRARY_PATH": os.pathsep.join(
                        filter(None, (str(runtime), os.environ.get("LD_LIBRARY_PATH")))
                    ),
                }
                env.pop("WAYLAND_DISPLAY", None)
                text = TextReader()
                fixture = RecordingRateFixture(binary, runtime, work, portal, text, env, app_log)
                if real_portal:
                    if capture_dir is None:
                        raise ValueError("--real-portal requires --capture-dir")
                    fixture.captures = capture_dir
                    fixture.original["capture_directory"] = str(capture_dir)
                    cases = [test_real_frame_rate_gui_and_command_line_recording_after_save]
                else:
                    cases = [
                        test_frame_rate_gui_saves_every_choice_and_restores_after_restart,
                        test_frame_rate_gui_save_respects_the_shared_settings_lock,
                        test_frame_rate_invalid_preference_blocks_normal_gui_recording,
                        test_frame_rate_command_line_recording_reads_saved_preference,
                        test_frame_rate_global_shortcut_reads_saved_preference,
                    ]
                if selected_case:
                    cases = [case for case in cases if case.__name__ == selected_case]
                await run_cases(cases, fixture)
        except BaseException:
            if fixture is not None and fixture.driver is not None:
                trace_window(fixture.driver, "frame-rate-failure")
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
    parser.add_argument("--case", help="run one exact GUI test case")
    parser.add_argument("--real-portal", action="store_true")
    parser.add_argument("--capture-dir", type=Path)
    arguments = parser.parse_args()
    asyncio.run(main(
        arguments.binary.resolve(strict=True), arguments.runtime_dir.resolve(strict=True),
        arguments.case, arguments.real_portal,
        arguments.capture_dir.resolve(strict=True) if arguments.capture_dir else None,
    ))
