#!/usr/bin/env python3
"""Tests for scripts/python-wheels.py and the release workflow steps that use it.

The binaries here are synthetic: the smallest ELF, Mach-O and PE files that carry
the fields the script reads (glibc version requirements, DT_NEEDED entries, the
minimum macOS version, the machine type).
"""

from __future__ import annotations

import csv
import importlib.util
import io
import pathlib
import re
import shutil
import struct
import tempfile
import unittest
import zipfile

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
_spec = importlib.util.spec_from_file_location(
    "python_wheels", REPO_ROOT / "scripts" / "python-wheels.py"
)
pw = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(pw)


def make_elf(machine: int, glibc: list[str], needed: list[str]) -> bytes:
    """A 64-bit LE ELF with .dynstr, .gnu.version_r and .dynamic sections."""
    strtab = bytearray(b"\0")

    def add(name: str) -> int:
        offset = len(strtab)
        strtab.extend(name.encode() + b"\0")
        return offset

    libc = add("libc.so.6")
    aux_names = [add(v) for v in glibc]
    needed_names = [add(n) for n in needed]

    verneed = bytearray()
    if aux_names:
        verneed += struct.pack("<HHIII", 1, len(aux_names), libc, 16, 0)
        for i, name in enumerate(aux_names):
            nxt = 16 if i < len(aux_names) - 1 else 0
            verneed += struct.pack("<IHHII", 0, 0, i + 2, name, nxt)
    dynamic = bytearray()
    for name in needed_names:
        dynamic += struct.pack("<qQ", pw.DT_NEEDED, name)
    dynamic += struct.pack("<qQ", 0, 0)

    header_size = 64
    body = bytearray()
    offsets = {}
    for key, blob in (("strtab", strtab), ("verneed", verneed), ("dynamic", dynamic)):
        offsets[key] = header_size + len(body)
        body += blob
    shoff = header_size + len(body)

    def shdr(sh_type: int, offset: int, size: int, link: int, info: int) -> bytes:
        return struct.pack("<IIQQQQIIQQ", 0, sh_type, 0, 0, offset, size, link, info, 0, 0)

    sections = [
        shdr(0, 0, 0, 0, 0),
        shdr(3, offsets["strtab"], len(strtab), 0, 0),
        shdr(pw.SHT_GNU_VERNEED, offsets["verneed"], len(verneed), 1, 1 if aux_names else 0),
        shdr(pw.SHT_DYNAMIC, offsets["dynamic"], len(dynamic), 1, 0),
    ]
    ident = b"\x7fELF" + bytes([2, 1, 1]) + bytes(9)
    header = ident + struct.pack(
        "<HHIQQQIHHHHHH", 2, machine, 1, 0, 0, shoff, 0, 64, 0, 0, 64, len(sections), 0
    )
    assert len(header) == header_size
    return bytes(header + body + b"".join(sections))


def make_macho(cputype: int, minos: tuple[int, int], build_version: bool = True) -> bytes:
    version = (minos[0] << 16) | (minos[1] << 8)
    if build_version:
        cmd = struct.pack("<IIIIII", pw.LC_BUILD_VERSION, 24, 1, version, version, 0)
    else:
        cmd = struct.pack("<IIII", pw.LC_VERSION_MIN_MACOSX, 16, version, version)
    header = struct.pack("<IiiIIIII", 0xFEEDFACF, cputype, 0, 2, 1, len(cmd), 0, 0)
    return header + cmd


def make_pe(machine: int) -> bytes:
    data = bytearray(0x80)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 0x40)
    data[0x40:0x44] = b"PE\0\0"
    struct.pack_into("<H", data, 0x44, machine)
    return bytes(data)


X86, ARM = pw.ELF_MACHINE["x86_64"], pw.ELF_MACHINE["aarch64"]
GNU_LIBS = ["libc.so.6", "libm.so.6", "libgcc_s.so.1"]


def release_binaries() -> dict[str, bytes]:
    return {
        "darwin-arm64": make_macho(pw.MACHO_CPU["arm64"], (11, 0)),
        "darwin-x64": make_macho(pw.MACHO_CPU["x86_64"], (10, 12), build_version=False),
        "linux-x64-gnu": make_elf(X86, ["GLIBC_2.2.5", "GLIBC_2.29", "GLIBC_2.30"], GNU_LIBS),
        "linux-x64-musl": make_elf(X86, [], []),
        "linux-arm64": make_elf(ARM, ["GLIBC_2.17", "GLIBC_2.28"], GNU_LIBS),
        "win32-x64": make_pe(pw.PE_MACHINE_AMD64),
    }


class PlatformTagTests(unittest.TestCase):
    def test_tags_are_read_from_the_binaries(self):
        tags = {p: pw.platform_tag(p, b) for p, b in release_binaries().items()}
        self.assertEqual(
            tags,
            {
                "darwin-arm64": "macosx_11_0_arm64",
                "darwin-x64": "macosx_10_12_x86_64",
                "linux-x64-gnu": "manylinux_2_30_x86_64",
                "linux-x64-musl": "musllinux_1_2_x86_64",
                "linux-arm64": "manylinux_2_28_aarch64",
                "win32-x64": "win_amd64",
            },
        )

    def test_glibc_newer_than_the_ceiling_is_refused(self):
        newest = f"GLIBC_{pw.MAX_GLIBC[0]}.{pw.MAX_GLIBC[1] + 1}"
        binary = make_elf(X86, ["GLIBC_2.17", newest], GNU_LIBS)
        with self.assertRaisesRegex(pw.BinaryError, "newer than the"):
            pw.platform_tag("linux-x64-gnu", binary)

    def test_glibc_at_the_ceiling_is_accepted(self):
        at = f"GLIBC_{pw.MAX_GLIBC[0]}.{pw.MAX_GLIBC[1]}"
        binary = make_elf(X86, [at], GNU_LIBS)
        self.assertEqual(
            pw.platform_tag("linux-x64-gnu", binary),
            f"manylinux_{pw.MAX_GLIBC[0]}_{pw.MAX_GLIBC[1]}_x86_64",
        )

    def test_library_outside_manylinux_is_refused(self):
        binary = make_elf(X86, ["GLIBC_2.17"], GNU_LIBS + ["libssl.so.3"])
        with self.assertRaisesRegex(pw.BinaryError, "libssl.so.3"):
            pw.platform_tag("linux-x64-gnu", binary)

    def test_gnu_binary_without_glibc_requirement_is_refused(self):
        with self.assertRaisesRegex(pw.BinaryError, "requires no GLIBC"):
            pw.platform_tag("linux-x64-gnu", make_elf(X86, [], []))

    def test_dynamic_binary_in_the_static_slot_is_refused(self):
        binary = make_elf(X86, ["GLIBC_2.17"], GNU_LIBS)
        with self.assertRaisesRegex(pw.BinaryError, "expected a static binary"):
            pw.platform_tag("linux-x64-musl", binary)

    def test_wrong_architecture_is_refused(self):
        with self.assertRaisesRegex(pw.BinaryError, "is not x86_64"):
            pw.platform_tag("linux-x64-gnu", make_elf(ARM, ["GLIBC_2.17"], GNU_LIBS))
        with self.assertRaisesRegex(pw.BinaryError, "is not arm64"):
            pw.platform_tag("darwin-arm64", make_macho(pw.MACHO_CPU["x86_64"], (11, 0)))
        with self.assertRaisesRegex(pw.BinaryError, "is not amd64"):
            pw.platform_tag("win32-x64", make_pe(0xAA64))

    def test_macos_minor_version_after_11_has_no_tag(self):
        binary = make_macho(pw.MACHO_CPU["arm64"], (12, 3))
        with self.assertRaisesRegex(pw.BinaryError, "has no wheel tag"):
            pw.platform_tag("darwin-arm64", binary)


class BuildTests(unittest.TestCase):
    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.pure = self.tmp / "khive-1.2.3-py3-none-any.whl"
        with zipfile.ZipFile(self.pure, "w") as wheel:
            wheel.writestr("khive/__init__.py", "__version__ = '1.2.3'\n")
            wheel.writestr(
                "khive-1.2.3.dist-info/WHEEL",
                "Wheel-Version: 1.0\nGenerator: test\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
            )
            wheel.writestr("khive-1.2.3.dist-info/METADATA", "Metadata-Version: 2.1\nName: khive\n")
            wheel.writestr("khive-1.2.3.dist-info/RECORD", "")
        self.artifacts = self.tmp / "artifacts"
        for platform, data in release_binaries().items():
            exe = pw.PLATFORMS[platform][0]
            path = self.artifacts / f"kernel-{platform}" / "bin" / exe
            path.parent.mkdir(parents=True)
            path.write_bytes(data)
        self.out = self.tmp / "dist"

    def build(self) -> int:
        return pw.main(
            [
                "build",
                "--pure-wheel",
                str(self.pure),
                "--artifacts",
                str(self.artifacts),
                "--out",
                str(self.out),
            ]
        )

    def test_writes_one_wheel_per_platform_and_the_pure_wheel(self):
        self.assertEqual(self.build(), 0)
        names = sorted(p.name for p in self.out.iterdir())
        self.assertEqual(len(names), len(pw.PLATFORMS) + 1)
        self.assertIn("khive-1.2.3-py3-none-any.whl", names)
        self.assertIn("khive-1.2.3-py3-none-manylinux_2_30_x86_64.whl", names)
        self.assertIn("khive-1.2.3-py3-none-win_amd64.whl", names)

    def test_platform_wheel_contents(self):
        self.assertEqual(self.build(), 0)
        wheel_path = self.out / "khive-1.2.3-py3-none-manylinux_2_28_aarch64.whl"
        with zipfile.ZipFile(wheel_path) as wheel:
            wheel_file = wheel.read("khive-1.2.3.dist-info/WHEEL").decode()
            self.assertIn("Tag: py3-none-manylinux_2_28_aarch64\n", wheel_file)
            self.assertIn("Root-Is-Purelib: false\n", wheel_file)
            self.assertNotIn("py3-none-any", wheel_file)
            script = wheel.getinfo("khive-1.2.3.data/scripts/kkernel")
            self.assertEqual(script.external_attr >> 16 & 0o777, 0o755)
            self.assertEqual(wheel.read(script), release_binaries()["linux-arm64"])
            rows = list(
                csv.reader(io.StringIO(wheel.read("khive-1.2.3.dist-info/RECORD").decode()))
            )
            self.assertEqual({r[0] for r in rows}, set(wheel.namelist()))
            self.assertIn(["khive-1.2.3.dist-info/RECORD", "", ""], rows)
        with zipfile.ZipFile(self.out / "khive-1.2.3-py3-none-win_amd64.whl") as wheel:
            self.assertIn("khive-1.2.3.data/scripts/kkernel.exe", wheel.namelist())

    def test_a_refused_binary_writes_no_wheels(self):
        bad = make_elf(X86, ["GLIBC_2.39"], GNU_LIBS)
        (self.artifacts / "kernel-linux-x64-gnu" / "bin" / "kkernel").write_bytes(bad)
        self.assertEqual(self.build(), 1)
        self.assertEqual(list(self.out.iterdir()), [])

    def test_a_missing_platform_is_refused(self):
        (self.artifacts / "kernel-win32-x64" / "bin" / "kkernel.exe").unlink()
        self.assertEqual(self.build(), 1)
        self.assertEqual(list(self.out.iterdir()), [])

    def test_verify_wheel_catches_a_tampered_member(self):
        self.assertEqual(self.build(), 0)
        wheel_path = self.out / "khive-1.2.3-py3-none-macosx_11_0_arm64.whl"
        tampered = self.tmp / "tampered.whl"
        with zipfile.ZipFile(wheel_path) as src, zipfile.ZipFile(tampered, "w") as dst:
            for info in src.infolist():
                payload = src.read(info)
                if info.filename == "khive/__init__.py":
                    payload += b"# changed\n"
                dst.writestr(info, payload)
        with self.assertRaises(SystemExit):
            pw.verify_wheel(tampered, "khive-1.2.3.data/scripts/kkernel")


class CheckGlibcCommandTests(unittest.TestCase):
    def run_check(self, data: bytes) -> int:
        with tempfile.TemporaryDirectory() as tmp:
            binary = pathlib.Path(tmp) / "kkernel"
            binary.write_bytes(data)
            return pw.main(["check-glibc", str(binary)])

    def test_passes_at_or_below_the_ceiling_and_fails_above(self):
        self.assertEqual(self.run_check(make_elf(ARM, ["GLIBC_2.17"], GNU_LIBS)), 0)
        self.assertEqual(self.run_check(make_elf(X86, ["GLIBC_2.39"], GNU_LIBS)), 1)
        self.assertEqual(self.run_check(make_macho(pw.MACHO_CPU["arm64"], (11, 0))), 1)


class ReleaseWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.workflow = (REPO_ROOT / ".github" / "workflows" / "release.yml").read_text()

    def test_every_linux_gnu_build_runs_the_glibc_check(self):
        self.assertIn(
            "if: ${{ endsWith(matrix.target, '-linux-gnu') }}\n"
            '        run: python3 scripts/python-wheels.py check-glibc "crates/target/${{ matrix.target }}/release/kkernel"',
            self.workflow,
        )
        gnu_rows = re.findall(r"target: (\S+-linux-gnu)\n\s+cross: (\w+)", self.workflow)
        self.assertEqual(
            sorted(gnu_rows),
            [("aarch64-unknown-linux-gnu", "true"), ("x86_64-unknown-linux-gnu", "true")],
        )

    def test_wheel_platforms_match_the_build_matrix(self):
        matrix = set(re.findall(r"- platform: (\S+)\n", self.workflow))
        self.assertEqual(matrix, set(pw.PLATFORMS))

    def test_dry_run_publishes_nothing(self):
        publish_all = self.workflow.index("  publish-all:\n")
        self.assertIn(
            "    if: ${{ !inputs.dry_run }}\n", self.workflow[publish_all : publish_all + 200]
        )
        upload = self.workflow.index("- name: Publish to PyPI\n")
        self.assertIn("if: ${{ !inputs.dry_run }}", self.workflow[upload : upload + 120])
        self.assertEqual(self.workflow.count("uses: pypa/gh-action-pypi-publish@"), 1)

    def test_python_version_matches_the_workspace(self):
        cargo = (REPO_ROOT / "crates" / "Cargo.toml").read_text()
        workspace = re.search(r'\[workspace\.package\][^\[]*?\nversion = "([^"]+)"', cargo).group(1)
        pyproject = (REPO_ROOT / "python" / "pyproject.toml").read_text()
        self.assertEqual(re.search(r'(?m)^name = "([^"]+)"', pyproject).group(1), "khive")
        self.assertEqual(re.search(r'(?m)^version = "([^"]+)"', pyproject).group(1), workspace)
        module = (REPO_ROOT / "python" / "khive" / "__init__.py").read_text()
        self.assertEqual(re.search(r'(?m)^__version__ = "([^"]+)"', module).group(1), workspace)


if __name__ == "__main__":
    unittest.main(verbosity=2)
