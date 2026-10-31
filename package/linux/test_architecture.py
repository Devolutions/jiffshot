"""Architecture dispatch and release regression tests; no real ARM execution."""

import contextlib
import hashlib
import io
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import assemble_release
import build
import validate_installed
from test_build import elf_header


class ElfArchitectureTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)

    def inputs(self, machine):
        directory = self.work / f"release-{machine}"
        directory.mkdir(exist_ok=True)
        for name in build.RUNTIME_FILES:
            (directory / name).write_bytes(elf_header(machine) + b"debug data")
        return directory

    def test_supported_elf_headers_dispatch(self):
        binary = self.work / "binary"
        for arch, machine, deb, rpm in (
            ("x64", 62, "amd64", "x86_64"), ("arm64", 183, "arm64", "aarch64"),
        ):
            for abi in (0, 3):
                for elf_type in (2, 3):
                    with self.subTest(arch=arch, abi=abi, type=elf_type):
                        header = bytearray(elf_header(machine, elf_type))
                        header[7] = abi
                        binary.write_bytes(header)
                        actual = build.validate_binary(binary)
                        self.assertEqual((actual.name, actual.machine, actual.deb, actual.rpm),
                                         (arch, machine, deb, rpm))

    def test_invalid_elf_headers_are_rejected(self):
        binary = self.work / "invalid"
        cases = [
            ("empty", b"", "ELF"),
            ("truncated_ident", elf_header()[:6], "ELF"),
            ("truncated_machine", elf_header()[:19], "ELF"),
            ("truncated_header", elf_header()[:63], "ELF"),
        ]
        for label, offset, value, error in (
            ("magic", 0, 0, "ELF"), ("elf32", 4, 1, "ELF"),
            ("big_endian", 5, 2, "ELF"), ("ident_version", 6, 2, "version or Linux ABI"),
            ("freebsd", 7, 9, "version or Linux ABI"), ("abi_version", 8, 1, "version or Linux ABI"),
            ("object", 16, 1, "executable or shared object"),
            ("core", 16, 4, "executable or shared object"),
            ("unknown_machine", 18, 0, "unsupported ELF machine"),
            ("arm32", 18, 40, "unsupported ELF machine 40"),
            ("x86_32", 18, 3, "unsupported ELF machine 3"),
            ("header_version", 20, 2, "version or Linux ABI"),
            ("small_header", 52, 63, "header size"),
            ("large_header", 52, 65, "header size"),
        ):
            header = bytearray(elf_header())
            header[offset] = value
            cases.append((label, bytes(header), error))
        for label, data, error in cases:
            with self.subTest(case=label):
                binary.write_bytes(data)
                with self.assertRaisesRegex(ValueError, f"invalid.*{error}"):
                    build.validate_binary(binary)

    def test_every_runtime_library_must_match_executable(self):
        for machine, other, expected, actual in (
            (62, 183, "x64", "arm64"), (183, 62, "arm64", "x64"),
        ):
            for name in build.RUNTIME_FILES[1:]:
                with self.subTest(arch=expected, library=name):
                    inputs = self.inputs(machine)
                    (inputs / name).write_bytes(elf_header(other))
                    with self.assertRaisesRegex(ValueError, f"{name}.*Linux {actual}, expected Linux {expected}"):
                        build.validate_build(inputs)
                    with patch.object(build, "run") as native:
                        with self.assertRaises(ValueError):
                            build.prepare_payload(inputs, self.work / "payload")
                        native.assert_not_called()
                    self.assertFalse((self.work / "payload").exists())

    def test_libraries_must_be_shared_objects(self):
        for machine in (62, 183):
            for name in build.RUNTIME_FILES[1:]:
                with self.subTest(machine=machine, library=name):
                    inputs = self.inputs(machine)
                    (inputs / name).write_bytes(elf_header(machine, 2))
                    with self.assertRaisesRegex(ValueError, f"{name}.*shared object"):
                        build.validate_build(inputs)

    def test_explicit_architecture_is_an_assertion(self):
        for arch, machine, other in (("x64", 62, "arm64"), ("arm64", 183, "x64")):
            with self.subTest(arch=arch):
                inputs = self.inputs(machine)
                self.assertEqual(build.validate_build(inputs, arch).name, arch)
                with self.assertRaisesRegex(ValueError, f"Linux {arch}, expected Linux {other}"):
                    build.validate_build(inputs, other)
                with self.assertRaisesRegex(ValueError, "unsupported architecture assertion"):
                    build.validate_build(inputs, "mips")

    def test_missing_runtime_is_rejected(self):
        for name in build.RUNTIME_FILES:
            with self.subTest(file=name):
                inputs = self.inputs(62)
                (inputs / name).unlink()
                with self.assertRaises(FileNotFoundError) as error:
                    build.validate_build(inputs)
                self.assertEqual(Path(error.exception.filename), inputs / name)

    def test_cli_rejects_bad_inputs_before_tools_or_publication(self):
        inputs = self.inputs(62)
        output = self.work / "out"
        for case in ("assertion", "mixed", "unsupported", "missing"):
            with self.subTest(case=case):
                self.inputs(62)
                asserted = "arm64" if case == "assertion" else "x64"
                if case == "mixed":
                    (inputs / "libSkiaSharp.so").write_bytes(elf_header(183))
                elif case == "unsupported":
                    (inputs / "jiffshot").write_bytes(elf_header(40))
                elif case == "missing":
                    (inputs / "libHarfBuzzSharp.so").unlink()
                argv = ["build.py", "--arch", asserted, "--build-dir", str(inputs),
                        "--output-dir", str(output), "--format", "tar.gz"]
                with patch.object(sys, "argv", argv), patch.object(sys, "platform", "linux"):
                    with patch.object(build, "require_tools") as tools:
                        with self.assertRaises((ValueError, FileNotFoundError)):
                            build.main()
                        tools.assert_not_called()
                self.assertFalse(output.exists())

    def test_cli_failure_has_nonzero_exit_and_file_specific_error(self):
        inputs = self.inputs(62)
        output = self.work / "out"
        result = subprocess.run(
            [sys.executable, str(build.ASSETS / "build.py"), "--arch", "arm64",
             "--build-dir", str(inputs), "--output-dir", str(output), "--format", "tar.gz"],
            capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("Linux packaging failed:", result.stderr)
        self.assertIn("jiffshot is Linux x64, expected Linux arm64", result.stderr)
        self.assertFalse(output.exists())

    def test_cli_rejects_unsupported_architecture_or_format_options(self):
        for option, value in (("--arch", "mips"), ("--format", "zip")):
            with self.subTest(option=option):
                with patch.object(sys, "argv", ["build.py", option, value]):
                    with contextlib.redirect_stderr(io.StringIO()) as errors:
                        with self.assertRaises(SystemExit) as failure:
                            build.main()
                self.assertEqual(failure.exception.code, 2)
                self.assertIn("invalid choice", errors.getvalue())

    def test_installed_validator_checks_all_runtime_libraries(self):
        inputs = self.inputs(183)
        args = ["validate_installed.py", "--arch", "arm64", "--runtime-dir", str(inputs)]
        with patch.object(sys, "argv", args), contextlib.redirect_stdout(io.StringIO()) as output:
            validate_installed.main()
        self.assertEqual(output.getvalue(), "Installed runtime: Linux arm64\n")
        (inputs / "libHarfBuzzSharp.so").write_bytes(elf_header(62))
        with patch.object(sys, "argv", args), self.assertRaisesRegex(ValueError, "libHarfBuzzSharp.so.*expected Linux arm64"):
            validate_installed.main()


class ArchitecturePackagingTests(unittest.TestCase):
    setUp = ElfArchitectureTests.setUp
    inputs = ElfArchitectureTests.inputs

    def test_tar_publication_uses_detected_architecture_and_checksums(self):
        for arch, machine in (("x64", 62), ("arm64", 183)):
            with self.subTest(arch=arch):
                inputs = self.inputs(machine)
                output = self.work / arch
                args = ["build.py", "--build-dir", str(inputs), "--output-dir", str(output),
                        "--format", "tar.gz", "tar.gz"]
                with patch.object(sys, "argv", args), patch.object(sys, "platform", "linux"):
                    with patch.object(build, "require_tools"), patch.object(build, "run"):
                        with patch.dict(os.environ, {"SOURCE_DATE_EPOCH": "123"}):
                            with contextlib.redirect_stdout(io.StringIO()):
                                build.main()
                version = build.package_version()
                name = f"Devolutions.Jiffshot-linux-{arch}-{version}.tar.gz"
                self.assertEqual({path.name for path in output.iterdir()},
                                 {name, f"SHA256SUMS-linux-{arch}.txt"})
                artifact = output / name
                digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
                self.assertEqual((output / f"SHA256SUMS-linux-{arch}.txt").read_text(),
                                 f"{digest}  {name}\n")
                with tarfile.open(artifact) as archive:
                    prefix = f"jiffshot-{version}-linux-{arch}"
                    self.assertEqual(archive.getnames()[0], prefix)
                    for runtime in build.RUNTIME_FILES:
                        self.assertEqual(
                            archive.extractfile(f"{prefix}/lib/jiffshot/{runtime}").read(),
                            elf_header(machine) + b"debug data",
                        )
                        self.assertEqual((inputs / runtime).read_bytes(),
                                         elf_header(machine) + b"debug data")

    def test_deb_metadata_and_filename_follow_elf_architecture(self):
        for arch, machine, deb in (("x64", 62, "amd64"), ("arm64", 183, "arm64")):
            with self.subTest(arch=arch):
                work = self.work / arch
                work.mkdir()
                payload = work / "payload"
                with patch.object(build, "run"):
                    build.prepare_payload(self.inputs(machine), payload)
                with patch.object(build, "run") as native:
                    native.return_value.stdout = "shlibs:Depends=libc6 (>= 2.39)\n"
                    destination = build.build_deb(payload, work, "1.2.3", {}, build.validate_build(self.inputs(machine)))
                self.assertEqual(destination.name, f"jiffshot_1.2.3_{deb}.deb")
                self.assertIn(f"\nArchitecture: {deb}\n", (work / "deb/DEBIAN/control").read_text())
                self.assertIn(f"\nArchitecture: {deb}\n", (work / "metadata/debian/control").read_text())
                self.assertIn("Depends: libc6 (>= 2.39), libx11-6", (work / "deb/DEBIAN/control").read_text())
                scan = native.call_args_list[0].args[0]
                self.assertEqual(scan, ["dpkg-shlibdeps", "-O", *(
                    f"-e{work / 'deb/usr/lib/jiffshot' / name}" for name in build.RUNTIME_FILES
                )])
                self.assertEqual(native.call_args_list[-1].args[0][-1], str(destination))

    def test_deb_rejects_invalid_dependency_metadata(self):
        payload = self.work / "payload"
        with patch.object(build, "run"):
            build.prepare_payload(self.inputs(62), payload)
        with patch.object(build, "run") as native:
            native.return_value.stdout = "not shlibs metadata"
            with self.assertRaisesRegex(ValueError, "unexpected dependency metadata"):
                build.build_deb(payload, self.work, "1.2.3", {}, build.ARCHITECTURES["x64"])
            self.assertEqual(len(native.call_args_list), 1)
        self.assertFalse((self.work / "deb/DEBIAN").exists())

    def test_stripped_runtime_is_revalidated_before_publication(self):
        inputs = self.inputs(183)
        output = self.work / "out"
        def wrong_strip(command, **kwargs):
            if command[0] == "strip":
                Path(command[-1]).write_bytes(elf_header(62))
        args = ["build.py", "--build-dir", str(inputs), "--output-dir", str(output),
                "--format", "tar.gz"]
        with patch.object(sys, "argv", args), patch.object(sys, "platform", "linux"):
            with patch.object(build, "require_tools"), patch.object(build, "run", side_effect=wrong_strip):
                with self.assertRaisesRegex(ValueError, "expected Linux arm64"):
                    build.main()
        self.assertFalse(output.exists())
        self.assertEqual((inputs / "jiffshot").read_bytes(), elf_header(183) + b"debug data")

    def test_timestamp_boundaries_and_invalid_values(self):
        inputs = self.inputs(62)
        for label, epoch, accepted in (
            ("zero", "0", True), ("maximum", "4294967295", True),
            ("negative", "-1", False), ("overflow", "4294967296", False),
            ("not_integer", "yesterday", False),
        ):
            with self.subTest(case=label):
                output = self.work / label
                args = ["build.py", "--build-dir", str(inputs), "--output-dir", str(output),
                        "--format", "tar.gz"]
                with patch.object(sys, "argv", args), patch.object(sys, "platform", "linux"):
                    with patch.object(build, "require_tools"), patch.object(build, "run"):
                        with patch.dict(os.environ, {"SOURCE_DATE_EPOCH": epoch}):
                            with contextlib.redirect_stdout(io.StringIO()):
                                if accepted:
                                    build.main()
                                else:
                                    with self.assertRaises(ValueError):
                                        build.main()
                if accepted:
                    artifact, = output.glob("*.tar.gz")
                    self.assertEqual(int.from_bytes(artifact.read_bytes()[4:8], "little"), int(epoch))
                else:
                    self.assertFalse(output.exists())

    def test_rpm_target_spec_and_filename_follow_elf_architecture(self):
        for arch, machine, rpm in (("x64", 62, "x86_64"), ("arm64", 183, "aarch64")):
            with self.subTest(arch=arch):
                work = self.work / arch
                work.mkdir()
                payload = work / "payload"
                with patch.object(build, "run"):
                    build.prepare_payload(self.inputs(machine), payload)
                def rpmbuild(command, **kwargs):
                    self.assertEqual(command[:4], ["rpmbuild", "-bb", "--target", rpm])
                    spec = Path(command[-1]).read_text()
                    self.assertIn(f"BuildArch:      {rpm}\n", spec)
                    self.assertIn("Version:        1.2.3\n", spec)
                    self.assertNotIn("@ARCH@", spec)
                    destination = work / f"rpm/RPMS/{rpm}/jiffshot-1.2.3-1.{rpm}.rpm"
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_bytes(b"rpm output")
                with patch.object(build, "run", side_effect=rpmbuild):
                    artifact = build.build_rpm(payload, work, "1.2.3", {}, 123, build.validate_build(self.inputs(machine)))
                self.assertEqual(artifact.relative_to(work).as_posix(),
                                 f"rpm/RPMS/{rpm}/jiffshot-1.2.3-1.{rpm}.rpm")
                self.assertEqual(artifact.read_bytes(), b"rpm output")

    def test_rpm_rejects_missing_target_output(self):
        payload = self.work / "payload"
        with patch.object(build, "run"):
            build.prepare_payload(self.inputs(183), payload)
            with self.assertRaisesRegex(ValueError, r"did not produce jiffshot-1.2.3-1.aarch64.rpm"):
                build.build_rpm(payload, self.work, "1.2.3", {}, 0, build.ARCHITECTURES["arm64"])

    def test_rpm_abi_guard_rejects_ubuntu_even_with_fedora_dependency(self):
        with patch.object(build, "run") as native:
            for label, needed in (
                ("missing", ""), ("ubuntu", "[liblept.so.5]"),
                ("mixed", "[liblept.so.5]\n[libleptonica.so.6]"),
            ):
                with self.subTest(case=label):
                    native.return_value.stdout = needed
                    with self.assertRaisesRegex(ValueError, "native Fedora build linked to libleptonica.so.6"):
                        build.validate_rpm_input(Path("/release"))
            native.return_value.stdout = "[libleptonica.so.6]"
            build.validate_rpm_input(Path("/release"))
            self.assertEqual(native.call_args.args[0], ["readelf", "--dynamic", "/release/jiffshot"])


class ReleaseAssemblyTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)
        self.version = build.package_version()
        self.artifacts = self.work / "artifacts"
        self.output = self.work / "release"
        self.packages = {}
        self.manifests = []
        for arch, deb, rpm in (("x64", "amd64", "x86_64"), ("arm64", "arm64", "aarch64")):
            for suffix, names in (
                ("", (f"Devolutions.Jiffshot-linux-{arch}-{self.version}.tar.gz",
                      f"jiffshot_{self.version}_{deb}.deb")),
                ("-rpm", (f"jiffshot-{self.version}-1.{rpm}.rpm",)),
            ):
                directory = self.artifacts / f"jiffshot-linux-{arch}{suffix}"
                directory.mkdir(parents=True)
                lines = []
                for name in names:
                    content = f"independent fixture {name}".encode()
                    (directory / name).write_bytes(content)
                    self.packages[name] = (directory / name, content)
                    lines.append(f"{hashlib.sha256(content).hexdigest()}  {name}\n")
                manifest = directory / f"SHA256SUMS-linux-{arch}.txt"
                manifest.write_text("".join(lines), encoding="ascii")
                self.manifests.append(manifest)

    def test_assembly_requires_six_verified_packages(self):
        self.output.mkdir()
        (self.output / "windows.msi").write_bytes(b"leave Windows assets alone")
        actual = assemble_release.assemble(self.artifacts, self.output, self.version)
        self.assertEqual(set(actual), set(self.packages))
        self.assertEqual(len(actual), 6)
        for name, (_, content) in self.packages.items():
            self.assertEqual((self.output / name).read_bytes(), content)
        for arch in ("x64", "arm64"):
            manifest = self.output / f"SHA256SUMS-linux-{arch}.txt"
            names = {name for name in self.packages if
                     f"linux-{arch}-" in name or
                     name.endswith("_amd64.deb" if arch == "x64" else "_arm64.deb") or
                     name.endswith(".x86_64.rpm" if arch == "x64" else ".aarch64.rpm")}
            records = assemble_release.read_checksums(manifest, names)
            self.assertEqual(len(records), 3)
            for name in names:
                self.assertEqual(records[name], hashlib.sha256(self.packages[name][1]).hexdigest())
        self.assertEqual((self.output / "windows.msi").read_bytes(), b"leave Windows assets alone")

    def test_each_missing_package_blocks_assembly_without_partial_output(self):
        for name, (path, content) in self.packages.items():
            with self.subTest(package=name):
                path.unlink()
                with self.assertRaises(FileNotFoundError) as error:
                    assemble_release.assemble(self.artifacts, self.output, self.version)
                self.assertEqual(Path(error.exception.filename), path)
                self.assertFalse(self.output.exists())
                path.write_bytes(content)

    def test_each_missing_manifest_blocks_assembly(self):
        for path in self.manifests:
            with self.subTest(manifest=path.parent.name):
                content = path.read_bytes()
                path.unlink()
                with self.assertRaises(FileNotFoundError):
                    assemble_release.assemble(self.artifacts, self.output, self.version)
                self.assertFalse(self.output.exists())
                path.write_bytes(content)

    def test_corrupt_package_blocks_assembly_before_copying(self):
        for name, (path, content) in self.packages.items():
            with self.subTest(package=name):
                path.write_bytes(content + b"corruption")
                with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                    assemble_release.assemble(self.artifacts, self.output, self.version)
                self.assertFalse(self.output.exists())
                path.write_bytes(content)

    def test_malformed_duplicate_or_wrong_checksum_records_are_rejected(self):
        manifest = self.manifests[-1]
        original = manifest.read_text()
        digest, name = original.strip().split("  ", 1)
        for label, content, error in (
            ("duplicate", original * 2, "duplicate checksum"),
            ("missing_record", "", "must cover exactly"),
            ("wrong_arch", original.replace(".aarch64.rpm", ".x86_64.rpm"), "must cover exactly"),
            ("extra_record", original + f"{digest}  unexpected.deb\n", "must cover exactly"),
            ("short_digest", f"bad  {name}\n", "invalid checksum record"),
            ("path_traversal", f"{digest}  ../{name}\n", "invalid checksum record"),
            ("wrong_separator", f"{digest} {name}\n", "invalid checksum record"),
        ):
            with self.subTest(case=label):
                manifest.write_text(content)
                with self.assertRaisesRegex(ValueError, error):
                    assemble_release.assemble(self.artifacts, self.output, self.version)
                self.assertFalse(self.output.exists())

    def test_version_mismatch_blocks_assembly(self):
        with self.assertRaisesRegex(ValueError, "does not match Cargo.toml"):
            assemble_release.assemble(self.artifacts, self.output, "999.0.0")
        self.assertFalse(self.output.exists())


class ArchitectureWorkflowTests(unittest.TestCase):
    def job(self, name):
        workflow = (build.ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        match = re.search(rf"(?ms)^  {name}:\n(.*?)(?=^  [a-zA-Z0-9_-]+:|\Z)", workflow)
        self.assertIsNotNone(match, f"missing job {name}")
        return match.group(1)

    def test_ubuntu_and_fedora_use_both_native_runners_and_distinct_artifacts(self):
        for job_name, cache_prefix, suffix in (
            ("linux", "ubuntu2404", ""), ("linux-rpm", "fedora43", "-rpm"),
        ):
            with self.subTest(job=job_name):
                job = self.job(job_name)
                rows = re.findall(r"- arch: (\w+)\n\s+runner: ([\w.-]+)\n\s+machine: (\w+)", job)
                self.assertEqual(rows, [("x64", "ubuntu-24.04", "x86_64"),
                                        ("arm64", "ubuntu-24.04-arm", "aarch64")])
                self.assertIn("runs-on: ${{ matrix.runner }}", job)
                self.assertIn('test "$(uname -m)" = "${{ matrix.machine }}"', job)
                self.assertIn("JIFFSHOT_TEST_ARCH: ${{ matrix.arch }}", job)
                self.assertIn('build.py --arch "${{ matrix.arch }}" --format', job)
                self.assertIn("cargo test --release --locked --workspace", job)
                self.assertIn("cargo build --release --locked -p jiffshot", job)
                self.assertIn("python3 -m unittest discover -s package/linux -p 'test_*.py' -v", job)
                self.assertIn(f"name: jiffshot-linux-${{{{ matrix.arch }}}}{suffix}", job)
                self.assertIn(f"key: {cache_prefix}-linux-${{{{ matrix.arch }}}}-", job)
                self.assertIn(f"restore-keys: {cache_prefix}-linux-${{{{ matrix.arch }}}}-", job)
        self.assertIn("container: fedora:43", self.job("linux-rpm"))
        self.assertIn("smoke.py --launcher", self.job("linux"))

    def test_install_matrix_covers_both_formats_and_native_architectures(self):
        job = self.job("linux-package-install")
        rows = re.findall(
            r"- format: (\w+)\n\s+arch: (\w+)\n\s+runner: ([\w.-]+)\n"
            r"\s+machine: (\w+)\n\s+package_arch: (\w+)\n\s+image: ([\w.:]+)\n"
            r"\s+artifact: ([\w-]+)", job,
        )
        self.assertEqual(rows, [
            ("deb", "x64", "ubuntu-24.04", "x86_64", "amd64", "ubuntu:24.04", "jiffshot-linux-x64"),
            ("rpm", "x64", "ubuntu-24.04", "x86_64", "x86_64", "fedora:43", "jiffshot-linux-x64-rpm"),
            ("deb", "arm64", "ubuntu-24.04-arm", "aarch64", "arm64", "ubuntu:24.04", "jiffshot-linux-arm64"),
            ("rpm", "arm64", "ubuntu-24.04-arm", "aarch64", "aarch64", "fedora:43", "jiffshot-linux-arm64-rpm"),
        ])
        self.assertIn("runs-on: ${{ matrix.runner }}", job)
        self.assertIn('sha256sum --check "SHA256SUMS-linux-$4.txt"', job)
        self.assertIn('validate_installed.py --arch "$4"', job)
        self.assertIn('smoke.py --launcher /usr/bin/jiffshot', job)
        self.assertIn('test "$(uname -m)" = "$2"', job)
        self.assertIn("package-removal-sentinel", job)
        self.assertIn("apt-get remove -y jiffshot", job)
        self.assertIn("dnf remove -y jiffshot", job)

    def test_release_uses_verified_collector_and_propagates_failure(self):
        release = (build.ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        self.assertIn("python package/linux/assemble_release.py --artifact-dir artifacts", release)
        self.assertIn('--output-dir release --version "$env:RELEASE_VERSION"', release)
        self.assertIn("if ($LASTEXITCODE -ne 0) { throw 'Linux release assembly", release)
        self.assertIn("Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256", release)
        self.assertIn("Set-Content release\\SHA256SUMS.txt -Encoding ascii", release)


if __name__ == "__main__":
    unittest.main()
