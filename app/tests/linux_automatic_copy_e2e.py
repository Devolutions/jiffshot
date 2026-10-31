#!/usr/bin/env python3
"""Verify automatic capture copies through the Linux editor and desktop portals."""

import argparse
import asyncio
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile

from dbus_next import Message, Variant
from linux_interactive_effects_e2e import EffectsPortal, count_canvas_color, decode_png, wheel
from linux_load_e2e import draw_arrow, solid_png, window_info
from linux_shortcuts_e2e import REQUEST, X11Driver, stop, wait_for
from linux_webcam_e2e import TextReader, scroll, trace_window


SETTING = "linux_auto_copy_directory"
PICKER_TITLE = "Choose folder for automatic capture copies"
FROZEN_SCREENSHOT = "Screenshot-1700000000-123456789.png"


def screenshot_files(directory):
    return set(directory.glob("Screenshot-*.png"))


def png_complete(path):
    try:
        data = path.read_bytes()
    except FileNotFoundError:
        return False
    return data.startswith(b"\x89PNG\r\n\x1a\n") and data.endswith(b"IEND\xaeB`\x82")


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
        self.source = work / "desktop.png"
        self.source.write_bytes(solid_png(255, 0, 0))
        self.editor_source = work / "editor.png"
        self.editor_source.write_bytes(solid_png(30, 120, 190))
        self.original = {
            "theme": "Light",
            "capture_directory": str(self.captures),
            "copy_to_clipboard": False,
            "other": {"keep": ["unrelated", 7]},
        }

    def preferences(self):
        return json.loads(self.settings.read_text())

    def set_preferences(self, directory=None):
        value = dict(self.original)
        if directory is not None:
            value[SETTING] = str(directory)
        self.settings.write_text(json.dumps(value))

    async def close_app(self):
        if self.driver is not None:
            self.driver.close()
            self.driver = None
        stop(self.app)
        self.app = None
        await wait_for(lambda: window_info(self.env) is None, "the previous editor to close")

    async def launch(self, *arguments, extra_env=None):
        if self.app is not None:
            await self.close_app()
        environment = {**self.env, **(extra_env or {})}
        self.app = subprocess.Popen(
            [str(self.binary), *map(str, arguments)], env=environment, cwd=self.work,
            stdin=subprocess.DEVNULL, stdout=self.app_log, stderr=self.app_log,
        )
        await wait_for(lambda: window_info(self.env), "the editor", 15)
        window, origin = window_info(self.env)
        self.driver = X11Driver(self.env["DISPLAY"], window, origin)
        await asyncio.sleep(0.4)

    async def find_and_click(self, label, direction):
        for _ in range(14):
            position = self.text.find_label(self.driver, label)
            if position is not None:
                self.driver.click(*position)
                return
            wheel(self.driver, direction, 2)
            await asyncio.sleep(0.15)
        trace_window(self.driver, "automatic-copy-label-not-found")
        observed = self.text._recognize(self.driver, 0, 0, 800, 270, 6)
        raise AssertionError(f"could not find {label!r} in the controls; OCR saw {observed!r}")

    async def show_organization(self):
        scroll(self.driver, "down")
        await asyncio.sleep(0.4)
        await self.find_and_click("Organize captures", "down")
        await asyncio.sleep(0.3)
        scroll(self.driver, "down")
        await asyncio.sleep(0.3)
        trace_window(self.driver, "automatic-copy-controls")

    async def begin_folder_picker(self, fail_method=False):
        previous = len(self.portal.open_requests)
        self.portal.fail_next_open = fail_method
        await self.find_and_click("Auto-copy new captures", "down")
        await wait_for(
            lambda: len(self.portal.open_requests) == previous + 1,
            "the automatic-copy folder chooser",
        )
        title, options, _request = self.portal.open_requests[previous]
        assert title == PICKER_TITLE, title
        assert options["directory"].value is True, "the chooser must select a directory"
        assert int(self.portal.open_parents[previous].removeprefix("x11:"), 16) == (
            self.driver.window
        ), "the folder chooser must be parented to the editor"
        await self.text.wait_for_status(self.driver, "Choose a folder for automatic")
        return previous

    async def choose_folder(self, directory):
        request = await self.begin_folder_picker()
        self.portal.finish_open(request, 0, directory.as_uri())
        await wait_for(
            lambda: self.preferences().get(SETTING) == str(directory),
            "the selected automatic-copy preference",
        )

    async def begin_screenshot(self):
        previous = len(self.portal.screenshot_requests)
        scroll(self.driver, "down")
        await asyncio.sleep(0.3)
        await self.find_and_click("Desktop screenshot chooser", "up")
        await wait_for(
            lambda: len(self.portal.screenshot_requests) == previous + 1,
            "the desktop screenshot chooser",
        )
        request, options = self.portal.screenshot_requests[previous]
        assert options["interactive"].value is True and options["modal"].value is True
        await self.text.wait_for_status(self.driver, "Choose a screenshot")
        return request

    async def launch_screenshot(self, extra_env=None):
        previous = len(self.portal.screenshot_requests)
        await self.launch("--capture-interactive", extra_env=extra_env)
        await wait_for(
            lambda: len(self.portal.screenshot_requests) == previous + 1,
            "the launched screenshot chooser",
        )
        request, options = self.portal.screenshot_requests[previous]
        assert options["interactive"].value is True and options["modal"].value is True
        return request

    def finish_screenshot(self, request, code=0, uri=None):
        result = {} if code else {"uri": Variant("s", uri or self.source.as_uri())}
        asyncio.get_running_loop().call_later(0.1, self.portal.response, request, code, result)

    async def saved_screenshot(self, previous):
        def ready():
            added = screenshot_files(self.captures) - previous
            return len(added) == 1 and png_complete(next(iter(added)))

        await wait_for(ready, "a completed original screenshot", 15)
        added = screenshot_files(self.captures) - previous
        assert len(added) == 1, f"expected one new original, got {added}"
        path = next(iter(added))
        assert decode_png(path).pixel(150, 100) == (255, 0, 0, 255), (
            "the saved capture must contain the selected red screenshot"
        )
        return path

    async def copied_screenshot(self, original, directory, previous):
        def ready():
            added = set(directory.glob("*.png")) - previous
            return len(added) == 1 and next(iter(added)).read_bytes() == original.read_bytes()

        await wait_for(ready, f"one byte-identical copy in {directory}", 15)
        added = set(directory.glob("*.png")) - previous
        assert len(added) == 1, f"expected one new copy, got {added}"
        copied = next(iter(added))
        assert copied.read_bytes() == original.read_bytes(), "copy bytes must equal saved bytes"
        return copied

    async def capture(self, directory=None):
        before_source = screenshot_files(self.captures)
        before_copy = set(directory.glob("*.png")) if directory is not None else set()
        request = await self.begin_screenshot()
        self.finish_screenshot(request)
        original = await self.saved_screenshot(before_source)
        copied = (
            await self.copied_screenshot(original, directory, before_copy)
            if directory is not None else None
        )
        await self.text.wait_for_status(self.driver, "Desktop screenshot captured")
        return original, copied


async def test_default_disabled_does_not_copy(fixture, destination):
    fixture.set_preferences()
    before = screenshot_files(fixture.captures)
    request = await fixture.launch_screenshot()
    fixture.finish_screenshot(request)
    await fixture.saved_screenshot(before)
    await fixture.text.wait_for_status(fixture.driver, "Desktop screenshot captured")
    assert SETTING not in fixture.preferences(), "automatic copying must be absent by default"
    assert not list(destination.iterdir()), "a default-disabled capture must not create copies"


async def test_folder_selection_persists_unrelated_preferences(fixture, destination):
    fixture.set_preferences()
    await fixture.launch(str(fixture.editor_source))
    await fixture.show_organization()
    await fixture.choose_folder(destination)
    assert fixture.preferences() == {**fixture.original, SETTING: str(destination)}
    assert destination.is_absolute(), "the chosen local folder must be an absolute path"


async def test_screenshot_copy_is_byte_identical(fixture, destination):
    original, copied = await fixture.capture(destination)
    assert copied.name == original.name, "without a collision, preserve the capture filename"


async def test_live_destination_change_uses_new_folder(fixture, first, second):
    old_files = {path.name: path.read_bytes() for path in first.iterdir()}
    await fixture.choose_folder(second)
    assert fixture.preferences() == {**fixture.original, SETTING: str(second)}
    original, copied = await fixture.capture(second)
    assert copied.read_bytes() == original.read_bytes()
    assert {path.name: path.read_bytes() for path in first.iterdir()} == old_files, (
        "changing destinations must leave the previous folder unchanged"
    )


async def test_persistence_and_disable_apply_to_later_captures(fixture, first, second):
    before_originals = screenshot_files(fixture.captures)
    before_second = set(second.glob("*.png"))
    request = await fixture.launch_screenshot()
    fixture.finish_screenshot(request)
    original = await fixture.saved_screenshot(before_originals)
    await fixture.copied_screenshot(original, second, before_second)
    await fixture.text.wait_for_status(fixture.driver, "Desktop screenshot captured")
    assert fixture.preferences() == {**fixture.original, SETTING: str(second)}

    await fixture.show_organization()
    await fixture.find_and_click("Disable auto-copy", "down")
    await wait_for(lambda: SETTING not in fixture.preferences(), "disabling auto-copy")
    assert fixture.preferences() == fixture.original
    snapshots = [{path.name: path.read_bytes() for path in folder.iterdir()} for folder in (first, second)]
    await fixture.capture()
    assert snapshots == [
        {path.name: path.read_bytes() for path in folder.iterdir()} for folder in (first, second)
    ], "disabling must immediately stop copying"

    before_originals = screenshot_files(fixture.captures)
    request = await fixture.launch_screenshot()
    fixture.finish_screenshot(request)
    await fixture.saved_screenshot(before_originals)
    await fixture.text.wait_for_status(fixture.driver, "Desktop screenshot captured")
    assert fixture.preferences() == fixture.original
    assert snapshots == [
        {path.name: path.read_bytes() for path in folder.iterdir()} for folder in (first, second)
    ], "the disabled preference must survive a restart"


async def test_folder_chooser_cancellation_and_errors_preserve_setting(fixture, first):
    fixture.set_preferences(first)
    await fixture.launch(str(fixture.editor_source))
    await fixture.show_organization()
    expected = {**fixture.original, SETTING: str(first)}

    request = await fixture.begin_folder_picker()
    fixture.portal.finish_open(request, 1)
    await fixture.text.wait_for_status(fixture.driver, "cancelled")
    assert fixture.preferences() == expected, "cancellation must preserve all preferences"

    request = await fixture.begin_folder_picker()
    fixture.portal.finish_open(request, 2)
    await fixture.text.wait_for_status(fixture.driver, "Could not set")
    assert fixture.preferences() == expected, "a failed response must preserve preferences"

    request = await fixture.begin_folder_picker(fail_method=True)
    fixture.portal.fail_open(request)
    await fixture.text.wait_for_status(fixture.driver, "Could not set")
    assert fixture.preferences() == expected, "a chooser method error must preserve preferences"

    request = await fixture.begin_folder_picker()
    fixture.portal.finish_open(request, 0, "https://example.invalid/not-a-local-folder")
    await fixture.text.wait_for_status(fixture.driver, "Could not set")
    assert fixture.preferences() == expected, "a non-local URI must preserve preferences"


async def test_stale_folder_chooser_response_cannot_reenable_after_disable(fixture, first, second):
    fixture.set_preferences(first)
    await fixture.launch(str(fixture.editor_source))
    await fixture.show_organization()
    request = await fixture.begin_folder_picker()
    await fixture.find_and_click("Disable auto-copy", "down")
    await wait_for(lambda: SETTING not in fixture.preferences(), "disabling an open chooser")
    await fixture.text.wait_for_status(fixture.driver, "disabled")

    before_first = {path.name: path.read_bytes() for path in first.iterdir()}
    before_second = {path.name: path.read_bytes() for path in second.iterdir()}
    response_path = fixture.portal.open_requests[request][2]
    await fixture.portal.bus.send(Message.new_signal(
        response_path, REQUEST, "Response", "ua{sv}",
        [0, {"uris": Variant("as", [second.as_uri()])}],
    ))
    # Completing a later capture gives the application an observable progress
    # barrier after the stale response has been sent.
    await fixture.capture()
    assert fixture.preferences() == fixture.original, "a stale choice must not re-enable copying"
    assert {path.name: path.read_bytes() for path in first.iterdir()} == before_first
    assert {path.name: path.read_bytes() for path in second.iterdir()} == before_second


async def test_cancelled_and_failed_screenshots_do_not_copy(fixture, destination):
    fixture.set_preferences(destination)
    await fixture.launch(str(fixture.editor_source))
    before_source = screenshot_files(fixture.captures)
    before_copy = {path.name: path.read_bytes() for path in destination.iterdir()}
    for code, expected in ((1, "cancelled"), (2, "failed")):
        request = await fixture.begin_screenshot()
        fixture.finish_screenshot(request, code)
        await fixture.text.wait_for_status(fixture.driver, expected)
        assert screenshot_files(fixture.captures) == before_source, (
            f"portal response {code} must not save a capture"
        )
        assert {path.name: path.read_bytes() for path in destination.iterdir()} == before_copy, (
            f"portal response {code} must not create a copy"
        )
    request = await fixture.begin_screenshot()
    fixture.finish_screenshot(request, uri="https://example.invalid/screenshot.png")
    await fixture.text.wait_for_status(fixture.driver, "non-local")
    assert screenshot_files(fixture.captures) == before_source
    assert {path.name: path.read_bytes() for path in destination.iterdir()} == before_copy


async def test_failed_screenshot_save_does_not_copy(fixture, destination):
    unavailable_capture_directory = fixture.work / "not-a-capture-directory"
    unavailable_capture_directory.write_bytes(b"keep this file")
    settings = {
        **fixture.original,
        "capture_directory": str(unavailable_capture_directory),
        SETTING: str(destination),
    }
    fixture.settings.write_text(json.dumps(settings))
    before_copy = {path.name: path.read_bytes() for path in destination.iterdir()}
    request = await fixture.launch_screenshot()
    fixture.finish_screenshot(request)
    await fixture.text.wait_for_status(fixture.driver, "could not save")
    assert unavailable_capture_directory.read_bytes() == b"keep this file"
    assert fixture.preferences() == settings
    assert {path.name: path.read_bytes() for path in destination.iterdir()} == before_copy, (
        "a screenshot which could not be saved must not be copied"
    )


async def test_unsaved_editor_changes_do_not_copy(fixture, destination):
    fixture.set_preferences(destination)
    await fixture.launch(str(fixture.editor_source))
    before_source = screenshot_files(fixture.captures)
    before_copy = {path.name: path.read_bytes() for path in destination.iterdir()}
    await wait_for(
        lambda: count_canvas_color(fixture.driver, (30, 120, 190)) > 5000,
        "the blue editor document",
    )
    red_before = fixture.driver.red_canvas_pixels()
    draw_arrow(fixture.driver)
    await wait_for(
        lambda: fixture.driver.red_canvas_pixels() > red_before + 40,
        "an unsaved red annotation",
    )
    await fixture.text.wait_for_status(fixture.driver, "Document changed")
    assert screenshot_files(fixture.captures) == before_source
    assert {path.name: path.read_bytes() for path in destination.iterdir()} == before_copy, (
        "editing a document must not copy an unsaved change"
    )
    saved, copied = await fixture.capture(destination)
    added = set(destination.iterdir()) - {destination / name for name in before_copy}
    assert added == {copied} and copied.read_bytes() == saved.read_bytes(), (
        "only the subsequently saved screenshot may be copied"
    )


async def test_collision_never_overwrites_existing_files(fixture, destination, clock_library):
    fixture.set_preferences(destination)
    sentinel = destination / FROZEN_SCREENSHOT
    second = destination / f"{sentinel.stem} (2){sentinel.suffix}"
    sentinel.write_bytes(b"first existing file")
    second.write_bytes(b"second existing file")
    before_originals = screenshot_files(fixture.captures)
    before_copies = set(destination.glob("*.png"))
    request = await fixture.launch_screenshot(extra_env={"LD_PRELOAD": str(clock_library)})
    fixture.finish_screenshot(request)
    original = await fixture.saved_screenshot(before_originals)
    assert original.name == FROZEN_SCREENSHOT, (
        "the test clock must make the pre-existing names actual collisions", original.name
    )
    copied = await fixture.copied_screenshot(original, destination, before_copies)
    await fixture.text.wait_for_status(fixture.driver, "Desktop screenshot captured")
    assert copied.name not in {sentinel.name, second.name}
    assert sentinel.read_bytes() == b"first existing file"
    assert second.read_bytes() == b"second existing file"


async def test_missing_folder_reports_error_and_preserves_original(fixture):
    destination = fixture.work / "removed-destination"
    destination.mkdir()
    fixture.set_preferences(destination)
    destination.rmdir()
    before = screenshot_files(fixture.captures)
    request = await fixture.launch_screenshot()
    fixture.finish_screenshot(request)
    original = await fixture.saved_screenshot(before)
    await fixture.text.wait_for_status(fixture.driver, "automatic copy failed")
    assert png_complete(original), "an automatic-copy error must not delete the original"
    assert not destination.exists(), "a missing destination must not be silently recreated"


async def test_capture_folder_destination_reports_error_without_duplicate(fixture):
    fixture.set_preferences(fixture.captures)
    before = screenshot_files(fixture.captures)
    request = await fixture.launch_screenshot()
    fixture.finish_screenshot(request)
    original = await fixture.saved_screenshot(before)
    await fixture.text.wait_for_status(fixture.driver, "automatic copy failed")
    assert png_complete(original)
    assert screenshot_files(fixture.captures) - before == {original}, (
        "using the capture folder as destination must not create a duplicate"
    )


async def test_invalid_auto_copy_setting_reports_error_and_preserves_original(fixture, destination):
    fixture.settings.write_text(json.dumps({**fixture.original, SETTING: 42}))
    expected = fixture.preferences()
    before_source = screenshot_files(fixture.captures)
    before_copy = {path.name: path.read_bytes() for path in destination.iterdir()}
    request = await fixture.launch_screenshot()
    fixture.finish_screenshot(request)
    original = await fixture.saved_screenshot(before_source)
    await fixture.text.wait_for_status(fixture.driver, "automatic copy failed")
    assert png_complete(original), "an invalid preference must not delete the original"
    assert fixture.preferences() == expected, "capturing must not silently rewrite invalid settings"
    assert {path.name: path.read_bytes() for path in destination.iterdir()} == before_copy


async def test_capture_folder_alias_reports_error_without_duplicate(fixture):
    alias = fixture.work / "captures-alias"
    alias.symlink_to(fixture.captures, target_is_directory=True)
    fixture.set_preferences(alias)
    before = screenshot_files(fixture.captures)
    request = await fixture.launch_screenshot()
    fixture.finish_screenshot(request)
    original = await fixture.saved_screenshot(before)
    await fixture.text.wait_for_status(fixture.driver, "automatic copy failed")
    assert png_complete(original)
    assert screenshot_files(fixture.captures) - before == {original}, (
        "an alias of the capture folder must not create a duplicate"
    )


async def run_case(function, *arguments):
    await function(*arguments)
    print(f"PASS {function.__name__}", flush=True)


async def main(binary, runtime):
    portal = EffectsPortal()
    portal.screenshot_mode = "hold"
    await portal.start()
    server = fixture = text = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-auto-copy-ui-") as directory:
        work = Path(directory)
        runtime_dir = work / "runtime"
        runtime_dir.mkdir(mode=0o700)
        (work / "config" / "Jiffshot").mkdir(parents=True)
        (work / "captures").mkdir()
        destinations = [work / name for name in ("first", "second", "collision")]
        for destination in destinations:
            destination.mkdir()
        clock_library = work / "frozen-realtime.so"
        subprocess.run(
            ["cc", "-shared", "-fPIC", "-Wall", "-Wextra", "-Werror",
             str(Path(__file__).with_name("linux_frozen_realtime.c")), "-o", str(clock_library)],
            check=True,
        )
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
                text = TextReader()
                fixture = Fixture(binary, runtime, work, portal, text, env, app_log)
                first, second, collision = destinations
                await run_case(test_default_disabled_does_not_copy, fixture, first)
                await run_case(test_folder_selection_persists_unrelated_preferences, fixture, first)
                await run_case(test_screenshot_copy_is_byte_identical, fixture, first)
                await run_case(test_live_destination_change_uses_new_folder, fixture, first, second)
                await run_case(test_persistence_and_disable_apply_to_later_captures, fixture, first, second)
                await run_case(test_folder_chooser_cancellation_and_errors_preserve_setting, fixture, first)
                await run_case(
                    test_stale_folder_chooser_response_cannot_reenable_after_disable,
                    fixture, first, second,
                )
                await run_case(test_cancelled_and_failed_screenshots_do_not_copy, fixture, first)
                await run_case(test_failed_screenshot_save_does_not_copy, fixture, first)
                await run_case(test_unsaved_editor_changes_do_not_copy, fixture, first)
                await run_case(test_collision_never_overwrites_existing_files, fixture, collision, clock_library)
                await run_case(test_missing_folder_reports_error_and_preserves_original, fixture)
                await run_case(test_capture_folder_destination_reports_error_without_duplicate, fixture)
                await run_case(test_invalid_auto_copy_setting_reports_error_and_preserves_original, fixture, first)
                await run_case(test_capture_folder_alias_reports_error_without_duplicate, fixture)
        except BaseException:
            if fixture is not None and fixture.driver is not None:
                trace_window(fixture.driver, "automatic-copy-failure")
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
            portal.bus.disconnect()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--runtime-dir", type=Path, required=True)
    arguments = parser.parse_args()
    asyncio.run(main(
        arguments.binary.resolve(strict=True),
        arguments.runtime_dir.resolve(strict=True),
    ))
