#!/usr/bin/env python3
"""Exercise the desktop screenshot chooser and stale-result protection."""

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


async def main(binary, runtime):
    portal = MockPortal()
    await portal.start()
    app = server = driver = None
    read_fd = write_fd = None
    with tempfile.TemporaryDirectory(prefix="jiffshot-screenshot-ui-") as directory:
        work = Path(directory)
        xdg_runtime = work / "runtime"
        xdg_runtime.mkdir(mode=0o700)
        config = work / "config" / "Jiffshot"
        config.mkdir(parents=True)
        captures = work / "captures"
        captures.mkdir()
        (config / "settings.json").write_text(json.dumps({
            "theme": "Light",
            "capture_directory": str(captures),
        }))
        source = work / "desktop screenshot.png"
        source.write_bytes(solid_png(255, 0, 0))
        portal.screenshot_uri = source.as_uri()
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
                for mode in ("success", "hold", "cancel", "remote"):
                    portal.screenshot_mode = "hold" if mode == "hold" else (
                        "cancel" if mode == "cancel" else "success"
                    )
                    portal.screenshot_uri = (
                        "https://example.invalid/screenshot.png" if mode == "remote"
                        else source.as_uri()
                    )
                    expected_requests = len(portal.screenshot_requests) + 1
                    before_files = set(captures.iterdir())
                    app = subprocess.Popen(
                        [str(binary), "--capture-interactive"], env=env, cwd=work,
                        stdin=subprocess.DEVNULL, stdout=app_log, stderr=app_log,
                    )
                    await wait_for(lambda: window_info(env), f"the editor for {mode}", 12)
                    window, origin = window_info(env)
                    driver = X11Driver(display_name, window, origin)
                    await wait_for(
                        lambda: len(portal.screenshot_requests) == expected_requests,
                        f"the screenshot request for {mode}",
                    )
                    request, options = portal.screenshot_requests[-1]
                    assert options["interactive"].value is True, options
                    assert options["modal"].value is True, options
                    assert portal.capture_requests == 0, (
                        "interactive screenshots must use the Screenshot portal"
                    )
                    await asyncio.sleep(0.5)
                    if mode == "hold":
                        draw_arrow(driver)
                        await wait_for(
                            lambda: driver.red_canvas_pixels() > 40,
                            "an annotation while the screenshot chooser is open",
                        )
                        before_pixels = driver.red_canvas_pixels()
                        portal.response(request, 0, {"uri": Variant("s", source.as_uri())})
                    if mode in ("success", "hold"):
                        await wait_for(
                            lambda: len(set(captures.iterdir()) - before_files) == 1,
                            f"the saved screenshot for {mode}",
                        )
                        if mode == "success":
                            await wait_for(
                                lambda: driver.red_canvas_pixels() > 5000,
                                "the chosen screenshot in the editor",
                            )
                        else:
                            await asyncio.sleep(2)
                            after_pixels = driver.red_canvas_pixels()
                            assert before_pixels * 0.8 <= after_pixels <= before_pixels * 1.5 + 50, (
                                f"a late screenshot replaced newer edits: {before_pixels}, {after_pixels}"
                            )
                    else:
                        await asyncio.sleep(1)
                        assert set(captures.iterdir()) == before_files, (
                            f"{mode} must not create a capture"
                        )
                        assert app.poll() is None, f"app exited after {mode}"
                    driver.close()
                    driver = None
                    stop(app)
                    app = None
                print("Interactive screenshots saved and loaded, preserved edits, and rejected invalid results")
        except BaseException:
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
            portal.bus.disconnect()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--runtime-dir", type=Path, required=True)
    arguments = parser.parse_args()
    asyncio.run(main(arguments.binary.resolve(strict=True), arguments.runtime_dir.resolve(strict=True)))
