#!/usr/bin/env python3
"""Exercise global shortcut controls with a private Xvfb and mock portal."""

import argparse
import asyncio
import ctypes
import ctypes.util
import json
import os
from pathlib import Path
import re
import select
import struct
import subprocess
import sys
import tempfile
import time
import zipfile
import zlib

from dbus_next import Message, MessageType, Variant
from dbus_next.aio import MessageBus


DESKTOP = "/org/freedesktop/portal/desktop"
SHORTCUTS = "org.freedesktop.portal.GlobalShortcuts"
SESSION = "org.freedesktop.portal.Session"
REQUEST = "org.freedesktop.portal.Request"
SCREENCAST = "org.freedesktop.portal.ScreenCast"
SCREENSHOT = "org.freedesktop.portal.Screenshot"


class XImage(ctypes.Structure):
    _fields_ = [
        ("width", ctypes.c_int), ("height", ctypes.c_int),
        ("xoffset", ctypes.c_int), ("format", ctypes.c_int),
        ("data", ctypes.c_void_p), ("byte_order", ctypes.c_int),
        ("bitmap_unit", ctypes.c_int), ("bitmap_bit_order", ctypes.c_int),
        ("bitmap_pad", ctypes.c_int), ("depth", ctypes.c_int),
        ("bytes_per_line", ctypes.c_int), ("bits_per_pixel", ctypes.c_int),
        ("red_mask", ctypes.c_ulong), ("green_mask", ctypes.c_ulong),
        ("blue_mask", ctypes.c_ulong),
    ]


def selection_document(path):
    width, height = 240, 140

    def chunk(kind, data):
        return (
            struct.pack(">I", len(data)) + kind + data
            + struct.pack(">I", zlib.crc32(kind + data))
        )

    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress((b"\0" + b"\xff\xff\xff" * width) * height))
        + chunk(b"IEND", b"")
    )
    objects = []
    for identifier, left in enumerate((15, 130), 1):
        objects.append({
            "id": identifier,
            "kind": {
                "type": "Shape",
                "rect": {"x0": left, "y0": 15, "x1": left + 70, "y1": 110},
                "shape": "Rectangle",
            },
            "style": {},
        })
    manifest = {
        "format": "jiffshot-document", "version": 1,
        "width": width, "height": height, "objects": objects,
    }
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("document.json", json.dumps(manifest))
        archive.writestr("background.png", png)


class MockPortal:
    def __init__(self):
        self.bus = None
        self.sessions = []
        self.bindings = []
        self.closes = []
        self.capture_requests = 0
        self.mode = "success"
        self.screen_mode = "reject"
        self.screen_sessions = []
        self.screen_creates = []
        self.screen_selects = []
        self.screen_selection_options = []
        self.screen_starts = []
        self.closed_requests = []
        self.screenshot_mode = "success"
        self.screenshot_uri = None
        self.screenshot_requests = []

    async def start(self):
        self.bus = await MessageBus().connect()
        await self.bus.request_name("org.freedesktop.portal.Desktop")
        self.bus.add_message_handler(self.message)

    def response(self, path, code, result):
        self.bus.send(Message.new_signal(path, REQUEST, "Response", "ua{sv}", [code, result]))

    def message(self, message):
        if message.message_type != MessageType.METHOD_CALL:
            return None
        if message.interface == "org.freedesktop.DBus.Properties":
            if message.member == "Get":
                version = 2 if message.body[0] == SCREENSHOT else 1
                return Message.new_method_return(message, "v", [Variant("u", version)])
            if message.member == "GetAll":
                return Message.new_method_return(message, "a{sv}", [{"version": Variant("u", 1)}])
        if message.interface == SHORTCUTS and message.member == "CreateSession":
            options = message.body[0]
            sender = message.sender.lstrip(":").replace(".", "_")
            session = f"{DESKTOP}/session/{sender}/{options['session_handle_token'].value}"
            request = f"{DESKTOP}/request/{sender}/{options['handle_token'].value}"
            self.sessions.append(session)
            asyncio.get_running_loop().call_later(
                0.1, self.response, request, 0, {"session_handle": Variant("o", session)}
            )
            return Message.new_method_return(message, "o", [request])
        if message.interface == SHORTCUTS and message.member == "BindShortcuts":
            session, shortcuts, _parent, options = message.body
            sender = message.sender.lstrip(":").replace(".", "_")
            request = f"{DESKTOP}/request/{sender}/{options['handle_token'].value}"
            self.bindings.append((session, shortcuts))
            if self.mode != "hold":
                names = {
                    "capture": "Ctrl+Alt+P",
                    "toggle-recording": "Shift+F8",
                    "pause-recording": "Shift+F9",
                    "stop-recording": "Shift+F10",
                }
                bound = [
                    [shortcut_id, {
                        "description": properties["description"],
                        "trigger_description": Variant("s", names[shortcut_id]),
                    }]
                    for shortcut_id, properties in shortcuts
                ]
                if self.mode == "partial":
                    bound.pop()
                result = {
                    "shortcuts": Variant("a(sa{sv})", bound)
                }
                code = 1 if self.mode == "reject" else 0
                asyncio.get_running_loop().call_later(0.1, self.response, request, code, result)
            return Message.new_method_return(message, "o", [request])
        if message.interface == SESSION and message.member == "Close":
            self.closes.append(message.path)
            return Message.new_method_return(message)
        if message.interface == REQUEST and message.member == "Close":
            self.closed_requests.append(message.path)
            return Message.new_method_return(message)
        if message.interface == SCREENSHOT and message.member == "Screenshot":
            _parent, options = message.body
            sender = message.sender.lstrip(":").replace(".", "_")
            request = f"{DESKTOP}/request/{sender}/{options['handle_token'].value}"
            self.screenshot_requests.append((request, options))
            if self.screenshot_mode != "hold":
                code = 1 if self.screenshot_mode == "cancel" else 0
                result = {} if code else {"uri": Variant("s", self.screenshot_uri)}
                asyncio.get_running_loop().call_later(0.1, self.response, request, code, result)
            return Message.new_method_return(message, "o", [request])
        if message.interface == SCREENCAST and message.member == "CreateSession":
            self.capture_requests += 1
            if self.screen_mode == "reject":
                return Message.new_error(
                    message, "org.freedesktop.DBus.Error.NotSupported", "Mock capture is unavailable"
                )
            options = message.body[0]
            sender = message.sender.lstrip(":").replace(".", "_")
            session = f"{DESKTOP}/session/{sender}/{options['session_handle_token'].value}"
            request = f"{DESKTOP}/request/{sender}/{options['handle_token'].value}"
            self.screen_sessions.append(session)
            self.screen_creates.append((session, request))
            if self.screen_mode != "hold-create":
                asyncio.get_running_loop().call_later(
                    0.1, self.response, request, 0, {"session_handle": Variant("s", session)}
                )
            return Message.new_method_return(message, "o", [request])
        if message.interface == SCREENCAST and message.member == "SelectSources":
            session, options = message.body
            sender = message.sender.lstrip(":").replace(".", "_")
            request = f"{DESKTOP}/request/{sender}/{options['handle_token'].value}"
            self.screen_selects.append((session, request))
            self.screen_selection_options.append(options)
            if self.screen_mode != "hold-select":
                asyncio.get_running_loop().call_later(0.1, self.response, request, 0, {})
            return Message.new_method_return(message, "o", [request])
        if message.interface == SCREENCAST and message.member == "Start":
            session, _parent, options = message.body
            sender = message.sender.lstrip(":").replace(".", "_")
            request = f"{DESKTOP}/request/{sender}/{options['handle_token'].value}"
            self.screen_starts.append((session, request))
            return Message.new_method_return(message, "o", [request])
        return None

    def activate(self, session, shortcut):
        self.bus.send(Message.new_signal(
            DESKTOP, SHORTCUTS, "Activated", "osta{sv}", [session, shortcut, 0, {}]
        ))

    def close_from_desktop(self, session):
        self.bus.send(Message.new_signal(session, SESSION, "Closed", "a{sv}", [{}]))


class X11Driver:
    def __init__(self, display_name, window, origin):
        self.x11 = ctypes.CDLL(ctypes.util.find_library("X11"))
        self.xtst = ctypes.CDLL(ctypes.util.find_library("Xtst"))
        self.x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.x11.XOpenDisplay.restype = ctypes.c_void_p
        self.x11.XDefaultScreen.argtypes = [ctypes.c_void_p]
        self.x11.XDefaultScreen.restype = ctypes.c_int
        self.x11.XSetInputFocus.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
        self.x11.XResizeWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_uint, ctypes.c_uint]
        self.x11.XStringToKeysym.argtypes = [ctypes.c_char_p]
        self.x11.XStringToKeysym.restype = ctypes.c_ulong
        self.x11.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        self.x11.XKeysymToKeycode.restype = ctypes.c_ubyte
        self.x11.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
        self.x11.XGetImage.argtypes = [
            ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
            ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_int,
        ]
        self.x11.XGetImage.restype = ctypes.POINTER(XImage)
        self.x11.XDestroyImage.argtypes = [ctypes.POINTER(XImage)]
        self.xtst.XTestFakeMotionEvent.argtypes = [
            ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_ulong
        ]
        self.xtst.XTestFakeButtonEvent.argtypes = [
            ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong
        ]
        self.xtst.XTestFakeKeyEvent.argtypes = [
            ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong
        ]
        self.display = self.x11.XOpenDisplay(display_name.encode())
        if not self.display:
            raise RuntimeError(f"could not open private display {display_name}")
        self.screen = self.x11.XDefaultScreen(self.display)
        self.window = window
        self.origin = origin
        self.x11.XSetInputFocus(self.display, window, 2, 0)
        self.x11.XResizeWindow(self.display, window, 800, 500)
        self.x11.XSync(self.display, 0)

    def click(self, x, y):
        self.xtst.XTestFakeMotionEvent(
            self.display, self.screen, x + self.origin[0], y + self.origin[1], 0
        )
        self.xtst.XTestFakeButtonEvent(self.display, 1, 1, 0)
        self.xtst.XTestFakeButtonEvent(self.display, 1, 0, 0)
        self.x11.XSync(self.display, 0)

    def key(self, name, down):
        keysym = self.x11.XStringToKeysym(name.encode())
        code = self.x11.XKeysymToKeycode(self.display, keysym)
        if not code:
            raise RuntimeError(f"no X11 keycode for {name!r}")
        self.xtst.XTestFakeKeyEvent(self.display, code, int(down), 0)

    def replace_shortcut(self, text):
        self.click(255, 80)
        self.key("Control_L", True)
        self.key("a", True)
        self.key("a", False)
        self.key("Control_L", False)
        self.key("Delete", True)
        self.key("Delete", False)
        for character in text:
            shifted = character.isupper() or character == "+"
            name = "equal" if character == "+" else character.lower()
            if shifted:
                self.key("Shift_L", True)
            self.key(name, True)
            self.key(name, False)
            if shifted:
                self.key("Shift_L", False)
        self.x11.XSync(self.display, 0)

    def select_all(self):
        self.key("Control_L", True)
        self.key("a", True)
        self.key("a", False)
        self.key("Control_L", False)
        self.x11.XSync(self.display, 0)

    def red_canvas_pixels(self):
        image = self.x11.XGetImage(
            self.display, self.window, 0, 280, 800, 190, ctypes.c_ulong(-1).value, 2,
        )
        if not image:
            raise RuntimeError("could not read the editor canvas")
        try:
            frame = image.contents
            if (frame.bits_per_pixel, frame.byte_order, frame.red_mask,
                    frame.green_mask, frame.blue_mask) != (32, 0, 0xFF0000, 0xFF00, 0xFF):
                raise RuntimeError("unexpected X11 pixel format")
            data = ctypes.string_at(frame.data, frame.bytes_per_line * frame.height)
            return sum(
                data[y * frame.bytes_per_line + x * 4 + 2] > 170
                and data[y * frame.bytes_per_line + x * 4 + 1] < 100
                and data[y * frame.bytes_per_line + x * 4] < 100
                for y in range(frame.height) for x in range(frame.width)
            )
        finally:
            self.x11.XDestroyImage(image)

    def close(self):
        self.x11.XCloseDisplay(self.display)


async def wait_for(predicate, description, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        await asyncio.sleep(0.05)
    raise AssertionError(f"timed out waiting for {description}")


def stop(process):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


async def test_global_recording_start_routes_selected_audio_and_stops(
    portal, session, driver, reader, captures
):
    previous_requests = portal.capture_requests
    portal.activate(session + "_foreign", "toggle-recording")
    await asyncio.sleep(0.4)
    assert portal.capture_requests == previous_requests, (
        "a foreign shortcut session must not start recording"
    )
    previous_starts = len(portal.screen_starts)
    portal.activate(session, "toggle-recording")
    await wait_for(
        lambda: len(portal.screen_starts) == previous_starts + 1,
        "the global recording start to request a source",
    )
    await reader.wait_for_status(
        driver, "Requesting permission for screen and microphone recording"
    )
    recording_session, recording_request = portal.screen_starts[-1]
    portal.activate(session, "toggle-recording")
    await wait_for(
        lambda: recording_request in portal.closed_requests,
        "the recording shortcut to stop a pending recording",
    )
    await wait_for(
        lambda: recording_session in portal.closes,
        "the stopped recording session to close",
    )
    await reader.wait_for_status(driver, "Screen and microphone recording cancelled")
    assert not list(captures.iterdir()), "stopping a pending recording must not create a capture"


async def main(binary, runtime):
    portal = MockPortal()
    await portal.start()
    app = server = driver = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-shortcut-ui-") as directory:
        work = Path(directory)
        captures = work / "captures"
        captures.mkdir()
        config = work / "config" / "Jiffshot"
        config.mkdir(parents=True)
        xdg_runtime = work / "runtime"
        xdg_runtime.mkdir(mode=0o700)
        settings = config / "settings.json"
        settings.write_text(json.dumps({"capture_directory": str(captures), "theme": "Light"}))
        document = work / "selection.jsd"
        selection_document(document)
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
                    [str(binary), str(document)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                    stdout=app_log, stderr=app_log,
                )
                window = None
                for _ in range(150):
                    if app.poll() is not None:
                        raise RuntimeError(f"application exited during startup: {app.returncode}")
                    tree = subprocess.run(
                        ["xwininfo", "-root", "-tree"], env=env, capture_output=True, text=True, check=True
                    ).stdout
                    for line in tree.splitlines():
                        if '"Jiffshot"' in line:
                            window = int(line.split()[0], 16)
                            break
                    if window is not None:
                        info = subprocess.run(
                            ["xwininfo", "-id", hex(window)], env=env,
                            capture_output=True, text=True, check=True,
                        ).stdout
                        if "Map State: IsViewable" in info:
                            break
                        window = None
                    await asyncio.sleep(0.1)
                if window is None:
                    raise RuntimeError("application window did not appear")
                x = re.search(r"Absolute upper-left X:\s*(-?\d+)", info)
                y = re.search(r"Absolute upper-left Y:\s*(-?\d+)", info)
                if x is None or y is None:
                    raise RuntimeError("could not determine the editor window origin")
                driver = X11Driver(display_name, window, (int(x.group(1)), int(y.group(1))))
                await asyncio.sleep(1)

                await wait_for(lambda: driver.red_canvas_pixels() > 100, "the annotations to render")
                baseline = driver.red_canvas_pixels()
                driver.click(20, 400)
                driver.select_all()
                await asyncio.sleep(0.3)
                selected_baseline = driver.red_canvas_pixels()
                assert selected_baseline > 100, "selecting annotations hid the fixture"
                driver.replace_shortcut("CTRL+ALT+P")
                await asyncio.sleep(0.3)
                after_text_edit = driver.red_canvas_pixels()
                assert after_text_edit >= selected_baseline * 0.8, (
                    "editing shortcut text deleted selected annotations: "
                    f"before={selected_baseline}, after={after_text_edit}"
                )
                driver.click(20, 400)
                driver.select_all()
                driver.key("Delete", True)
                driver.key("Delete", False)
                driver.x11.XSync(driver.display, 0)
                await wait_for(
                    lambda: driver.red_canvas_pixels() < baseline * 0.1,
                    "Delete to remove the selected annotations",
                )
                driver.key("Control_L", True)
                driver.key("z", True)
                driver.key("z", False)
                driver.key("Control_L", False)
                driver.x11.XSync(driver.display, 0)
                await wait_for(
                    lambda: driver.red_canvas_pixels() >= selected_baseline * 0.8,
                    "Undo to restore the deleted annotations",
                )
                print("GUI selection and Delete preserve text editing and are undoable")
                driver.click(460, 80)
                await wait_for(lambda: len(portal.bindings) == 1, "the initial shortcut binding")
                session, shortcuts = portal.bindings[0]
                assert shortcuts[0][0] == "capture", shortcuts
                assert shortcuts[0][1]["preferred_trigger"].value == "CTRL+ALT+P", shortcuts
                saved = json.loads(settings.read_text())
                assert saved["global_capture_shortcut"] == "CTRL+ALT+P", saved
                assert saved["global_recording_shortcuts"] is False, saved
                assert saved["linux_global_shortcuts_enabled"] is True, saved
                assert saved["capture_directory"] == str(captures) and saved["theme"] == "Light", saved
                await asyncio.sleep(0.5)
                portal.activate(session + "_foreign", "capture")
                portal.activate(session, "other")
                portal.activate(session, "toggle-recording")
                await asyncio.sleep(0.5)
                assert portal.capture_requests == 0, "foreign shortcut events triggered capture"
                portal.activate(session, "capture")
                await wait_for(lambda: portal.capture_requests == 1, "capture shortcut activation")

                portal.screen_mode = "hold-start"
                previous = len(portal.screen_starts)
                driver.click(300, 45)
                await wait_for(
                    lambda: len(portal.screen_starts) == previous + 1,
                    "the capture-only recording request",
                )
                recording_session, recording_request = portal.screen_starts[-1]
                portal.activate(session, "pause-recording")
                portal.activate(session, "stop-recording")
                await asyncio.sleep(0.5)
                assert recording_request not in portal.closed_requests, (
                    "recording shortcuts were active without opt-in"
                )
                driver.click(560, 140)
                await wait_for(
                    lambda: recording_request in portal.closed_requests,
                    "the capture-only recording request to cancel",
                )
                await wait_for(
                    lambda: recording_session in portal.closes,
                    "the capture-only recording session to close",
                )

                driver.click(670, 80)
                await wait_for(lambda: session in portal.closes, "the active session to close")
                assert json.loads(settings.read_text())["linux_global_shortcuts_enabled"] is False
                await asyncio.sleep(0.4)
                print("GUI saved the preferred trigger, filtered events, captured and disabled")

                portal.mode = "hold"
                driver.click(460, 80)
                await wait_for(lambda: len(portal.bindings) == 2, "a pending shortcut chooser")
                pending = portal.bindings[1][0]
                driver.click(670, 80)
                await wait_for(lambda: pending in portal.closes, "the pending session to close")
                await asyncio.sleep(0.4)
                print("GUI cancelled a pending binding and closed its portal session")

                portal.mode = "reject"
                driver.click(460, 80)
                await wait_for(lambda: len(portal.bindings) == 3, "a rejected shortcut request")
                rejected = portal.bindings[2][0]
                await wait_for(lambda: rejected in portal.closes, "the rejected session to close")
                await asyncio.sleep(0.4)
                print("Rejected shortcut request closed its portal session")

                portal.mode = "success"
                driver.click(460, 80)
                await wait_for(lambda: len(portal.bindings) == 4, "a second accepted shortcut")
                remote = portal.bindings[3][0]
                await asyncio.sleep(0.4)
                portal.close_from_desktop(remote)
                await asyncio.sleep(0.5)
                driver.click(460, 80)
                await wait_for(lambda: len(portal.bindings) == 5, "re-enabling after desktop closure")
                assert remote not in portal.closes, "an already-closed session was closed twice"
                last = portal.bindings[4][0]
                await asyncio.sleep(0.4)
                driver.click(670, 80)
                await wait_for(lambda: last in portal.closes, "the final session to close")
                print("Desktop closure allowed the shortcut to be enabled again")

                await asyncio.sleep(0.4)
                driver.click(20, 175)
                driver.click(460, 80)
                await wait_for(lambda: len(portal.bindings) == 6, "recording shortcut bindings")
                recording_shortcuts_session, shortcuts = portal.bindings[-1]
                assert [shortcut[0] for shortcut in shortcuts] == [
                    "capture", "pause-recording", "stop-recording",
                ], shortcuts
                assert [shortcut[1]["preferred_trigger"].value for shortcut in shortcuts] == [
                    "CTRL+ALT+P", "SHIFT+F9", "SHIFT+F10",
                ], shortcuts
                saved = json.loads(settings.read_text())
                assert saved["global_recording_shortcuts"] is True, saved
                assert saved["linux_global_recording_start"] == "disabled", saved
                await asyncio.sleep(0.4)
                previous = portal.capture_requests
                portal.activate(recording_shortcuts_session, "toggle-recording")
                await asyncio.sleep(0.4)
                assert portal.capture_requests == previous, (
                    "recording controls must not enable the separate start shortcut by default"
                )

                portal.screen_mode = "hold-start"
                previous = len(portal.screen_starts)
                driver.click(300, 45)
                await wait_for(
                    lambda: len(portal.screen_starts) == previous + 1,
                    "the recording-shortcut test request",
                )
                recording_session, recording_request = portal.screen_starts[-1]
                portal.activate(recording_shortcuts_session + "_foreign", "stop-recording")
                portal.activate(recording_shortcuts_session, "pause-recording")
                await asyncio.sleep(0.5)
                assert recording_request not in portal.closed_requests, (
                    "pause or a foreign session stopped the pending recording"
                )
                portal.activate(recording_shortcuts_session, "stop-recording")
                await wait_for(
                    lambda: recording_request in portal.closed_requests,
                    "the global stop shortcut to cancel the pending request",
                )
                await wait_for(
                    lambda: recording_session in portal.closes,
                    "the global stop shortcut to close the pending recording session",
                )
                assert not list(captures.iterdir()), "the global stop saved an empty recording"
                driver.click(670, 80)
                await wait_for(
                    lambda: recording_shortcuts_session in portal.closes,
                    "the recording shortcuts session to close",
                )
                await asyncio.sleep(0.4)

                from linux_load_e2e import window_info
                from linux_webcam_e2e import TextReader, press, trace_window

                reader = TextReader()
                try:
                    combo = reader.find_label(driver, "No start shortcut")
                    if combo is None:
                        label = reader.find_label(driver, "Recording start shortcut")
                        if label is None:
                            trace_window(driver, "shortcut-start-missing")
                            observed = reader._recognize(driver, 0, 0, 800, 270, 6)
                            raise AssertionError(
                                f"the recording start choice must be visible; OCR saw {observed!r}"
                            )
                        combo = (label[0] + 175, label[1])
                    driver.click(*combo)
                    press(driver, "Home")
                    press(driver, "Down")
                    press(driver, "Down")
                    press(driver, "Return")
                    driver.click(460, 80)
                    await wait_for(lambda: len(portal.bindings) == 7, "the recording start binding")
                    recording_start_session, shortcuts = portal.bindings[-1]
                    assert [shortcut[0] for shortcut in shortcuts] == [
                        "capture", "toggle-recording", "pause-recording", "stop-recording",
                    ], shortcuts
                    assert shortcuts[1][1]["preferred_trigger"].value == "SHIFT+F8", shortcuts
                    saved = json.loads(settings.read_text())
                    assert saved["linux_global_recording_start"] == "microphone", saved
                    assert saved["capture_directory"] == str(captures) and saved["theme"] == "Light"
                    await asyncio.sleep(0.4)

                    await test_global_recording_start_routes_selected_audio_and_stops(
                        portal, recording_start_session, driver, reader, captures
                    )
                    driver.close()
                    driver = None
                    stop(app)
                    app = None
                    previous_bindings = len(portal.bindings)
                    app = subprocess.Popen(
                        [str(binary)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                        stdout=app_log, stderr=app_log,
                    )
                    await wait_for(lambda: window_info(env), "the restarted editor", 12)
                    window, origin = window_info(env)
                    driver = X11Driver(display_name, window, origin)
                    await wait_for(
                        lambda: len(portal.bindings) == previous_bindings + 1,
                        "the recording start shortcut to restore after restart",
                    )
                    restored_start_session, restored = portal.bindings[-1]
                    assert [shortcut[0] for shortcut in restored] == [
                        "capture", "toggle-recording", "pause-recording", "stop-recording",
                    ], restored
                    assert json.loads(settings.read_text())["linux_global_recording_start"] == (
                        "microphone"
                    )
                    await asyncio.sleep(0.4)
                    await test_global_recording_start_routes_selected_audio_and_stops(
                        portal, restored_start_session, driver, reader, captures
                    )
                    driver.click(670, 80)
                    await wait_for(
                        lambda: restored_start_session in portal.closes,
                        "the restored shortcut session to close",
                    )
                    await asyncio.sleep(0.4)
                    driver.click(*combo)
                    press(driver, "Home")
                    press(driver, "Return")
                    print(
                        "PASS test_global_recording_start_routes_selected_audio_and_stops "
                        "(including restart)",
                        flush=True,
                    )
                finally:
                    reader.close()

                portal.mode = "partial"
                driver.click(460, 80)
                await wait_for(lambda: len(portal.bindings) == 9, "a partial shortcut response")
                partial = portal.bindings[-1][0]
                await wait_for(
                    lambda: partial in portal.closes,
                    "the partially bound shortcut session to close",
                )
                await asyncio.sleep(0.4)
                portal.mode = "success"
                driver.click(20, 175)
                print("GUI opted in to recording shortcuts, routed stop, and rejected partial bindings")

                for phase, action in (
                    ("create", "cancel"),
                    ("create", "stop"),
                    ("select", "cancel"),
                    ("select", "stop"),
                    ("start", "cancel"),
                    ("start", "stop"),
                ):
                    portal.screen_mode = f"hold-{phase}"
                    requests = {
                        "create": portal.screen_creates,
                        "select": portal.screen_selects,
                        "start": portal.screen_starts,
                    }[phase]
                    previous = len(requests)
                    driver.click(300, 45)
                    await wait_for(
                        lambda: len(requests) == previous + 1,
                        f"the pending {phase} request",
                    )
                    pending, request = requests[-1]
                    if action == "cancel":
                        driver.click(560, 140)
                    else:
                        driver.click(300, 45)
                    await wait_for(
                        lambda: request in portal.closed_requests,
                        f"the {phase} request to close after {action}",
                        timeout=5,
                    )
                    await wait_for(
                        lambda: pending in portal.closes,
                        f"the {phase} session to close after {action}",
                    )
                    assert not list(captures.iterdir()), f"{action} during {phase} left a capture file"
                    await asyncio.sleep(0.5)

                portal.screen_mode = "hold-select"
                previous = len(portal.screen_selects)
                driver.click(300, 45)
                await wait_for(
                    lambda: len(portal.screen_selects) == previous + 1,
                    "the selection response/cancel race",
                )
                pending, request = portal.screen_selects[-1]
                portal.response(request, 0, {})
                driver.click(560, 140)
                await wait_for(lambda: pending in portal.closes, "the raced session to close")
                await asyncio.sleep(0.5)
                for session, start_request in portal.screen_starts:
                    if session == pending:
                        assert start_request in portal.closed_requests, (
                            "a start request created during cancellation remained open"
                        )
                assert not list(captures.iterdir()), "the cancellation race left a capture file"
                print("GUI cancelled and stopped all pending portal phases and a response race")

                driver.close()
                driver = None
                stop(app)
                app = None
                saved = json.loads(settings.read_text())
                assert saved["linux_global_shortcuts_enabled"] is True, saved
                previous_bindings = len(portal.bindings)
                app = subprocess.Popen(
                    [str(binary)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                    stdout=app_log, stderr=app_log,
                )
                await wait_for(
                    lambda: len(portal.bindings) > previous_bindings,
                    "the opted-in shortcuts to bind after restart",
                )
                restored_session, restored = portal.bindings[-1]
                assert [shortcut[0] for shortcut in restored] == [
                    "capture", "pause-recording", "stop-recording",
                ], restored
                assert restored[0][1]["preferred_trigger"].value == "CTRL+ALT+P", restored
                previous_requests = portal.capture_requests
                portal.screen_mode = "reject"
                portal.activate(restored_session, "capture")
                await wait_for(
                    lambda: portal.capture_requests > previous_requests,
                    "the restored shortcut to activate",
                )
                stop(app)
                app = None
                saved = json.loads(settings.read_text())
                settings.write_text(json.dumps({**saved, "linux_global_shortcuts_enabled": False}))
                previous_bindings = len(portal.bindings)
                app = subprocess.Popen(
                    [str(binary)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                    stdout=app_log, stderr=app_log,
                )
                await asyncio.sleep(1.5)
                assert app.poll() is None
                assert len(portal.bindings) == previous_bindings, (
                    "disabled shortcuts must not bind on restart"
                )
                stop(app)
                app = None
                print("Opted-in shortcuts restored after restart and stayed disabled after opt-out")
                portal.screen_mode = "hold-select"
                for argument, expected_cursor_mode, cursor_settings in (
                    ("--capture", 2, {}),
                    ("--scrolling", 1, {}),
                    ("--scrolling-horizontal", 1, {}),
                    ("--panoramic", 1, {}),
                    ("--record", 2, {}),
                    ("--capture", 1, {"capture_cursor": False, "recording_cursor": True}),
                    ("--record", 2, {"capture_cursor": False, "recording_cursor": True}),
                    ("--capture", 2, {"capture_cursor": True, "recording_cursor": False}),
                    ("--record", 1, {"capture_cursor": True, "recording_cursor": False}),
                    ("--scrolling", 1, {"capture_cursor": True, "recording_cursor": False}),
                ):
                    saved = json.loads(settings.read_text())
                    settings.write_text(json.dumps({**saved, **cursor_settings}))
                    previous = len(portal.screen_selects)
                    app = subprocess.Popen(
                        [str(binary), argument], env=env, cwd=work, stdin=subprocess.DEVNULL,
                        stdout=app_log, stderr=app_log,
                    )
                    await wait_for(
                        lambda: len(portal.screen_selects) == previous + 1,
                        f"the {argument} startup action",
                    )
                    startup_session, startup_request = portal.screen_selects[-1]
                    options = portal.screen_selection_options[-1]
                    assert options["cursor_mode"].value == expected_cursor_mode, (
                        argument, options
                    )
                    portal.response(startup_request, 1, {})
                    await wait_for(
                        lambda: startup_session in portal.closes,
                        f"the {argument} session to close after rejection",
                    )
                    stop(app)
                    app = None
                for argument, invalid_key in (
                    ("--capture", "capture_cursor"),
                    ("--record", "recording_cursor"),
                ):
                    saved = json.loads(settings.read_text())
                    settings.write_text(json.dumps({**saved, invalid_key: "invalid"}))
                    previous = portal.capture_requests
                    app = subprocess.Popen(
                        [str(binary), argument], env=env, cwd=work, stdin=subprocess.DEVNULL,
                        stdout=app_log, stderr=app_log,
                    )
                    await wait_for(
                        lambda: '"Jiffshot"' in subprocess.run(
                            ["xwininfo", "-root", "-tree"], env=env, capture_output=True,
                            text=True, check=True,
                        ).stdout,
                        f"the editor for {argument}",
                        12,
                    )
                    await asyncio.sleep(0.5)
                    assert portal.capture_requests == previous, (
                        f"an invalid {invalid_key} must block the portal request"
                    )
                    stop(app)
                    app = None
                    settings.write_text(json.dumps(saved))
                print("Command-line actions honored independent cursor settings and rejected invalid values")
        except BaseException:
            print((work / "xvfb.log").read_text(errors="replace")[-12000:], file=sys.stderr)
            print((work / "app.log").read_text(errors="replace")[-12000:], file=sys.stderr)
            print(
                f"sessions={portal.sessions!r} bindings={portal.bindings!r} closes={portal.closes!r} "
                f"screen_creates={portal.screen_creates!r} screen_selects={portal.screen_selects!r} "
                f"screen_starts={portal.screen_starts!r} "
                f"closed_requests={portal.closed_requests!r}",
                file=sys.stderr,
            )
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
