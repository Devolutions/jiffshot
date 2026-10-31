#!/usr/bin/env python3
"""Discover and run the fixed-rate tests inside a real portal fixture."""

import argparse
from pathlib import Path
import re
import subprocess


EXPECTED = {
    "portal_fixed_rate_gui_restart_and_command_line_use_saved_rate",
    "portal_fixed_rate_pause_preserves_video_webcam_and_virtual_audio",
    "portal_fixed_rate_restart_keeps_original_choice_and_discards_old_segment",
}


def run(command):
    print("+", " ".join(map(str, command)), flush=True)
    result = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    print(result.stdout, end="", flush=True)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    arguments = parser.parse_args()
    binary = arguments.binary.resolve(strict=True)
    listing = run([binary, "portal_fixed_rate_", "--list"])
    if listing.returncode:
        raise SystemExit(listing.returncode)
    names = re.findall(r"(?m)^([a-zA-Z0-9_:]+): test$", listing.stdout)
    actual = [name.rsplit("::", 1)[-1] for name in names]
    if len(actual) != len(EXPECTED) or set(actual) != EXPECTED:
        raise RuntimeError(f"unexpected portal test discovery: {actual!r}")
    print(f"Verified discovery of {len(actual)} real-portal tests.", flush=True)
    result = run([binary, "portal_fixed_rate_", "--ignored", "--nocapture", "--test-threads=1"])
    summary = re.search(
        r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; "
        r"(\d+) ignored; (\d+) measured; (\d+) filtered out",
        result.stdout,
    )
    if not summary:
        raise RuntimeError("portal test run has no parseable summary")
    passed, failed, ignored = map(int, summary.group(2, 3, 4))
    if passed + failed + ignored != len(EXPECTED) or ignored:
        raise RuntimeError(
            f"incomplete portal result: {passed} passed, {failed} failed, {ignored} ignored"
        )
    if result.returncode or failed:
        raise SystemExit(result.returncode or 1)


if __name__ == "__main__":
    main()
