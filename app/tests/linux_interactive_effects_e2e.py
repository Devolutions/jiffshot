#!/usr/bin/env python3
"""Exercise Linux interactive effects in a private Xvfb with a mock portal."""

import argparse
import asyncio
from collections import deque
import errno
import json
import os
from pathlib import Path
import select
import struct
import subprocess
import sys
import tempfile
import zlib

from dbus_next import Message, MessageType, Variant
from linux_load_e2e import solid_png, window_info
from linux_shortcuts_e2e import DESKTOP, MockPortal, X11Driver, stop, wait_for
from linux_webcam_e2e import pixels, press, trace_window


class EffectsPortal(MockPortal):
    def __init__(self):
        super().__init__()
        self.open_requests = []
        self.save_requests = []
        self.save_destinations = deque()
        self.save_responses = deque()
        self.open_parents = []
        self.save_parents = []
        self.fail_next_open = False
        self.pending_open_errors = {}

    def message(self, message):
        if message.message_type != MessageType.METHOD_CALL:
            return None
        if (
            message.interface == "org.freedesktop.DBus.Properties"
            and message.member == "Get"
            and message.body[0] == "org.freedesktop.portal.FileChooser"
        ):
            return Message.new_method_return(message, "v", [Variant("u", 4)])
        if message.interface == "org.freedesktop.portal.FileChooser" and message.member in (
            "OpenFile", "SaveFile"
        ):
            parent, title, options = message.body
            sender = message.sender.lstrip(":").replace(".", "_")
            request = f"{DESKTOP}/request/{sender}/{options['handle_token'].value}"
            if message.member == "OpenFile":
                self.open_requests.append((title, options, request))
                self.open_parents.append(parent)
                if self.fail_next_open:
                    self.fail_next_open = False
                    self.pending_open_errors[len(self.open_requests) - 1] = message
                    return True
            else:
                self.save_parents.append(parent)
                if self.save_responses:
                    code, result = self.save_responses.popleft()
                    self.save_requests.append((title, options, None))
                    asyncio.get_running_loop().call_later(
                        0.1, self.response, request, code, result
                    )
                elif not self.save_destinations:
                    return Message.new_error(
                        message, "org.freedesktop.portal.Error.Failed", "unexpected save dialog"
                    )
                else:
                    destination = self.save_destinations.popleft()
                    self.save_requests.append((title, options, destination))
                    asyncio.get_running_loop().call_later(
                        0.1, self.response, request, 0,
                        {"uris": Variant("as", [destination.as_uri()])},
                    )
            return Message.new_method_return(message, "o", [request])
        return super().message(message)

    def finish_open(self, index, code, uri=None, omit_uris=False):
        request = self.open_requests[index][2]
        # The XDG FileChooser frontend includes an empty URI array on non-success.
        result = {} if omit_uris else {"uris": Variant("as", [uri] if code == 0 else [])}
        # Allow the picker to subscribe to the portal response signal.
        asyncio.get_running_loop().call_later(0.1, self.response, request, code, result)

    def fail_open(self, index):
        message = self.pending_open_errors.pop(index)
        self.bus.send(Message.new_error(
            message, "org.freedesktop.portal.Error.Failed", "mock file chooser unavailable"
        ))


class Image:
    def __init__(self, width, height, rgba):
        self.width = width
        self.height = height
        self.rgba = rgba

    def pixel(self, x, y):
        offset = (y * self.width + x) * 4
        return tuple(self.rgba[offset:offset + 4])

    def __eq__(self, other):
        return (
            isinstance(other, Image)
            and (self.width, self.height, self.rgba)
            == (other.width, other.height, other.rgba)
        )


def decode_png(path):
    data = path.read_bytes()
    assert data.startswith(b"\x89PNG\r\n\x1a\n"), "export must be a PNG"
    offset = 8
    compressed = bytearray()
    width = height = channels = None
    while offset < len(data):
        length = struct.unpack_from(">I", data, offset)[0]
        kind = data[offset + 4:offset + 8]
        chunk = data[offset + 8:offset + 8 + length]
        offset += length + 12
        if kind == b"IHDR":
            width, height, depth, color, compression, filtering, interlace = struct.unpack(
                ">IIBBBBB", chunk
            )
            assert depth == 8 and color in (2, 6), (depth, color)
            assert (compression, filtering, interlace) == (0, 0, 0)
            channels = 3 if color == 2 else 4
        elif kind == b"IDAT":
            compressed.extend(chunk)
        elif kind == b"IEND":
            break
    assert width and height and channels, "PNG header must describe an image"
    raw = zlib.decompress(compressed)
    stride = width * channels
    assert len(raw) == height * (stride + 1), "PNG must contain every scanline"
    previous = bytearray(stride)
    rgba = bytearray()
    for row_index in range(height):
        start = row_index * (stride + 1)
        mode = raw[start]
        row = bytearray(raw[start + 1:start + 1 + stride])
        assert mode in range(5), f"unsupported PNG filter {mode}"
        for i in range(stride):
            left = row[i - channels] if i >= channels else 0
            above = previous[i]
            upper_left = previous[i - channels] if i >= channels else 0
            if mode == 1:
                predictor = left
            elif mode == 2:
                predictor = above
            elif mode == 3:
                predictor = (left + above) // 2
            elif mode == 4:
                estimate = left + above - upper_left
                distances = (
                    abs(estimate - left), abs(estimate - above), abs(estimate - upper_left)
                )
                predictor = (left, above, upper_left)[distances.index(min(distances))]
            else:
                predictor = 0
            row[i] = (row[i] + predictor) & 0xFF
        if channels == 4:
            rgba.extend(row)
        else:
            for i in range(0, stride, 3):
                rgba.extend(row[i:i + 3])
                rgba.append(255)
        previous = row
    return Image(width, height, bytes(rgba))


def hotkey(driver, key):
    driver.key("Control_L", True)
    driver.key(key, True)
    driver.key(key, False)
    driver.key("Control_L", False)
    driver.x11.XSync(driver.display, 0)


def status_pixels(driver):
    return pixels(driver, 0, 480, 800, 20)[1]


def canvas_pixels(driver):
    return pixels(driver, 0, 280, 800, 190)[1]


def count_canvas_color(driver, color):
    stride, data = pixels(driver, 0, 280, 800, 190)
    red, green, blue = color
    return sum(
        data[y * stride + x * 4:y * stride + x * 4 + 3] == bytes((blue, green, red))
        for y in range(190) for x in range(800)
    )


def wheel(driver, direction, count):
    driver.xtst.XTestFakeMotionEvent(
        driver.display, driver.screen, 750 + driver.origin[0], 150 + driver.origin[1], 0
    )
    button = 5 if direction == "down" else 4
    for _ in range(count):
        driver.xtst.XTestFakeButtonEvent(driver.display, button, 1, 0)
        driver.xtst.XTestFakeButtonEvent(driver.display, button, 0, 0)
    driver.x11.XSync(driver.display, 0)


async def show_effects(driver):
    for _ in range(2):
        wheel(driver, "down", 3)
        await asyncio.sleep(0.15)
    before = pixels(driver, 0, 115, 800, 35)[1]
    driver.click(178, 96)
    await wait_for(
        lambda: pixels(driver, 0, 115, 800, 35)[1] != before,
        "the editor properties and effects panel",
    )


async def select_effect(driver, previous_from_end):
    driver.click(525, 130)
    press(driver, "End")
    for _ in range(previous_from_end):
        press(driver, "Up")
    press(driver, "Return")
    await asyncio.sleep(0.1)


async def select_first_effect(driver):
    driver.click(525, 130)
    press(driver, "Home")
    press(driver, "Return")
    await asyncio.sleep(0.1)


def set_effect_parameter(driver, value):
    driver.click(260, 160)
    driver.select_all()
    press(driver, "BackSpace")
    for character in value:
        press(driver, character)


async def export_image(driver, portal, work):
    destination = work / f"effect-export-{len(portal.save_requests)}.png"
    portal.save_destinations.append(destination)
    previous = len(portal.save_requests)
    previous_status = status_pixels(driver)
    hotkey(driver, "e")
    await wait_for(lambda: len(portal.save_requests) > previous, "the export dialog")
    assert int(portal.save_parents[previous].removeprefix("x11:"), 16) == driver.window, (
        "the export dialog must be parented to the editor"
    )
    await wait_for(
        lambda: destination.exists()
        and destination.read_bytes().endswith(b"\x00\x00\x00\x00IEND\xaeB`\x82"),
        "the completed PNG export",
    )
    await wait_for(lambda: status_pixels(driver) != previous_status, "the export status")
    return decode_png(destination)


async def apply_effect(driver, previous_from_end, description):
    await select_effect(driver, previous_from_end)
    before = canvas_pixels(driver)
    driver.click(670, 130)
    await wait_for(lambda: canvas_pixels(driver) != before, description)


async def undo_effect(driver):
    before = canvas_pixels(driver)
    hotkey(driver, "z")
    await wait_for(lambda: canvas_pixels(driver) != before, "undoing the editor change")


async def begin_watermark(driver, portal, parameter=None):
    await select_effect(driver, 1)
    if parameter is not None:
        set_effect_parameter(driver, parameter)
    previous = len(portal.open_requests)
    before_status = status_pixels(driver)
    driver.click(670, 130)
    await wait_for(lambda: len(portal.open_requests) > previous, "the watermark picker")
    await wait_for(
        lambda: status_pixels(driver) != before_status, "the watermark selection prompt"
    )
    title, options, _request = portal.open_requests[previous]
    assert title == "Choose a watermark image", title
    assert int(portal.open_parents[previous].removeprefix("x11:"), 16) == driver.window, (
        "the watermark dialog must be parented to the editor"
    )
    filters = options.get("filters")
    assert filters is not None, "the watermark picker must offer image filters"
    patterns = {pattern for _label, entries in filters.value for _kind, pattern in entries}
    assert {"*.png", "*.jpg", "*.jpeg", "*.bmp", "*.gif", "*.webp"} <= patterns, patterns
    return previous


async def draw_annotation(driver):
    hotkey(driver, "2")

    def move(x, y):
        driver.xtst.XTestFakeMotionEvent(
            driver.display, driver.screen, x + driver.origin[0], y + driver.origin[1], 0
        )

    move(310, 340)
    driver.xtst.XTestFakeButtonEvent(driver.display, 1, 1, 0)
    move(480, 420)
    driver.xtst.XTestFakeButtonEvent(driver.display, 1, 0, 0)
    driver.x11.XSync(driver.display, 0)
    await wait_for(lambda: driver.red_canvas_pixels() > 40, "a newer red annotation")


async def fifo_writer(path):
    opened = []

    def connect():
        try:
            opened.append(os.open(path, os.O_WRONLY | os.O_NONBLOCK))
        except OSError as error:
            if error.errno == errno.ENXIO:
                return False
            raise
        return True

    await wait_for(connect, "the background watermark reader")
    return opened[0]


async def set_selection_tool_status(driver):
    previous = status_pixels(driver)
    hotkey(driver, "1")
    await wait_for(lambda: status_pixels(driver) != previous, "the selection tool status")
    return status_pixels(driver)


async def test_selector_capture_info_adds_band_and_undoes(driver, portal, work, baseline):
    await apply_effect(driver, 2, "the Capture info band")
    result = await export_image(driver, portal, work)
    assert result.width == 240 and result.height > 140, (result.width, result.height)
    assert result.pixel(0, 0) == (30, 120, 190, 255)
    assert result.pixel(0, 140) == (34, 38, 46, 255)
    assert any(
        result.pixel(x, y) != (34, 38, 46, 255)
        for y in range(140, result.height) for x in range(result.width)
    ), "the capture information band must contain visible text"
    await undo_effect(driver)
    assert await export_image(driver, portal, work) == baseline


async def verify_invalid_capture_sidecar(driver, portal, work, source, baseline, contents):
    sidecar = source.with_suffix(".json")
    backup = work / "capture-sidecar-backup.json"
    sidecar.rename(backup)
    os.mkfifo(sidecar)
    fd = None
    try:
        await select_effect(driver, 2)
        driver.click(670, 130)
        fd = await fifo_writer(sidecar)
        before_status = await set_selection_tool_status(driver)
        with os.fdopen(fd, "wb", buffering=0) as writer:
            fd = None
            writer.write(contents)
        await wait_for(lambda: status_pixels(driver) != before_status, "the capture metadata error")
        assert await export_image(driver, portal, work) == baseline, (
            "invalid capture metadata must not change the document"
        )
    finally:
        if fd is not None:
            os.close(fd)
        sidecar.unlink()
        backup.rename(sidecar)


async def test_selector_capture_info_rejects_malformed_sidecar(driver, portal, work, source, baseline):
    await verify_invalid_capture_sidecar(
        driver, portal, work, source, baseline, b'{"window_title":"unfinished"'
    )


async def test_selector_capture_info_rejects_oversized_sidecar(driver, portal, work, source, baseline):
    contents = json.dumps({"window_title": "A" * 1015}).encode("utf-8")
    await verify_invalid_capture_sidecar(driver, portal, work, source, baseline, contents)


async def test_selector_spotlight_magnifies_image_center_and_undoes(driver, portal, work, baseline):
    await apply_effect(driver, 0, "the centered Spotlight and magnify effect")
    result = await export_image(driver, portal, work)
    assert (result.width, result.height) == (240, 140)
    assert result.pixel(0, 0) != baseline.pixel(0, 0), "the outside must be dimmed"
    assert result.pixel(120, 70) == baseline.pixel(120, 70), "the center must remain visible"
    await undo_effect(driver)
    assert await export_image(driver, portal, work) == baseline


async def test_custom_border_parameter_is_applied_and_undoable(driver, portal, work, baseline):
    await select_first_effect(driver)
    set_effect_parameter(driver, "3")
    before = canvas_pixels(driver)
    driver.click(670, 130)
    await wait_for(lambda: canvas_pixels(driver) != before, "the configured border")
    result = await export_image(driver, portal, work)
    assert (result.width, result.height) == (246, 146), (result.width, result.height)
    assert result.pixel(3, 3) == baseline.pixel(0, 0)
    assert result.pixel(0, 0) != baseline.pixel(0, 0)
    await undo_effect(driver)
    assert await export_image(driver, portal, work) == baseline


async def test_invalid_parameter_preserves_document_and_switching_effect_resets_it(
    driver, portal, work, baseline
):
    await select_first_effect(driver)
    set_effect_parameter(driver, "999")
    before_status = status_pixels(driver)
    driver.click(670, 130)
    await wait_for(lambda: status_pixels(driver) != before_status, "the invalid parameter error")
    assert await export_image(driver, portal, work) == baseline, (
        "invalid effect parameters must not alter the document"
    )
    set_effect_parameter(driver, "20")
    await select_effect(driver, 2)
    await select_first_effect(driver)
    before = canvas_pixels(driver)
    driver.click(670, 130)
    await wait_for(lambda: canvas_pixels(driver) != before, "the default border after switching")
    result = await export_image(driver, portal, work)
    assert (result.width, result.height) == (256, 156), (
        "switching effects must clear the old parameter and restore the default",
        result.width,
        result.height,
    )
    await undo_effect(driver)
    assert await export_image(driver, portal, work) == baseline


async def test_custom_spotlight_magnification_is_applied_and_undoable(
    driver, portal, work, baseline
):
    await select_effect(driver, 0)
    set_effect_parameter(driver, "2")
    before = canvas_pixels(driver)
    driver.click(670, 130)
    await wait_for(lambda: canvas_pixels(driver) != before, "the custom spotlight")
    result = await export_image(driver, portal, work)
    assert result.pixel(45, 70) == baseline.pixel(45, 70), (
        "2x magnification must leave the wider spotlight area undimmed"
    )
    assert result.pixel(20, 70) != baseline.pixel(20, 70), (
        "the area outside the spotlight must still be dimmed"
    )
    await undo_effect(driver)
    assert await export_image(driver, portal, work) == baseline


async def test_watermark_picker_blends_local_image_and_undoes(driver, portal, work, baseline, mark):
    request = await begin_watermark(driver, portal)
    before = canvas_pixels(driver)
    portal.finish_open(request, 0, mark.as_uri())
    await wait_for(lambda: canvas_pixels(driver) != before, "the decoded watermark")
    result = await export_image(driver, portal, work)
    assert (result.width, result.height) == (240, 140)
    assert result.pixel(200, 115) == (109, 78, 124, 255), result.pixel(200, 115)
    assert result.pixel(20, 20) == (30, 120, 190, 255)
    await undo_effect(driver)
    assert await export_image(driver, portal, work) == baseline


async def test_watermark_parameter_is_snapshotted_before_the_picker_and_undoes(
    driver, portal, work, baseline, mark
):
    request = await begin_watermark(driver, portal, "80")
    set_effect_parameter(driver, "20")
    before = canvas_pixels(driver)
    portal.finish_open(request, 0, mark.as_uri())
    await wait_for(lambda: canvas_pixels(driver) != before, "the custom-opacity watermark")
    result = await export_image(driver, portal, work)
    assert result.pixel(200, 115) == (210, 24, 38, 255), result.pixel(200, 115)
    assert result.pixel(20, 20) == baseline.pixel(20, 20)
    driver.click(25, 300)
    await undo_effect(driver)
    assert await export_image(driver, portal, work) == baseline


async def test_invalid_watermark_parameter_does_not_open_picker(driver, portal, work, baseline):
    await select_effect(driver, 1)
    set_effect_parameter(driver, "101")
    previous_requests = len(portal.open_requests)
    before_status = status_pixels(driver)
    driver.click(670, 130)
    await wait_for(lambda: status_pixels(driver) != before_status, "the invalid watermark opacity")
    await asyncio.sleep(0.3)
    assert len(portal.open_requests) == previous_requests, (
        "invalid opacity must be rejected before opening a file chooser"
    )
    assert await export_image(driver, portal, work) == baseline
    set_effect_parameter(driver, "")


async def verify_picker_non_success(
    driver, portal, work, baseline, code, description, omit_uris=False
):
    request = await begin_watermark(driver, portal)
    before_status = status_pixels(driver)
    portal.finish_open(request, code, omit_uris=omit_uris)
    await wait_for(
        lambda: status_pixels(driver) != before_status, f"the {description} picker result"
    )
    trace_window(driver, f"interactive-effects-picker-{code}")
    try:
        result = await export_image(driver, portal, work)
    except AssertionError as error:
        raise AssertionError(
            f"export must remain available after the {description} picker: {error}"
        ) from error
    assert result == baseline, f"a {description} picker must not apply a watermark"


async def test_watermark_picker_cancellation_leaves_document_unchanged(
    driver, portal, work, baseline
):
    await verify_picker_non_success(driver, portal, work, baseline, 1, "cancelled")


async def test_watermark_picker_error_leaves_document_unchanged(driver, portal, work, baseline):
    portal.fail_next_open = True
    request = await begin_watermark(driver, portal)
    before_status = status_pixels(driver)
    portal.fail_open(request)
    await wait_for(lambda: status_pixels(driver) != before_status, "the picker method error")
    trace_window(driver, "interactive-effects-picker-method-error")
    try:
        result = await export_image(driver, portal, work)
    except AssertionError as error:
        raise AssertionError(f"export must remain available after the picker error: {error}") from error
    assert result == baseline, "a picker method error must not apply a watermark"


async def test_watermark_picker_other_response_leaves_document_unchanged(
    driver, portal, work, baseline
):
    await verify_picker_non_success(driver, portal, work, baseline, 2, "ended")


async def test_watermark_cancel_without_uris_keeps_later_dialogs_usable(
    driver, portal, work, baseline
):
    await verify_picker_non_success(
        driver, portal, work, baseline, 1, "cancelled without URIs", omit_uris=True
    )


async def test_watermark_error_without_uris_keeps_later_dialogs_usable(
    driver, portal, work, baseline
):
    await verify_picker_non_success(
        driver, portal, work, baseline, 2, "failed without URIs", omit_uris=True
    )


async def test_export_cancel_and_failure_keep_later_dialogs_usable(
    driver, portal, work, baseline
):
    for code in (1, 2):
        portal.save_responses.append((code, {}))
        previous = len(portal.save_requests)
        before_status = status_pixels(driver)
        hotkey(driver, "e")
        await wait_for(lambda: len(portal.save_requests) > previous, "the export dialog")
        assert int(portal.save_parents[previous].removeprefix("x11:"), 16) == driver.window
        if code == 2:
            await wait_for(lambda: status_pixels(driver) != before_status, "the export error")
        else:
            await asyncio.sleep(0.3)
        assert await export_image(driver, portal, work) == baseline, (
            f"an export response {code} must leave the document and future pickers usable"
        )


async def test_delayed_watermark_picker_preserves_a_newer_edit(driver, portal, work, mark):
    request = await begin_watermark(driver, portal)
    await draw_annotation(driver)
    edited = await export_image(driver, portal, work)
    red_pixels = sum(
        edited.pixel(x, y) == (229, 57, 53, 255)
        for y in range(edited.height) for x in range(edited.width)
    )
    assert red_pixels > 40, f"the red annotation must be exported, got {red_pixels} pixels"
    before_status = await set_selection_tool_status(driver)
    portal.finish_open(request, 0, mark.as_uri())
    await wait_for(
        lambda: status_pixels(driver) != before_status, "rejecting the stale picker result"
    )
    assert await export_image(driver, portal, work) == edited, (
        "a late picker must preserve the newer annotation and not apply the watermark"
    )


async def test_blocked_watermark_decode_preserves_a_newer_edit(driver, portal, work, mark_bytes):
    fifo = work / "blocked-watermark.png"
    os.mkfifo(fifo)
    request = await begin_watermark(driver, portal)
    portal.finish_open(request, 0, fifo.as_uri())
    fd = await fifo_writer(fifo)
    try:
        await draw_annotation(driver)
        edited = await export_image(driver, portal, work)
        before_status = await set_selection_tool_status(driver)
        with os.fdopen(fd, "wb", buffering=0) as writer:
            fd = None
            writer.write(mark_bytes)
        await wait_for(
            lambda: status_pixels(driver) != before_status, "rejecting the stale image read"
        )
        assert await export_image(driver, portal, work) == edited, (
            "a late watermark decode must preserve the newer annotation"
        )
    finally:
        if fd is not None:
            os.close(fd)


async def test_invalid_watermark_image_does_not_change_document(driver, portal, work, baseline):
    fifo = work / "invalid-watermark.png"
    os.mkfifo(fifo)
    request = await begin_watermark(driver, portal)
    portal.finish_open(request, 0, fifo.as_uri())
    fd = await fifo_writer(fifo)
    before_status = status_pixels(driver)
    with os.fdopen(fd, "wb", buffering=0) as writer:
        writer.write(b"not an image")
    await wait_for(lambda: status_pixels(driver) != before_status, "the image decode error")
    assert await export_image(driver, portal, work) == baseline, (
        "a failed watermark decode must not change the document"
    )


async def test_nonlocal_watermark_uri_does_not_change_document(driver, portal, work, baseline):
    request = await begin_watermark(driver, portal)
    before_status = status_pixels(driver)
    portal.finish_open(request, 0, "https://example.invalid/watermark.png")
    await wait_for(lambda: status_pixels(driver) != before_status, "rejecting a nonlocal image")
    assert await export_image(driver, portal, work) == baseline, (
        "a nonlocal watermark URI must not change the document"
    )


async def paste_replacement_document(driver, env, replacement, color):
    subprocess.run(
        ["xclip", "-selection", "clipboard", "-t", "image/png", "-i", str(replacement)],
        env=env, check=True, timeout=5,
    )
    before = pixels(driver, 0, 0, 800, 275)[1]
    wheel(driver, "up", 6)
    await wait_for(lambda: pixels(driver, 0, 0, 800, 275)[1] != before, "the image toolbar")
    driver.click(375, 207)
    await wait_for(
        lambda: count_canvas_color(driver, color) > 5000,
        "the pasted replacement document",
    )
    before = pixels(driver, 0, 0, 800, 275)[1]
    wheel(driver, "down", 6)
    await wait_for(lambda: pixels(driver, 0, 0, 800, 275)[1] != before, "the effects toolbar")


async def test_delayed_watermark_picker_preserves_a_newer_document(
    driver, portal, work, env, replacement, mark
):
    request = await begin_watermark(driver, portal)
    await paste_replacement_document(driver, env, replacement, (10, 200, 40))
    expected = await export_image(driver, portal, work)
    assert expected.pixel(200, 115) == (10, 200, 40, 255)
    before_status = await set_selection_tool_status(driver)
    portal.finish_open(request, 0, mark.as_uri())
    await wait_for(
        lambda: status_pixels(driver) != before_status,
        "rejecting the picker result for the replaced document",
    )
    assert await export_image(driver, portal, work) == expected, (
        "a delayed picker must not watermark a newer document"
    )


async def test_blocked_watermark_decode_preserves_a_newer_document(
    driver, portal, work, env, replacement, mark_bytes
):
    fifo = work / "blocked-before-replacement.png"
    os.mkfifo(fifo)
    request = await begin_watermark(driver, portal)
    portal.finish_open(request, 0, fifo.as_uri())
    fd = await fifo_writer(fifo)
    try:
        await paste_replacement_document(driver, env, replacement, (170, 30, 200))
        expected = await export_image(driver, portal, work)
        assert expected.pixel(200, 115) == (170, 30, 200, 255)
        before_status = await set_selection_tool_status(driver)
        with os.fdopen(fd, "wb", buffering=0) as writer:
            fd = None
            writer.write(mark_bytes)
        await wait_for(
            lambda: status_pixels(driver) != before_status,
            "rejecting the image read for the replaced document",
        )
        assert await export_image(driver, portal, work) == expected, (
            "a delayed image decode must not watermark a newer document"
        )
    finally:
        if fd is not None:
            os.close(fd)


async def main(binary, runtime):
    app = server = driver = portal = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-interactive-effects-ui-") as directory:
        work = Path(directory)
        runtime_dir = work / "runtime"
        runtime_dir.mkdir(mode=0o700)
        source = work / "capture.png"
        source.write_bytes(solid_png(30, 120, 190))
        source.with_suffix(".json").write_text(json.dumps({
            "captured_at": "2026-03-01T10:20:30",
            "window_title": "Design review",
            "process": "Browser",
        }))
        mark = work / "watermark.png"
        mark_bytes = solid_png(255, 0, 0)
        mark.write_bytes(mark_bytes)
        replacement = work / "replacement.png"
        replacement.write_bytes(solid_png(10, 200, 40))
        second_replacement = work / "second-replacement.png"
        second_replacement.write_bytes(solid_png(170, 30, 200))
        config = work / "config" / "Jiffshot"
        config.mkdir(parents=True)
        (config / "settings.json").write_text(json.dumps({"theme": "Light"}))
        portal = EffectsPortal()
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
                    [str(binary), str(source)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                    stdout=app_log, stderr=app_log,
                )
                await wait_for(lambda: window_info(env), "the editor", 12)
                window, origin = window_info(env)
                driver = X11Driver(display, window, origin)
                await wait_for(
                    lambda: count_canvas_color(driver, (30, 120, 190)) > 5000,
                    "the source image",
                )
                await show_effects(driver)
                trace_window(driver, "interactive-effects-controls")
                baseline = await export_image(driver, portal, work)
                assert (baseline.width, baseline.height) == (240, 140)
                assert baseline.pixel(200, 115) == (30, 120, 190, 255)

                await test_custom_border_parameter_is_applied_and_undoable(
                    driver, portal, work, baseline
                )
                print("PASS test_custom_border_parameter_is_applied_and_undoable", flush=True)
                await test_invalid_parameter_preserves_document_and_switching_effect_resets_it(
                    driver, portal, work, baseline
                )
                print(
                    "PASS test_invalid_parameter_preserves_document_and_switching_effect_resets_it",
                    flush=True,
                )
                await test_custom_spotlight_magnification_is_applied_and_undoable(
                    driver, portal, work, baseline
                )
                print("PASS test_custom_spotlight_magnification_is_applied_and_undoable", flush=True)
                await test_selector_capture_info_adds_band_and_undoes(
                    driver, portal, work, baseline
                )
                print("PASS test_selector_capture_info_adds_band_and_undoes", flush=True)
                await test_selector_capture_info_rejects_malformed_sidecar(
                    driver, portal, work, source, baseline
                )
                print("PASS test_selector_capture_info_rejects_malformed_sidecar", flush=True)
                await test_selector_capture_info_rejects_oversized_sidecar(
                    driver, portal, work, source, baseline
                )
                print("PASS test_selector_capture_info_rejects_oversized_sidecar", flush=True)
                await test_selector_spotlight_magnifies_image_center_and_undoes(
                    driver, portal, work, baseline
                )
                print("PASS test_selector_spotlight_magnifies_image_center_and_undoes", flush=True)
                await test_watermark_picker_blends_local_image_and_undoes(
                    driver, portal, work, baseline, mark
                )
                print("PASS test_watermark_picker_blends_local_image_and_undoes", flush=True)
                await test_watermark_parameter_is_snapshotted_before_the_picker_and_undoes(
                    driver, portal, work, baseline, mark
                )
                print(
                    "PASS test_watermark_parameter_is_snapshotted_before_the_picker_and_undoes",
                    flush=True,
                )
                await test_invalid_watermark_parameter_does_not_open_picker(
                    driver, portal, work, baseline
                )
                print("PASS test_invalid_watermark_parameter_does_not_open_picker", flush=True)
                await test_invalid_watermark_image_does_not_change_document(
                    driver, portal, work, baseline
                )
                print("PASS test_invalid_watermark_image_does_not_change_document", flush=True)
                await test_nonlocal_watermark_uri_does_not_change_document(
                    driver, portal, work, baseline
                )
                print("PASS test_nonlocal_watermark_uri_does_not_change_document", flush=True)
                await test_delayed_watermark_picker_preserves_a_newer_edit(
                    driver, portal, work, mark
                )
                print("PASS test_delayed_watermark_picker_preserves_a_newer_edit", flush=True)
                await undo_effect(driver)
                assert await export_image(driver, portal, work) == baseline
                await test_blocked_watermark_decode_preserves_a_newer_edit(
                    driver, portal, work, mark_bytes
                )
                print("PASS test_blocked_watermark_decode_preserves_a_newer_edit", flush=True)
                await undo_effect(driver)
                assert await export_image(driver, portal, work) == baseline
                await test_delayed_watermark_picker_preserves_a_newer_document(
                    driver, portal, work, env, replacement, mark
                )
                print("PASS test_delayed_watermark_picker_preserves_a_newer_document", flush=True)
                await test_blocked_watermark_decode_preserves_a_newer_document(
                    driver, portal, work, env, second_replacement, mark_bytes
                )
                print("PASS test_blocked_watermark_decode_preserves_a_newer_document", flush=True)
                await test_export_cancel_and_failure_keep_later_dialogs_usable(
                    driver, portal, work, await export_image(driver, portal, work)
                )
                print("PASS test_export_cancel_and_failure_keep_later_dialogs_usable", flush=True)
                failures = []
                for test in (
                    test_watermark_picker_cancellation_leaves_document_unchanged,
                    test_watermark_picker_error_leaves_document_unchanged,
                    test_watermark_picker_other_response_leaves_document_unchanged,
                    test_watermark_cancel_without_uris_keeps_later_dialogs_usable,
                    test_watermark_error_without_uris_keeps_later_dialogs_usable,
                ):
                    driver.close()
                    driver = None
                    stop(app)
                    app = None
                    await wait_for(lambda: window_info(env) is None, "the previous editor to close")
                    portal.save_destinations.clear()
                    app = subprocess.Popen(
                        [str(binary), str(source)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                        stdout=app_log, stderr=app_log,
                    )
                    await wait_for(lambda: window_info(env), "a fresh editor", 12)
                    window, origin = window_info(env)
                    driver = X11Driver(display, window, origin)
                    await wait_for(
                        lambda: count_canvas_color(driver, (30, 120, 190)) > 5000,
                        "the source image in the fresh editor",
                    )
                    await show_effects(driver)
                    fresh_baseline = await export_image(driver, portal, work)
                    try:
                        await test(driver, portal, work, fresh_baseline)
                    except AssertionError as error:
                        trace_window(driver, f"interactive-effects-{test.__name__}")
                        failures.append(f"{test.__name__}: {error}")
                        print(f"FAIL {failures[-1]}", file=sys.stderr, flush=True)
                    else:
                        print(f"PASS {test.__name__}", flush=True)
                assert not failures, "\n".join(failures)
                print("GUI interactive effects, portal errors, undo, and stale-result guards passed")
        except BaseException:
            if driver is not None:
                trace_window(driver, "interactive-effects-failure")
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
    arguments = parser.parse_args()
    asyncio.run(main(arguments.binary.resolve(strict=True), arguments.runtime_dir.resolve(strict=True)))
