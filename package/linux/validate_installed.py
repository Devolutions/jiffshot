#!/usr/bin/env python3
"""Assert the architecture of every installed private runtime ELF before smoke testing."""

import argparse
from pathlib import Path

from build import ARCHITECTURES, validate_build


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=tuple(ARCHITECTURES), required=True)
    parser.add_argument("--runtime-dir", type=Path, default=Path("/usr/lib/jiffshot"))
    args = parser.parse_args()
    print(f"Installed runtime: Linux {validate_build(args.runtime_dir, args.arch).name}")


if __name__ == "__main__":
    main()
