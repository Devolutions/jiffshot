#!/usr/bin/env python3
"""Package a Linux release build with its private Rustolonia runtime."""

import argparse
import gzip
import hashlib
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[2]
ASSETS = Path(__file__).resolve().parent
RUNTIME_FILES = (
    "jiffshot",
    "librustolonia_host.so",
    "libSkiaSharp.so",
    "libHarfBuzzSharp.so",
)
MAINTAINER = "Devolutions <service@devolutions.net>"
DEB_RUNTIME = (
    "libx11-6", "libice6", "libsm6", "libxcursor1", "libxi6", "libxrandr2",
    "libgtk-3-0t64 | libgtk-3-0",
    "libicu74 | libicu76", "libssl3t64 | libssl3",
    "fontconfig", "fonts-dejavu-core", "tesseract-ocr-eng",
    "gstreamer1.0-plugins-base (>= 1.24)", "gstreamer1.0-plugins-good",
    "gstreamer1.0-plugins-bad", "gstreamer1.0-plugins-ugly",
    "gstreamer1.0-libav", "gstreamer1.0-pipewire", "gstreamer1.0-pulseaudio",
    "desktop-file-utils", "shared-mime-info", "hicolor-icon-theme",
)


def run(command, **kwargs):
    return subprocess.run(command, check=True, capture_output=True, text=True, **kwargs)


def package_version(expected=None):
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["workspace"]["package"]["version"]
    if not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", version):
        raise ValueError("Linux packages require an X.Y.Z workspace release version")
    if expected is not None and expected != version:
        raise ValueError(f"requested version {expected!r} does not match Cargo.toml ({version})")
    return version


def validate_binary(path):
    with path.open("rb") as binary:
        header = binary.read(20)
    if len(header) != 20 or header[:6] != b"\x7fELF\x02\x01":
        raise ValueError(f"{path} is not a little-endian 64-bit Linux ELF binary")
    if struct.unpack_from("<H", header, 18)[0] != 62:
        raise ValueError(f"{path} is not Linux x64; Linux packages currently support x64 only")


def require_tools(formats):
    tools = {"strip", "desktop-file-validate"}
    if "deb" in formats:
        tools.update(("dpkg-deb", "dpkg-shlibdeps"))
    if "rpm" in formats:
        tools.update(("rpmbuild", "readelf"))
    missing = sorted(tool for tool in tools if shutil.which(tool) is None)
    if missing:
        raise ValueError(f"missing Linux packaging tools: {', '.join(missing)}; see package/linux/README.txt")


def validate_rpm_input(build_dir):
    dependencies = run(["readelf", "--dynamic", str(build_dir / "jiffshot")]).stdout
    if "[libleptonica.so.6]" not in dependencies:
        raise ValueError(
            "RPM requires a native Fedora build linked to libleptonica.so.6; "
            "an Ubuntu 24.04 build linked to liblept.so.5 cannot be repackaged for Fedora"
        )


def prepare_payload(build_dir, payload):
    runtime = payload / "lib/jiffshot"
    runtime.mkdir(parents=True)
    for name in RUNTIME_FILES:
        source = build_dir / name
        validate_binary(source)
        destination = runtime / name
        shutil.copyfile(source, destination)
        destination.chmod(0o755)
        run(["strip", "--strip-debug", str(destination)])
    files = (
        (ASSETS / "jiffshot", "bin/jiffshot", 0o755),
        (ASSETS / "com.devolutions.Jiffshot.desktop", "share/applications/com.devolutions.Jiffshot.desktop", 0o644),
        (ROOT / "assets/jiffshot.svg", "share/icons/hicolor/scalable/apps/com.devolutions.Jiffshot.svg", 0o644),
        (ASSETS / "com.devolutions.Jiffshot.xml", "share/mime/packages/com.devolutions.Jiffshot.xml", 0o644),
        (ASSETS / "README.txt", "share/doc/jiffshot/README-linux.txt", 0o644),
        (ROOT / "README.md", "share/doc/jiffshot/README.md", 0o644),
    )
    for source, relative, mode in files:
        destination = payload / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(source.read_text(encoding="utf-8"), encoding="utf-8", newline="\n")
        destination.chmod(mode)
    for directory in (payload, *(path for path in payload.rglob("*") if path.is_dir())):
        directory.chmod(0o755)
    run(["desktop-file-validate", str(payload / "share/applications/com.devolutions.Jiffshot.desktop")])


def write_archive(payload, destination, top_directory, epoch):
    with destination.open("wb") as output:
        with gzip.GzipFile(fileobj=output, filename="", mode="wb", mtime=epoch, compresslevel=9) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive:
                for path in (payload, *sorted(payload.rglob("*"))):
                    relative = path.relative_to(payload)
                    name = str(Path(top_directory) / relative)
                    info = archive.gettarinfo(str(path), arcname=name)
                    info.uid = info.gid = 0
                    info.uname = info.gname = "root"
                    info.mtime = epoch
                    info.mode = 0o755 if path.is_dir() or path.stat().st_mode & 0o111 else 0o644
                    if path.is_file():
                        with path.open("rb") as content:
                            archive.addfile(info, content)
                    else:
                        archive.addfile(info)


def build_deb(payload, work, version, env):
    root = work / "deb"
    shutil.copytree(payload, root / "usr")
    root.chmod(0o755)
    metadata = work / "metadata/debian"
    metadata.mkdir(parents=True)
    (metadata / "control").write_text(
        f"Source: jiffshot\nSection: graphics\nPriority: optional\nMaintainer: {MAINTAINER}\n\n"
        "Package: jiffshot\nArchitecture: amd64\nDescription: Jiffshot screen capture and editor\n",
        encoding="utf-8",
    )
    # Scan every ELF, including the runtime loaded with dlopen. System package
    # names and minimum symbol versions come from the build distro's shlibs data.
    elf_paths = [root / "usr/lib/jiffshot" / name for name in RUNTIME_FILES]
    dependencies = run(
        ["dpkg-shlibdeps", "-O", *(f"-e{path}" for path in elf_paths)],
        cwd=metadata.parent,
        env=env,
    ).stdout.strip()
    prefix = "shlibs:Depends="
    if not dependencies.startswith(prefix):
        raise ValueError(f"dpkg-shlibdeps returned unexpected dependency metadata: {dependencies}")
    dependencies = ", ".join((dependencies.removeprefix(prefix), *DEB_RUNTIME))
    installed_size = (sum(path.stat().st_size for path in payload.rglob("*") if path.is_file()) + 1023) // 1024
    control = root / "DEBIAN/control"
    control.parent.mkdir()
    control.parent.chmod(0o755)
    control.write_text(
        f"Package: jiffshot\nVersion: {version}\nArchitecture: amd64\n"
        f"Maintainer: {MAINTAINER}\nSection: graphics\nPriority: optional\n"
        f"Installed-Size: {installed_size}\nDepends: {dependencies}\n"
        "Homepage: https://github.com/Devolutions/jiffshot\n"
        "Description: Screen capture, recording, and non-destructive image editor\n"
        " Linux image/document editing, OCR, portal capture, and video tools.\n"
        " Screen capture requires a running XDG desktop portal and PipeWire.\n",
        encoding="utf-8",
    )
    control.chmod(0o644)
    destination = work / f"jiffshot_{version}_amd64.deb"
    run(["dpkg-deb", "--build", "--root-owner-group", "-Zxz", str(root), str(destination)], env=env)
    return destination


def build_rpm(payload, work, version, env, epoch):
    topdir = work / "rpm"
    for name in ("BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"):
        (topdir / name).mkdir(parents=True)
    write_archive(payload, topdir / "SOURCES/payload.tar.gz", f"jiffshot-{version}", epoch)
    spec = (ASSETS / "jiffshot.spec").read_text(encoding="utf-8").replace("@VERSION@", version)
    spec_path = topdir / "SPECS/jiffshot.spec"
    spec_path.write_text(spec, encoding="utf-8")
    run(
        ["rpmbuild", "-bb", "--define", f"_topdir {topdir}",
         "--define", "use_source_date_epoch_as_buildtime 1",
         "--define", "clamp_mtime_to_source_date_epoch 1", str(spec_path)],
        env=env,
    )
    destination = topdir / f"RPMS/x86_64/jiffshot-{version}-1.x86_64.rpm"
    if not destination.is_file():
        raise ValueError(f"rpmbuild did not produce {destination.name}")
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-dir", type=Path, default=ROOT / "target/release")
    parser.add_argument("--output-dir", type=Path, default=ROOT / "package/out/linux")
    parser.add_argument("--version", default=os.environ.get("JIFFSHOT_VERSION") or None,
                        help="expected version; must match Cargo.toml (defaults to JIFFSHOT_VERSION)")
    parser.add_argument("--format", choices=("tar.gz", "deb", "rpm"), nargs="+", default=["tar.gz", "deb"])
    args = parser.parse_args()
    if sys.platform != "linux":
        raise ValueError("run this packaging script on Linux (or in WSL)")
    version = package_version(args.version)
    formats = tuple(dict.fromkeys(args.format))
    if "deb" in formats and "rpm" in formats:
        raise ValueError("DEB and Fedora RPM require separate native builds; generate them in separate invocations")
    require_tools(formats)
    build_dir = args.build_dir.resolve()
    for name in RUNTIME_FILES:
        validate_binary(build_dir / name)
    if "rpm" in formats:
        validate_rpm_input(build_dir)
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    if not 0 <= epoch <= 0xFFFFFFFF:
        raise ValueError("SOURCE_DATE_EPOCH must fit an unsigned 32-bit gzip timestamp")
    env = {**os.environ, "SOURCE_DATE_EPOCH": str(epoch)}
    with tempfile.TemporaryDirectory(prefix="jiffshot-package-") as temporary:
        work = Path(temporary)
        payload = work / "payload"
        prepare_payload(build_dir, payload)
        artifacts = []
        if "tar.gz" in formats:
            destination = work / f"Devolutions.Jiffshot-linux-x64-{version}.tar.gz"
            write_archive(payload, destination, f"jiffshot-{version}-linux-x64", epoch)
            artifacts.append(destination)
        if "deb" in formats:
            artifacts.append(build_deb(payload, work, version, env))
        if "rpm" in formats:
            artifacts.append(build_rpm(payload, work, version, env, epoch))
        output_dir = args.output_dir.resolve()
        output_dir.mkdir(parents=True, exist_ok=True)
        checksums = []
        for path in artifacts:
            with path.open("rb") as content:
                checksums.append(f"{hashlib.file_digest(content, 'sha256').hexdigest()}  {path.name}\n")
        sums = work / "SHA256SUMS-linux-x64.txt"
        sums.write_text("".join(checksums), encoding="ascii")
        with tempfile.TemporaryDirectory(prefix=".jiffshot-", dir=output_dir) as publishing:
            for artifact in (*artifacts, sums):
                staged = Path(publishing) / artifact.name
                shutil.copyfile(artifact, staged)
                staged.chmod(0o644)
                staged.replace(output_dir / artifact.name)
                print(f"{output_dir / artifact.name} ({artifact.stat().st_size:,} bytes)")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"Linux packaging failed: {error}", file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError):
            print(error.stdout, error.stderr, file=sys.stderr)
        sys.exit(1)
