#!/usr/bin/env python3
"""Verify XDG login startup and tray fallback in a private desktop session."""

import argparse
import asyncio
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio

from linux_load_e2e import window_info
from linux_shortcuts_e2e import MockPortal, X11Driver, stop, wait_for
from linux_tray_e2e import MockWatcher, request_window_close
from linux_webcam_e2e import scroll, trace_window


def running(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1][0] != "Z"
    except FileNotFoundError:
        return False


async def main(binary, runtime):
    portal = MockPortal()
    await portal.start()
    watcher = None
    app = server = driver = None
    launched_pid = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-autostart-ui-") as directory:
        work = Path(directory)
        xdg_runtime = work / "runtime"
        xdg_runtime.mkdir(mode=0o700)
        config = work / "config" / "Jiffshot"
        config.mkdir(parents=True)
        settings = config / "settings.json"
        settings.write_text(json.dumps({"theme": "Light", "linux_tray_enabled": False}))
        entry = work / "config" / "autostart" / "com.devolutions.Jiffshot.desktop"
        marker = work / "launched.json"
        launcher = work / 'start $`"percent%\\ space'
        launcher.write_text(
            "#!/usr/bin/python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            f"Path({str(marker)!r}).write_text(json.dumps([os.getpid(), sys.argv[1:]]))\n"
            f"os.execv({str(binary)!r}, [{str(binary)!r}, *sys.argv[1:]])\n"
        )
        launcher.chmod(0o755)
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
                    "JIFFSHOT_LAUNCHER": str(launcher),
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

                async def launch_entry():
                    marker.unlink(missing_ok=True)
                    try:
                        desktop = Gio.DesktopAppInfo.new_from_filename(str(entry))
                    except TypeError as error:
                        raise AssertionError(
                            f"GLib rejected the desktop entry: {entry.read_text()!r}"
                        ) from error
                    assert desktop is not None, "GLib must parse the enabled desktop entry"
                    assert not desktop.get_is_hidden()
                    context = Gio.AppLaunchContext()
                    for name, value in env.items():
                        context.setenv(name, value)
                    context.unsetenv("WAYLAND_DISPLAY")
                    assert desktop.launch([], context), "GLib must launch the desktop entry"
                    await wait_for(marker.exists, "the autostart executable", 10)
                    pid, arguments = json.loads(marker.read_text())
                    assert arguments == ["--background"], arguments
                    return pid

                app = start_app()
                driver = await attach()
                await asyncio.sleep(0.5)
                scroll(driver, "down")
                await asyncio.sleep(0.5)
                driver.click(280, 48)
                await wait_for(entry.exists, "the enabled startup entry")
                subprocess.run(["desktop-file-validate", str(entry)], check=True)
                assert "X-Jiffshot-Autostart=true\n" in entry.read_text()
                request_window_close(driver)
                await wait_for(lambda: app.poll() is not None, "the initial editor to close")
                driver.close()
                driver = None
                app = None

                launched_pid = await launch_entry()
                driver = await attach()
                assert running(launched_pid)
                request_window_close(driver)
                await wait_for(lambda: not running(launched_pid), "close without an enabled tray")
                launched_pid = None
                driver.close()
                driver = None
                print("Desktop startup preserves a quoted launcher and opens without tray opt-in")

                settings.write_text(json.dumps({"theme": "Light", "linux_tray_enabled": True}))
                launched_pid = await launch_entry()
                driver = await attach()
                await asyncio.sleep(1.5)
                assert window_info(env), "a missing watcher must not hide an autostarted editor"
                request_window_close(driver)
                await wait_for(lambda: not running(launched_pid), "close without a tray watcher")
                launched_pid = None
                driver.close()
                driver = None
                print("Login startup remains visible when the tray is unavailable")

                watcher = MockWatcher()
                await watcher.start()
                settings.write_text(json.dumps({
                    "theme": "Light",
                    "linux_tray_enabled": True,
                    "linux_global_shortcuts_enabled": True,
                    "global_capture_shortcut": "CTRL+ALT+P",
                    "global_recording_shortcuts": True,
                }))
                launched_pid = await launch_entry()
                await wait_for(
                    lambda: watcher.items and watcher.host_queries >= 2,
                    "the autostarted tray registration",
                )
                await wait_for(lambda: portal.bindings, "shortcuts restored at login")
                shortcut, bindings = portal.bindings[-1]
                assert [binding[0] for binding in bindings] == [
                    "capture", "pause-recording", "stop-recording",
                ], bindings
                assert bindings[0][1]["preferred_trigger"].value == "CTRL+ALT+P"
                await wait_for(lambda: not window_info(env), "login startup to hide in the tray")
                assert running(launched_pid), "background startup must keep the app running"
                previous = portal.capture_requests
                portal.activate(shortcut, "capture")
                await wait_for(
                    lambda: portal.capture_requests > previous,
                    "a restored shortcut to activate while hidden",
                )
                await asyncio.sleep(0.6)
                item = watcher.items[-1]
                await watcher.activate(item)
                driver = await attach()
                menu = await watcher.menu(item)
                await watcher.click_menu(item, menu["Quit Jiffshot"])
                await wait_for(lambda: not running(launched_pid), "the autostarted app to quit")
                launched_pid = None
                driver.close()
                driver = None
                print("Login startup restores shortcuts and hides only after tray registration")

                previous_items = len(watcher.items)
                app = start_app()
                driver = await attach()
                await wait_for(
                    lambda: len(watcher.items) > previous_items, "the normal launch tray item"
                )
                await asyncio.sleep(0.5)
                scroll(driver, "down")
                await asyncio.sleep(0.5)
                driver.click(440, 48)
                await wait_for(lambda: "Hidden=true\n" in entry.read_text(), "startup to be disabled")
                subprocess.run(["desktop-file-validate", str(entry)], check=True)
                desktop = Gio.DesktopAppInfo.new_from_filename(str(entry))
                assert desktop is not None and desktop.get_is_hidden()
                entry.write_text("unmanaged startup configuration\n")
                driver.click(280, 48)
                await asyncio.sleep(0.5)
                assert entry.read_text() == "unmanaged startup configuration\n"
                menu = await watcher.menu(watcher.items[-1])
                await watcher.click_menu(watcher.items[-1], menu["Quit Jiffshot"])
                await wait_for(lambda: app.poll() is not None, "the normal launch to quit")
                app = None
                driver.close()
                driver = None
                print("Disabling startup suppresses autostart and leaves unmanaged entries untouched")
        except BaseException:
            if driver is not None and window_info(env):
                trace_window(driver, "autostart-failure")
            print((work / "xvfb.log").read_text(errors="replace")[-12000:], file=sys.stderr)
            print((work / "app.log").read_text(errors="replace")[-12000:], file=sys.stderr)
            raise
        finally:
            if driver is not None:
                driver.close()
            stop(app)
            if launched_pid is not None and running(launched_pid):
                os.kill(launched_pid, signal.SIGTERM)
                await wait_for(lambda: not running(launched_pid), "autostart cleanup")
            stop(server)
            if read_fd is not None:
                os.close(read_fd)
            if write_fd is not None:
                os.close(write_fd)
            if watcher is not None:
                watcher.disconnect()
            portal.bus.disconnect()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--runtime-dir", type=Path, required=True)
    arguments = parser.parse_args()
    asyncio.run(main(arguments.binary.resolve(), arguments.runtime_dir.resolve()))
