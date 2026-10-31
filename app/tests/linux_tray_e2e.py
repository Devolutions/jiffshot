#!/usr/bin/env python3
"""Verify tray lifecycle, fallback, and background capture in private X11/D-Bus sessions."""

import argparse
import asyncio
import ctypes
import datetime
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile

from dbus_next import Message, MessageType, Variant
from dbus_next.aio import MessageBus
from linux_load_e2e import window_info
from linux_schedule_e2e import safe_current_minute
from linux_shortcuts_e2e import MockPortal, X11Driver, stop, wait_for
from linux_webcam_e2e import scroll, trace_window


WATCHER = "org.kde.StatusNotifierWatcher"
ITEM = "org.kde.StatusNotifierItem"
MENU = "com.canonical.dbusmenu"
NOTIFICATIONS = "org.freedesktop.Notifications"


class MockNotifications:
    def __init__(self):
        self.bus = None
        self.messages = []
        self.responsive = True

    async def start(self):
        self.bus = await MessageBus().connect()
        self.bus.add_message_handler(self.message)
        await self.bus.request_name(NOTIFICATIONS)

    def message(self, message):
        if (message.message_type == MessageType.METHOD_CALL
                and message.interface == NOTIFICATIONS and message.member == "Notify"):
            self.messages.append(message.body)
            if not self.responsive:
                return True
            return Message.new_method_return(message, "u", [len(self.messages)])
        return None

    def disconnect(self):
        if self.bus is not None:
            self.bus.disconnect()
            self.bus = None


class MockWatcher:
    def __init__(self, host=True):
        self.host = host
        self.unresponsive = False
        self.bus = None
        self.items = []
        self.host_queries = 0

    async def start(self):
        self.bus = await MessageBus().connect()
        self.bus.add_message_handler(self.message)
        await self.bus.request_name(WATCHER)

    def message(self, message):
        if message.message_type != MessageType.METHOD_CALL:
            return None
        if message.interface == WATCHER and message.member == "RegisterStatusNotifierItem":
            self.items.append(message.body[0])
            return Message.new_method_return(message)
        if message.interface == "org.freedesktop.DBus.Properties":
            values = {
                "IsStatusNotifierHostRegistered": Variant("b", self.host),
                "RegisteredStatusNotifierItems": Variant("as", self.items),
                "ProtocolVersion": Variant("i", 0),
            }
            if message.member == "Get" and message.body[0] == WATCHER:
                self.host_queries += message.body[1] == "IsStatusNotifierHostRegistered"
                if self.unresponsive:
                    return True
                return Message.new_method_return(message, "v", [values[message.body[1]]])
            if message.member == "GetAll" and message.body[0] == WATCHER:
                self.host_queries += 1
                return Message.new_method_return(message, "a{sv}", [values])
        return None

    async def call(self, item, path, interface, member, signature="", body=None):
        reply = await self.bus.call(Message(
            destination=item, path=path, interface=interface, member=member,
            signature=signature, body=body or [],
        ))
        if reply.message_type == MessageType.ERROR:
            raise AssertionError(f"{interface}.{member} failed: {reply.body}")
        return reply.body

    async def activate(self, item):
        await self.call(item, "/StatusNotifierItem", ITEM, "Activate", "ii", [0, 0])

    async def menu(self, item):
        _revision, layout = await self.call(
            item, "/MenuBar", MENU, "GetLayout", "iias", [0, -1, []]
        )
        return {
            entry.value[1]["label"].value: entry.value[0]
            for entry in layout[2]
            if "label" in entry.value[1]
        }

    async def click_menu(self, item, identifier):
        await self.call(
            item, "/MenuBar", MENU, "Event", "isvu",
            [identifier, "clicked", Variant("i", 0), 0],
        )

    def disconnect(self):
        if self.bus is not None:
            self.bus.disconnect()
            self.bus = None


class ClientMessageData(ctypes.Union):
    _fields_ = [
        ("b", ctypes.c_byte * 20),
        ("s", ctypes.c_short * 10),
        ("l", ctypes.c_long * 5),
    ]


class ClientMessageEvent(ctypes.Structure):
    _fields_ = [
        ("type", ctypes.c_int),
        ("serial", ctypes.c_ulong),
        ("send_event", ctypes.c_int),
        ("display", ctypes.c_void_p),
        ("window", ctypes.c_ulong),
        ("message_type", ctypes.c_ulong),
        ("format", ctypes.c_int),
        ("data", ClientMessageData),
    ]


class XEvent(ctypes.Union):
    _fields_ = [("client", ClientMessageEvent), ("pad", ctypes.c_long * 24)]


def request_window_close(driver):
    x11 = driver.x11
    x11.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
    x11.XInternAtom.restype = ctypes.c_ulong
    x11.XSendEvent.argtypes = [
        ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_long, ctypes.POINTER(XEvent)
    ]
    x11.XSendEvent.restype = ctypes.c_int
    event = XEvent()
    event.client.type = 33
    event.client.display = driver.display
    event.client.window = driver.window
    event.client.message_type = x11.XInternAtom(driver.display, b"WM_PROTOCOLS", 0)
    event.client.format = 32
    event.client.data.l[0] = x11.XInternAtom(driver.display, b"WM_DELETE_WINDOW", 0)
    if not x11.XSendEvent(driver.display, driver.window, 0, 0, ctypes.byref(event)):
        raise RuntimeError("could not send the window close request")
    x11.XSync(driver.display, 0)


async def main(binary, runtime):
    portal = MockPortal()
    await portal.start()
    notifications = MockNotifications()
    await notifications.start()
    watcher = None
    app = server = driver = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-tray-ui-") as directory:
        work = Path(directory)
        xdg_runtime = work / "runtime"
        xdg_runtime.mkdir(mode=0o700)
        config = work / "config" / "Jiffshot"
        config.mkdir(parents=True)
        settings = config / "settings.json"
        captures = work / "captures"
        captures.mkdir()
        original = {
            "theme": "Light",
            "capture_directory": str(captures),
            "linux_tray_enabled": True,
            "other": {"keep": True},
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
                display = ":" + os.read(read_fd, 128).decode("ascii").strip()
                env = {
                    **os.environ,
                    "DISPLAY": display,
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

                def start_app():
                    return subprocess.Popen(
                        [str(binary)], env=env, cwd=work, stdin=subprocess.DEVNULL,
                        stdout=app_log, stderr=app_log,
                    )

                async def attach():
                    await wait_for(lambda: window_info(env), "the editor", 12)
                    window, origin = window_info(env)
                    return X11Driver(display, window, origin)

                app = start_app()
                driver = await attach()
                await asyncio.sleep(1.5)
                request_window_close(driver)
                await wait_for(lambda: app.poll() is not None, "normal close without a watcher")
                assert json.loads(settings.read_text()) == original
                driver.close()
                driver = None
                print("Missing watcher keeps the editor normally closable")

                watcher = MockWatcher(host=False)
                await watcher.start()
                app = start_app()
                driver = await attach()
                await wait_for(lambda: watcher.items and watcher.host_queries, "a hostless watcher")
                await asyncio.sleep(0.8)
                request_window_close(driver)
                await wait_for(lambda: app.poll() is not None, "normal close without a tray host")
                assert json.loads(settings.read_text()) == original
                driver.close()
                driver = None
                print("A watcher without a tray host does not hide the editor")

                settings.write_text(json.dumps({**original, "linux_tray_enabled": False}))
                previous_items = len(watcher.items)
                previous_queries = watcher.host_queries
                app = start_app()
                driver = await attach()
                await asyncio.sleep(0.5)
                scroll(driver, "down")
                await asyncio.sleep(0.5)
                driver.click(50, 48)
                await wait_for(
                    lambda: len(watcher.items) > previous_items
                    and watcher.host_queries > previous_queries,
                    "a rejected attempt to enable the tray",
                )
                await asyncio.sleep(0.4)
                assert json.loads(settings.read_text()) == {
                    **original, "linux_tray_enabled": False,
                }, "an unavailable tray must not be persisted as enabled"
                request_window_close(driver)
                await wait_for(lambda: app.poll() is not None, "close after a failed tray enable")
                driver.close()
                driver = None

                watcher.host = True
                previous_items = len(watcher.items)
                previous_queries = watcher.host_queries
                app = start_app()
                driver = await attach()
                await asyncio.sleep(0.5)
                scroll(driver, "down")
                await asyncio.sleep(0.5)
                driver.click(50, 48)
                await wait_for(
                    lambda: len(watcher.items) > previous_items
                    and watcher.host_queries >= previous_queries + 2
                    and json.loads(settings.read_text()).get("linux_tray_enabled") is True,
                    "manually enabling and persisting the tray",
                )
                item = watcher.items[-1]
                request_window_close(driver)
                await wait_for(lambda: not window_info(env), "manual close to the tray")
                await watcher.activate(item)
                await wait_for(lambda: window_info(env), "manual tray restoration")
                scroll(driver, "down")
                await asyncio.sleep(0.4)
                driver.click(150, 48)
                await wait_for(
                    lambda: json.loads(settings.read_text()).get("linux_tray_enabled") is False,
                    "manually disabling and persisting the tray",
                )
                assert json.loads(settings.read_text())["other"] == {"keep": True}
                request_window_close(driver)
                await wait_for(lambda: app.poll() is not None, "normal close after disabling the tray")
                driver.close()
                driver = None
                print("GUI enable and disable persist without changing other settings")

                now = await safe_current_minute()
                scheduled = (now + datetime.timedelta(minutes=1)).strftime("%H:%M")
                settings.write_text(json.dumps({
                    **original, "schedule": scheduled, "schedule_default": scheduled,
                }))
                previous_items = len(watcher.items)
                previous_queries = watcher.host_queries
                app = start_app()
                driver = await attach()
                await wait_for(
                    lambda: len(watcher.items) > previous_items
                    and watcher.host_queries >= previous_queries + 2,
                    "registered tray item with a host",
                )
                await asyncio.sleep(0.6)
                item = watcher.items[-1]
                pixmap = await watcher.call(
                    item, "/StatusNotifierItem", "org.freedesktop.DBus.Properties",
                    "Get", "ss", [ITEM, "IconPixmap"],
                )
                assert pixmap[0].value[0][:2] == [32, 32], pixmap
                assert len(pixmap[0].value[0][2]) == 32 * 32 * 4
                menu = await watcher.menu(item)
                assert set(menu) == {
                    "Open Jiffshot", "Capture screen or window", "Quit Jiffshot",
                }, menu
                driver.click(460, 80)
                await wait_for(lambda: portal.bindings, "the capture shortcut to bind")
                shortcut = portal.bindings[-1][0]
                await asyncio.sleep(0.4)
                request_window_close(driver)
                await wait_for(lambda: not window_info(env), "the window to hide in the tray")
                assert app.poll() is None, "closing to tray must not quit"

                previous = portal.capture_requests
                portal.activate(shortcut, "capture")
                await wait_for(
                    lambda: portal.capture_requests > previous, "a shortcut while hidden"
                )
                await wait_for(
                    lambda: any(message[3] == "Capture failed" for message in notifications.messages),
                    "the background capture failure notification",
                )
                notification = next(
                    message for message in notifications.messages if message[3] == "Capture failed"
                )
                assert notification[0] == "Jiffshot"
                assert "Mock capture is unavailable" in notification[4], notification
                assert notification[6]["urgency"].value == 2, notification
                assert window_info(env) is None, "a background shortcut should not restore the editor"
                await watcher.activate(item)
                await wait_for(lambda: window_info(env), "tray activation to restore the editor")
                previous = portal.capture_requests
                previous_notifications = len(notifications.messages)
                portal.activate(shortcut, "capture")
                await wait_for(
                    lambda: portal.capture_requests > previous, "a shortcut while visible"
                )
                await asyncio.sleep(0.7)
                assert len(notifications.messages) == previous_notifications, (
                    "visible capture errors should remain in the editor rather than notifying"
                )
                request_window_close(driver)
                await wait_for(lambda: not window_info(env), "the restored editor to hide again")

                notifications.responsive = False
                previous = portal.capture_requests
                previous_notifications = len(notifications.messages)
                portal.activate(shortcut, "capture")
                await wait_for(
                    lambda: portal.capture_requests > previous
                    and len(notifications.messages) > previous_notifications,
                    "the unresponsive notification request",
                )
                await wait_for(
                    lambda: "the notification service did not respond" in (
                        work / "app.log"
                    ).read_text(errors="replace"),
                    "the bounded notification timeout",
                    8,
                )
                assert app.poll() is None and not window_info(env)
                notifications.responsive = True

                portal.screen_mode = "hold-start"
                previous_starts = len(portal.screen_starts)
                await wait_for(
                    lambda: len(portal.screen_starts) > previous_starts,
                    "the daily scheduled capture while hidden", 80,
                )
                scheduled_session, scheduled_request = portal.screen_starts[-1]
                assert window_info(env) is None, "daily capture must not open the editor"
                assert json.loads(settings.read_text())["linux_schedule_last_fired"] == (
                    datetime.datetime.now().astimezone().strftime("%Y-%j")
                )
                portal.response(scheduled_request, 1, {})
                await wait_for(lambda: scheduled_session in portal.closes, "scheduled cancellation")
                await wait_for(
                    lambda: any(
                        message[3] == "Daily capture failed"
                        for message in notifications.messages
                    ),
                    "the scheduled capture failure notification",
                )
                await asyncio.sleep(0.3)

                watcher.host = False
                await wait_for(lambda: window_info(env), "the editor after its tray host disappears", 8)
                assert app.poll() is None
                assert json.loads(settings.read_text())["linux_tray_enabled"] is True
                request_window_close(driver)
                await wait_for(lambda: app.poll() is not None, "normal close after host loss")
                driver.close()
                driver = None
                print("Tray restore, shortcuts, scheduled capture, and host-loss fallback worked")

                watcher.host = True
                previous_items = len(watcher.items)
                previous_queries = watcher.host_queries
                app = start_app()
                driver = await attach()
                await wait_for(
                    lambda: len(watcher.items) > previous_items
                    and watcher.host_queries >= previous_queries + 2,
                    "tray registration before the host stops responding",
                )
                await asyncio.sleep(0.5)
                request_window_close(driver)
                await wait_for(lambda: not window_info(env), "the editor hidden before a host timeout")
                watcher.unresponsive = True
                await wait_for(lambda: window_info(env), "the editor after a host timeout", 10)
                assert app.poll() is None
                watcher.unresponsive = False
                request_window_close(driver)
                await wait_for(lambda: app.poll() is not None, "normal close after a host timeout")
                driver.close()
                driver = None
                print("An unresponsive tray host restores the editor")

                previous_items = len(watcher.items)
                previous_queries = watcher.host_queries
                app = start_app()
                driver = await attach()
                await wait_for(
                    lambda: len(watcher.items) > previous_items
                    and watcher.host_queries >= previous_queries + 2,
                    "tray registration after restart",
                )
                await asyncio.sleep(0.5)
                item = watcher.items[-1]
                menu = await watcher.menu(item)
                request_window_close(driver)
                await wait_for(lambda: not window_info(env), "the editor hidden before tray actions")
                portal.screen_mode = "hold-start"
                previous_starts = len(portal.screen_starts)
                await watcher.click_menu(item, menu["Capture screen or window"])
                await wait_for(lambda: window_info(env), "the tray capture to restore the editor")
                await wait_for(
                    lambda: len(portal.screen_starts) > previous_starts,
                    "the tray capture chooser",
                )
                capture_session, capture_request = portal.screen_starts[-1]
                request_window_close(driver)
                await wait_for(lambda: not window_info(env), "the editor hidden during capture")
                await watcher.click_menu(item, menu["Quit Jiffshot"])
                await wait_for(lambda: window_info(env), "quit to restore an active capture")
                assert app.poll() is None, "quit must not discard an active capture"
                portal.response(capture_request, 1, {})
                await wait_for(lambda: capture_session in portal.closes, "the cancelled tray capture")
                await asyncio.sleep(0.5)
                request_window_close(driver)
                await wait_for(lambda: not window_info(env), "the editor hidden before menu restore")
                await watcher.click_menu(item, menu["Open Jiffshot"])
                await wait_for(lambda: window_info(env), "the tray menu to restore the editor")
                request_window_close(driver)
                await wait_for(lambda: not window_info(env), "the editor hidden before quit")
                await watcher.click_menu(item, menu["Quit Jiffshot"])
                await wait_for(lambda: app.poll() is not None, "the tray menu to quit")
                driver.close()
                driver = None
                print("Tray menu capture, restore, and safe quit worked")

                previous_items = len(watcher.items)
                previous_queries = watcher.host_queries
                app = start_app()
                driver = await attach()
                await wait_for(
                    lambda: len(watcher.items) > previous_items
                    and watcher.host_queries >= previous_queries + 2,
                    "tray registration before watcher loss",
                )
                await asyncio.sleep(0.5)
                request_window_close(driver)
                await wait_for(lambda: not window_info(env), "the editor hidden before watcher loss")
                watcher.disconnect()
                await wait_for(lambda: window_info(env), "the editor after its watcher disappears", 8)
                request_window_close(driver)
                await wait_for(lambda: app.poll() is not None, "normal close after watcher loss")
                driver.close()
                driver = None
                print("Watcher loss restores the hidden editor")
        except BaseException:
            if driver is not None and window_info(env):
                trace_window(driver, "tray-failure")
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
            if watcher is not None:
                watcher.disconnect()
            notifications.disconnect()
            portal.bus.disconnect()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--runtime-dir", type=Path, required=True)
    arguments = parser.parse_args()
    asyncio.run(main(arguments.binary.resolve(), arguments.runtime_dir.resolve()))
