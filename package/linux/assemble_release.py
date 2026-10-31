#!/usr/bin/env python3
"""Require and verify all six Linux packages before adding them to a release."""

import argparse
import hashlib
from pathlib import Path
import re
import shutil

from build import package_version


def read_checksums(path, expected_names):
    records = {}
    for line in path.read_text(encoding="ascii").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([^/\\]+)", line)
        if match is None:
            raise ValueError(f"invalid checksum record in {path}")
        digest, name = match.groups()
        if name in records:
            raise ValueError(f"duplicate checksum for {name} in {path}")
        records[name] = digest
    if set(records) != set(expected_names):
        raise ValueError(f"{path} must cover exactly {', '.join(expected_names)}")
    return records


def assemble(artifact_dir, output_dir, version):
    version = package_version(version)
    verified = []
    merged = {}
    for arch, deb, rpm in (("x64", "amd64", "x86_64"), ("arm64", "arm64", "aarch64")):
        records = {}
        for suffix, names in (
            ("", (f"Devolutions.Jiffshot-linux-{arch}-{version}.tar.gz", f"jiffshot_{version}_{deb}.deb")),
            ("-rpm", (f"jiffshot-{version}-1.{rpm}.rpm",)),
        ):
            directory = artifact_dir / f"jiffshot-linux-{arch}{suffix}"
            checksums = read_checksums(directory / f"SHA256SUMS-linux-{arch}.txt", names)
            for name in names:
                source = directory / name
                with source.open("rb") as content:
                    actual = hashlib.file_digest(content, "sha256").hexdigest()
                if actual != checksums[name]:
                    raise ValueError(f"checksum mismatch for {source}")
                verified.append(source)
                records[name] = actual
        merged[arch] = records
    # No output is touched until all architectures, formats, and hashes pass.
    output_dir.mkdir(parents=True, exist_ok=True)
    for source in verified:
        shutil.copyfile(source, output_dir / source.name)
    for arch, records in merged.items():
        (output_dir / f"SHA256SUMS-linux-{arch}.txt").write_text(
            "".join(f"{digest}  {name}\n" for name, digest in sorted(records.items())),
            encoding="ascii",
        )
    return [source.name for source in verified]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    for name in assemble(args.artifact_dir, args.output_dir, args.version):
        print(name)


if __name__ == "__main__":
    main()
