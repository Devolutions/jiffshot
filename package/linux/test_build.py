"""Focused regression tests for the Linux package builder and its outputs."""

import configparser
import hashlib
import json
import os
import platform
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

import build


def elf_header(machine=62, elf_type=3):
    """Synthetic ELF64 header, never an executable runtime fixture."""
    header = bytearray(64)
    header[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HHI", header, 16, elf_type, machine, 1)
    struct.pack_into("<H", header, 52, 64)
    return bytes(header)

EXPECTED_FILES = {
    *(f"lib/jiffshot/{name}" for name in build.RUNTIME_FILES),
    "bin/jiffshot",
    "share/applications/com.devolutions.Jiffshot.desktop",
    "share/icons/hicolor/scalable/apps/com.devolutions.Jiffshot.svg",
    "share/mime/packages/com.devolutions.Jiffshot.xml",
    "share/doc/jiffshot/README-linux.txt",
    "share/doc/jiffshot/README.md",
}


def sha256(path):
    with path.open("rb") as content:
        return hashlib.file_digest(content, "sha256").hexdigest()


class PackageBuilderTests(unittest.TestCase):
    def test_desktop_actions_launch_supported_linux_workflows(self):
        desktop = configparser.ConfigParser(interpolation=None)
        desktop.read(build.ASSETS / "com.devolutions.Jiffshot.desktop", encoding="utf-8")
        self.assertEqual(
            desktop["Desktop Entry"]["Actions"],
            "Capture;InteractiveCapture;GrabText;Scrolling;HorizontalScrolling;Panoramic;Interval;Record;",
        )
        self.assertEqual(desktop["Desktop Action Capture"]["Exec"], "jiffshot --capture")
        self.assertEqual(
            desktop["Desktop Action InteractiveCapture"]["Exec"], "jiffshot --capture-interactive"
        )
        self.assertEqual(desktop["Desktop Action GrabText"]["Exec"], "jiffshot --grab-text")
        self.assertEqual(desktop["Desktop Action Scrolling"]["Exec"], "jiffshot --scrolling")
        self.assertEqual(
            desktop["Desktop Action HorizontalScrolling"]["Exec"], "jiffshot --scrolling-horizontal"
        )
        self.assertEqual(desktop["Desktop Action Panoramic"]["Exec"], "jiffshot --panoramic")
        self.assertEqual(desktop["Desktop Action Interval"]["Exec"], "jiffshot --interval")
        self.assertEqual(desktop["Desktop Action Record"]["Exec"], "jiffshot --record")

    def test_ci_jobs_are_unique_and_include_linux_package_builds(self):
        workflow = (build.ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("\njobs:\n", workflow)
        jobs = workflow.split("\njobs:\n", 1)[1]
        job_names = re.findall(r"(?m)^  ([a-zA-Z0-9_-]+):[ \t]*$", jobs)
        self.assertEqual(len(job_names), len(set(job_names)), f"duplicate CI job IDs: {job_names}")
        self.assertTrue({"linux", "linux-rpm", "linux-package-install"}.issubset(job_names))

    def test_release_version_matches_workspace_and_rejects_mismatches(self):
        version = build.package_version()
        self.assertEqual(build.package_version(version), version)
        with self.assertRaisesRegex(ValueError, "does not match Cargo.toml"):
            build.package_version("999.0.0")

    def test_release_version_rejects_prereleases(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Cargo.toml").write_text('[workspace.package]\nversion = "1.0.0-beta.1"\n')
            with patch.object(build, "ROOT", root), self.assertRaisesRegex(ValueError, "X.Y.Z"):
                build.package_version()

    def test_binary_validation_rejects_non_elf_and_unsupported_inputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "binary"
            binary.write_bytes(b"not an ELF file")
            with self.assertRaisesRegex(ValueError, "ELF"):
                build.validate_binary(binary)
            binary.write_bytes(elf_header(40))
            with self.assertRaisesRegex(ValueError, "unsupported ELF machine 40"):
                build.validate_binary(binary)
            binary.write_bytes(elf_header())
            self.assertEqual(build.validate_binary(binary).name, "x64")

    def test_missing_tools_are_reported_before_building(self):
        with patch.object(shutil, "which", return_value=None):
            with self.assertRaisesRegex(ValueError, "desktop-file-validate, dpkg-deb, dpkg-shlibdeps, readelf, rpmbuild, strip"):
                build.require_tools(("deb", "rpm"))

    def test_deb_and_rpm_require_separate_native_builds(self):
        with patch.object(sys, "argv", ["build.py", "--format", "deb", "rpm"]):
            with self.assertRaisesRegex(ValueError, "separate native builds"):
                build.main()

    def test_rpm_input_rejects_ubuntu_ocr_abi_and_accepts_fedora_abi(self):
        with patch.object(build, "run") as native:
            native.return_value.stdout = "NEEDED Shared library: [liblept.so.5]"
            with self.assertRaisesRegex(ValueError, "native Fedora build"):
                build.validate_rpm_input(Path("/release"))
            native.return_value.stdout = "NEEDED Shared library: [libleptonica.so.6]"
            build.validate_rpm_input(Path("/release"))

    def test_payload_staging_strips_copies_without_modifying_build_inputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            inputs, payload = work / "release", work / "payload"
            inputs.mkdir()
            content = elf_header() + b"original debug data"
            for name in build.RUNTIME_FILES:
                (inputs / name).write_bytes(content)
            previous_umask = os.umask(0o077)
            try:
                with patch.object(build, "run") as native:
                    build.prepare_payload(inputs, payload)
            finally:
                os.umask(previous_umask)
            self.assertEqual([call.args[0] for call in native.call_args_list[:4]], [
                ["strip", "--strip-debug", str(payload / "lib/jiffshot" / name)]
                for name in build.RUNTIME_FILES
            ])
            for name in build.RUNTIME_FILES:
                self.assertEqual((inputs / name).read_bytes(), content)
            for directory in (payload, *(path for path in payload.rglob("*") if path.is_dir())):
                self.assertEqual(directory.stat().st_mode & 0o777, 0o755)
            self.assertEqual(
                {path.relative_to(payload).as_posix() for path in payload.rglob("*") if path.is_file()},
                EXPECTED_FILES,
            )

    def test_archive_is_reproducible_with_safe_paths_and_normalized_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            payload = work / "payload with spaces"
            (payload / "bin").mkdir(parents=True)
            executable = payload / "bin/jiffshot"
            executable.write_bytes(b"launcher")
            executable.chmod(0o755)
            first, second = work / "first.tar.gz", work / "second.tar.gz"
            for destination in (first, second):
                build.write_archive(payload, destination, "jiffshot-1.2.3-linux-x64", 123456789)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            with tarfile.open(first) as archive:
                self.assertEqual(archive.getnames(), [
                    "jiffshot-1.2.3-linux-x64", "jiffshot-1.2.3-linux-x64/bin",
                    "jiffshot-1.2.3-linux-x64/bin/jiffshot",
                ])
                for member in archive:
                    self.assertEqual((member.uid, member.gid, member.mtime, member.mode), (0, 0, 123456789, 0o755))
                self.assertEqual(archive.extractfile(archive.getmembers()[-1]).read(), b"launcher")

    def test_launcher_preserves_arguments_cwd_and_library_path_through_symlinks(self):
        with tempfile.TemporaryDirectory(prefix="jiffshot launch ") as temporary:
            work = Path(temporary)
            bindir, runtime = work / "package/bin", work / "package/lib/jiffshot"
            bindir.mkdir(parents=True)
            runtime.mkdir(parents=True)
            launcher = bindir / "jiffshot"
            launcher.write_text((build.ASSETS / "jiffshot").read_text())
            launcher.chmod(0o755)
            executable = runtime / "jiffshot"
            executable.write_text(
                "#!/usr/bin/env python3\nimport json, os, sys\n"
                "print(json.dumps([sys.argv[1:], os.getcwd(), os.environ['LD_LIBRARY_PATH'], "
                "os.environ['JIFFSHOT_LAUNCHER']]))\n"
            )
            executable.chmod(0o755)
            symlink = work / "linked launcher"
            symlink.symlink_to(launcher)
            args = ["image with spaces.png", "", "--not-a-shell-command", "$unchanged"]
            for command in (str(symlink), "jiffshot"):
                result = subprocess.run(
                    [command, *args], cwd=work, check=True, capture_output=True, text=True,
                    env={**os.environ, "PATH": f"{bindir}:{os.environ['PATH']}", "LD_LIBRARY_PATH": "/existing/libs"},
                )
                actual_args, cwd, libraries, startup_launcher = json.loads(result.stdout)
                self.assertEqual(actual_args, args)
                self.assertEqual(cwd, str(work))
                self.assertEqual(Path(libraries.split(":")[0]).resolve(), runtime)
                self.assertTrue(libraries.endswith(":/existing/libs"))
                self.assertEqual(startup_launcher, str(launcher.resolve()))

class PackageContentTests(unittest.TestCase):
    def test_requested_formats_have_complete_payload_metadata_and_checksums(self):
        version = build.package_version()
        native_arch = {"x86_64": "x64", "aarch64": "arm64"}.get(platform.machine())
        arch = os.environ.get("JIFFSHOT_TEST_ARCH", native_arch)
        # Independent expectations: do not derive test identity from the builder.
        deb_arch, rpm_arch, machine = {
            "x64": ("amd64", "x86_64", 62),
            "arm64": ("arm64", "aarch64", 183),
        }[arch]
        output = Path(os.environ.get("JIFFSHOT_PACKAGE_DIR", build.ROOT / "package/out/linux")).resolve()
        formats = os.environ.get("JIFFSHOT_TEST_FORMATS", "tar.gz,deb,rpm").split(",")
        names = {
            "tar.gz": f"Devolutions.Jiffshot-linux-{arch}-{version}.tar.gz",
            "deb": f"jiffshot_{version}_{deb_arch}.deb",
            "rpm": f"jiffshot-{version}-1.{rpm_arch}.rpm",
        }
        sums = dict(
            (name, digest) for digest, name in (
                line.split("  ", 1) for line in (output / f"SHA256SUMS-linux-{arch}.txt").read_text().splitlines()
            )
        )
        self.assertTrue(formats, "no package formats requested")
        self.assertEqual(set(sums), {names[format_name] for format_name in formats})
        with tempfile.TemporaryDirectory(prefix="jiffshot extracted ") as temporary:
            extracted = {}
            for format_name in formats:
                artifact = output / names[format_name]
                with self.subTest(format=format_name):
                    self.assertEqual(sha256(artifact), sums[artifact.name])
                    destination = Path(temporary) / format_name
                    destination.mkdir()
                    if format_name == "tar.gz":
                        with tarfile.open(artifact) as archive:
                            prefix = f"jiffshot-{version}-linux-{arch}"
                            self.assertTrue(all(
                                member.name == prefix or member.name.startswith(prefix + "/")
                                for member in archive
                            ))
                            self.assertTrue(all(not member.issym() and not member.islnk() for member in archive))
                            for member in archive:
                                self.assertEqual((member.uid, member.gid), (0, 0))
                                if member.isdir():
                                    self.assertEqual(member.mode, 0o755)
                            archive.extractall(destination, filter="data")
                        root = destination / prefix
                    elif format_name == "deb":
                        metadata = build.run(["dpkg-deb", "-f", str(artifact), "Version", "Architecture"]).stdout
                        self.assertEqual(metadata, f"Version: {version}\nArchitecture: {deb_arch}\n")
                        depends = build.run(["dpkg-deb", "-f", str(artifact), "Depends"]).stdout
                        for dependency in (
                            "libc6", "libtesseract5", "libx11-6", "libgtk-3-0t64",
                            "tesseract-ocr-eng", "gstreamer1.0-pipewire",
                        ):
                            self.assertIn(dependency, depends)
                        with subprocess.Popen(
                            ["dpkg-deb", "--fsys-tarfile", str(artifact)], stdout=subprocess.PIPE,
                        ) as unpacker:
                            with tarfile.open(fileobj=unpacker.stdout, mode="r|") as archive:
                                directories = [
                                    (member.name.rstrip("/"), member.mode, member.uid, member.gid)
                                    for member in archive if member.isdir()
                                ]
                            unpacker.stdout.close()
                            self.assertEqual(unpacker.wait(), 0)
                        self.assertIn((".", 0o755, 0, 0), directories)
                        self.assertTrue(all(mode == 0o755 and uid == gid == 0 for _, mode, uid, gid in directories))
                        build.run(["dpkg-deb", "-x", str(artifact), str(destination)])
                        root = destination / "usr"
                    else:
                        self.assertEqual(build.run([
                            "rpm", "-qp", "--queryformat", "%{VERSION} %{RELEASE} %{ARCH}", str(artifact),
                        ]).stdout, f"{version} 1 {rpm_arch}")
                        requires = build.run(["rpm", "-qp", "--requires", str(artifact)]).stdout
                        self.assertRegex(requires, r"(?m)^libtesseract\.so\.5(?:\.\d+)*\(\)\(64bit\)$")
                        for dependency in ("libX11.so.6()(64bit)", "gtk3", "pipewire-gstreamer"):
                            self.assertIn(dependency, requires)
                        self.assertIn("libleptonica.so.6()(64bit)", requires)
                        self.assertNotIn("liblept.so.5", requires)
                        modes = dict(line.split(" ", 1) for line in build.run([
                            "rpm", "-qp", "--queryformat", "[%{FILENAMES} %{FILEMODES:perms}\n]", str(artifact),
                        ]).stdout.splitlines())
                        for directory in ("/usr/lib/jiffshot", "/usr/share/doc/jiffshot"):
                            self.assertEqual(modes[directory], "drwxr-xr-x")
                        with subprocess.Popen(["rpm2cpio", str(artifact)], stdout=subprocess.PIPE) as converter:
                            subprocess.run(
                                ["cpio", "-idm", "--quiet", "--no-absolute-filenames"],
                                stdin=converter.stdout, cwd=destination, check=True, capture_output=True,
                            )
                            converter.stdout.close()
                            self.assertEqual(converter.wait(), 0)
                        root = destination / "usr"
                    files = {path.relative_to(root).as_posix() for path in root.rglob("*") if path.is_file()}
                    self.assertEqual(files, EXPECTED_FILES)
                    self.assertEqual((root / "bin/jiffshot").stat().st_mode & 0o777, 0o755)
                    self.assertNotIn(b"\r", (root / "bin/jiffshot").read_bytes())
                    for name in build.RUNTIME_FILES:
                        binary = root / "lib/jiffshot" / name
                        header = binary.read_bytes()[:64]
                        self.assertEqual(header[:7], b"\x7fELF\x02\x01\x01", name)
                        self.assertEqual(struct.unpack_from("<H", header, 18)[0], machine, name)
                        self.assertEqual(build.validate_binary(binary).name, arch)
                    desktop = root / "share/applications/com.devolutions.Jiffshot.desktop"
                    build.run(["desktop-file-validate", str(desktop)])
                    self.assertEqual(
                        desktop.read_text(encoding="utf-8"),
                        (build.ASSETS / "com.devolutions.Jiffshot.desktop").read_text(encoding="utf-8"),
                    )
                    mime = ET.parse(root / "share/mime/packages/com.devolutions.Jiffshot.xml")
                    ns = {"m": "http://www.freedesktop.org/standards/shared-mime-info"}
                    self.assertEqual(mime.find("m:mime-type", ns).get("type"), "application/x-jiffshot")
                    self.assertEqual(mime.find("m:mime-type/m:glob", ns).get("pattern"), "*.jsd")
                    extracted[format_name] = root
            if "tar.gz" in extracted and "deb" in extracted:
                for relative in EXPECTED_FILES:
                    self.assertEqual(sha256(extracted["tar.gz"] / relative), sha256(extracted["deb"] / relative))


if __name__ == "__main__":
    unittest.main()
