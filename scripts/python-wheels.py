#!/usr/bin/env python3
"""Package the release kkernel binaries as platform wheels of the khive Python package.

The pure-Python client in python/ builds once as a py3-none-any wheel. For each
release platform this script copies that wheel, adds the platform's kkernel binary
to the wheel's scripts directory (an installer places it in the environment's
bin/ or Scripts/ directory, next to the interpreter), and retags the wheel for
that platform. The pure wheel is published beside the platform wheels: installers
prefer a platform wheel, and the pure one serves platforms without a binary and
clients that only talk to a remote deployment.

Every platform tag is read from the binary itself, never from a table: the macOS
minimum version from the Mach-O load commands, the glibc floor from the ELF
version requirements. A wheel therefore never claims to run on a system its
binary cannot start on.

Subcommands:
  check-glibc BINARY
      Print the glibc floor of a Linux gnu binary and fail when it is newer than
      MAX_GLIBC or when the binary links a library outside the manylinux set. The
      release workflow runs this on every Linux gnu build, so the npm packages and
      the wheels share the same floor.
  build --pure-wheel WHEEL --artifacts DIR --out DIR
      Write one platform wheel per release platform. DIR holds the release
      artifacts as kernel-<platform>/bin/kkernel[.exe].

Standard library only.
"""

from __future__ import annotations

import argparse
import base64
import csv
import hashlib
import io
import pathlib
import re
import shutil
import struct
import sys
import zipfile

# Newest glibc a Linux gnu binary may require. 2.31 is the glibc of Debian 11 and
# Ubuntu 20.04; raising it drops every system older than the new value.
MAX_GLIBC = (2, 31)

# Shared libraries a manylinux binary may link (the core of the PEP 600 policy).
MANYLINUX_LIBS = frozenset(
    {
        "libc.so.6",
        "libm.so.6",
        "libdl.so.2",
        "librt.so.1",
        "libpthread.so.0",
        "libgcc_s.so.1",
        "libstdc++.so.6",
        "libresolv.so.2",
        "libutil.so.1",
        "ld-linux-x86-64.so.2",
        "ld-linux-aarch64.so.1",
    }
)

ELF_MACHINE = {"x86_64": 0x3E, "aarch64": 0xB7}
MACHO_CPU = {"x86_64": 0x01000007, "arm64": 0x0100000C}
PE_MACHINE_AMD64 = 0x8664

SHT_DYNAMIC = 6
SHT_GNU_VERNEED = 0x6FFFFFFE
DT_NEEDED = 1
LC_VERSION_MIN_MACOSX = 0x24
LC_BUILD_VERSION = 0x32

# Release platform -> (binary file name, binary format, architecture). The names
# match the build-platform matrix in .github/workflows/release.yml.
PLATFORMS = {
    "darwin-arm64": ("kkernel", "macho", "arm64"),
    "darwin-x64": ("kkernel", "macho", "x86_64"),
    "linux-x64-gnu": ("kkernel", "elf-gnu", "x86_64"),
    "linux-x64-musl": ("kkernel", "elf-static", "x86_64"),
    "linux-arm64": ("kkernel", "elf-gnu", "aarch64"),
    "win32-x64": ("kkernel.exe", "pe", "amd64"),
}

_GLIBC_RE = re.compile(r"^GLIBC_(\d+)\.(\d+)(?:\.\d+)?$")


class BinaryError(Exception):
    """The binary does not have the format, architecture or floor its platform needs."""


def _cstr(data: bytes, offset: int) -> str:
    end = data.index(b"\0", offset)
    return data[offset:end].decode("ascii")


def _elf_sections(data: bytes) -> list[tuple[int, int, int, int, int]]:
    """Return (type, offset, size, link, info) for every section of a 64-bit LE ELF."""
    if data[:4] != b"\x7fELF":
        raise BinaryError("not an ELF file")
    if data[4] != 2 or data[5] != 1:
        raise BinaryError("not a 64-bit little-endian ELF file")
    (shoff,) = struct.unpack_from("<Q", data, 0x28)
    shentsize, shnum = struct.unpack_from("<HH", data, 0x3A)
    if shoff == 0 or shnum == 0:
        raise BinaryError("ELF file has no section headers")
    sections = []
    for i in range(shnum):
        base = shoff + i * shentsize
        (sh_type,) = struct.unpack_from("<I", data, base + 4)
        sh_offset, sh_size = struct.unpack_from("<QQ", data, base + 24)
        sh_link, sh_info = struct.unpack_from("<II", data, base + 40)
        sections.append((sh_type, sh_offset, sh_size, sh_link, sh_info))
    return sections


def elf_glibc_floor(data: bytes, arch: str) -> tuple[tuple[int, int] | None, list[str]]:
    """Return the newest GLIBC_x.y version the binary requires and its DT_NEEDED list.

    The floor is None when the binary requires no versioned glibc symbol (a static
    binary).
    """
    (machine,) = struct.unpack_from("<H", data, 18)
    if machine != ELF_MACHINE[arch]:
        raise BinaryError(f"ELF machine {machine:#x} is not {arch}")
    sections = _elf_sections(data)
    floor: tuple[int, int] | None = None
    needed: list[str] = []
    for sh_type, offset, size, link, info in sections:
        if sh_type == SHT_GNU_VERNEED:
            strtab = sections[link][1]
            entry = offset
            for _ in range(info):
                _, vn_cnt, _, vn_aux, vn_next = struct.unpack_from("<HHIII", data, entry)
                aux = entry + vn_aux
                for _ in range(vn_cnt):
                    _, _, _, vna_name, vna_next = struct.unpack_from("<IHHII", data, aux)
                    match = _GLIBC_RE.match(_cstr(data, strtab + vna_name))
                    if match:
                        version = (int(match.group(1)), int(match.group(2)))
                        floor = version if floor is None else max(floor, version)
                    aux += vna_next
                entry += vn_next
        elif sh_type == SHT_DYNAMIC:
            strtab = sections[link][1]
            for pos in range(offset, offset + size, 16):
                d_tag, d_val = struct.unpack_from("<qQ", data, pos)
                if d_tag == 0:
                    break
                if d_tag == DT_NEEDED:
                    needed.append(_cstr(data, strtab + d_val))
    return floor, needed


def check_gnu_binary(data: bytes, arch: str) -> tuple[int, int]:
    """Return the glibc floor of a Linux gnu binary, refusing one wheels cannot ship."""
    floor, needed = elf_glibc_floor(data, arch)
    if floor is None:
        raise BinaryError("a Linux gnu binary requires no GLIBC version; expected at least one")
    if floor > MAX_GLIBC:
        raise BinaryError(
            f"requires glibc {floor[0]}.{floor[1]}, newer than the "
            f"{MAX_GLIBC[0]}.{MAX_GLIBC[1]} ceiling"
        )
    outside = sorted(set(needed) - MANYLINUX_LIBS)
    if outside:
        raise BinaryError(f"links libraries outside the manylinux set: {', '.join(outside)}")
    return floor


def check_static_binary(data: bytes, arch: str) -> None:
    floor, needed = elf_glibc_floor(data, arch)
    if floor is not None or needed:
        raise BinaryError(f"expected a static binary, found shared libraries {needed}")


def macho_minimum(data: bytes, arch: str) -> tuple[int, int]:
    """Return the minimum macOS version recorded in a thin 64-bit Mach-O binary."""
    magic, cputype = struct.unpack_from("<Ii", data, 0)
    if magic != 0xFEEDFACF:
        raise BinaryError("not a thin 64-bit Mach-O file")
    if cputype != MACHO_CPU[arch]:
        raise BinaryError(f"Mach-O CPU type {cputype:#x} is not {arch}")
    (ncmds,) = struct.unpack_from("<I", data, 16)
    offset = 32
    for _ in range(ncmds):
        cmd, cmdsize = struct.unpack_from("<II", data, offset)
        if cmd == LC_BUILD_VERSION:
            (version,) = struct.unpack_from("<I", data, offset + 12)
            return version >> 16, (version >> 8) & 0xFF
        if cmd == LC_VERSION_MIN_MACOSX:
            (version,) = struct.unpack_from("<I", data, offset + 8)
            return version >> 16, (version >> 8) & 0xFF
        offset += cmdsize
    raise BinaryError("Mach-O file records no minimum macOS version")


def check_pe_amd64(data: bytes) -> None:
    if data[:2] != b"MZ":
        raise BinaryError("not a PE file")
    (pe_offset,) = struct.unpack_from("<I", data, 0x3C)
    if data[pe_offset : pe_offset + 4] != b"PE\0\0":
        raise BinaryError("PE signature missing")
    (machine,) = struct.unpack_from("<H", data, pe_offset + 4)
    if machine != PE_MACHINE_AMD64:
        raise BinaryError(f"PE machine {machine:#x} is not amd64")


def platform_tag(platform: str, data: bytes) -> str:
    """Return the wheel platform tag for a release platform, read from its binary."""
    _, fmt, arch = PLATFORMS[platform]
    if fmt == "macho":
        major, minor = macho_minimum(data, arch)
        if major >= 11:
            # Installers match macOS 11 and later only as <major>_0.
            if minor != 0:
                raise BinaryError(
                    f"minimum macOS {major}.{minor} has no wheel tag; build for {major}.0"
                )
            return f"macosx_{major}_0_{arch}"
        return f"macosx_{major}_{minor}_{arch}"
    if fmt == "elf-gnu":
        major, minor = check_gnu_binary(data, arch)
        return f"manylinux_{major}_{minor}_{arch}"
    if fmt == "elf-static":
        check_static_binary(data, arch)
        return f"musllinux_1_2_{arch}"
    check_pe_amd64(data)
    return "win_amd64"


def _record_hash(data: bytes) -> str:
    digest = hashlib.sha256(data).digest()
    return "sha256=" + base64.urlsafe_b64encode(digest).rstrip(b"=").decode("ascii")


def _parse_pure_wheel_name(path: pathlib.Path) -> tuple[str, str]:
    parts = path.name[: -len(".whl")].split("-")
    if not path.name.endswith(".whl") or len(parts) != 5 or parts[2:] != ["py3", "none", "any"]:
        raise SystemExit(f"{path.name}: expected <name>-<version>-py3-none-any.whl")
    return parts[0], parts[1]


def build_platform_wheel(
    pure: pathlib.Path, binary_name: str, binary: bytes, tag: str, out_dir: pathlib.Path
) -> pathlib.Path:
    name, version = _parse_pure_wheel_name(pure)
    dist_info = f"{name}-{version}.dist-info"
    data_dir = f"{name}-{version}.data"
    out = out_dir / f"{name}-{version}-py3-none-{tag}.whl"

    with zipfile.ZipFile(pure) as src:
        wheel_info = src.getinfo(f"{dist_info}/WHEEL")
        wheel_lines = src.read(wheel_info).decode("utf-8").splitlines()
        if "Tag: py3-none-any" not in wheel_lines or "Root-Is-Purelib: true" not in wheel_lines:
            raise SystemExit(f"{pure.name}: WHEEL is not a pure py3-none-any wheel")
        retagged = [
            line for line in wheel_lines if not line.startswith(("Tag:", "Root-Is-Purelib:"))
        ]
        retagged += ["Root-Is-Purelib: false", f"Tag: py3-none-{tag}"]
        new_wheel = ("\n".join(retagged) + "\n").encode("utf-8")

        rows: list[list[str]] = []
        with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as dst:
            for info in src.infolist():
                if info.filename in (f"{dist_info}/WHEEL", f"{dist_info}/RECORD"):
                    continue
                payload = src.read(info)
                dst.writestr(info, payload)
                rows.append([info.filename, _record_hash(payload), str(len(payload))])

            script = zipfile.ZipInfo(f"{data_dir}/scripts/{binary_name}", wheel_info.date_time)
            script.compress_type = zipfile.ZIP_DEFLATED
            script.external_attr = (0o100755 & 0xFFFF) << 16
            dst.writestr(script, binary)
            rows.append([script.filename, _record_hash(binary), str(len(binary))])

            new_wheel_info = zipfile.ZipInfo(f"{dist_info}/WHEEL", wheel_info.date_time)
            new_wheel_info.compress_type = zipfile.ZIP_DEFLATED
            new_wheel_info.external_attr = wheel_info.external_attr
            dst.writestr(new_wheel_info, new_wheel)
            rows.append([new_wheel_info.filename, _record_hash(new_wheel), str(len(new_wheel))])

            record = io.StringIO()
            writer = csv.writer(record, lineterminator="\n")
            writer.writerows(rows)
            writer.writerow([f"{dist_info}/RECORD", "", ""])
            record_info = zipfile.ZipInfo(f"{dist_info}/RECORD", wheel_info.date_time)
            record_info.compress_type = zipfile.ZIP_DEFLATED
            record_info.external_attr = wheel_info.external_attr
            dst.writestr(record_info, record.getvalue().encode("utf-8"))

    verify_wheel(out, f"{data_dir}/scripts/{binary_name}")
    return out


def verify_wheel(path: pathlib.Path, script_path: str) -> None:
    """Re-read a written wheel: every RECORD hash matches and the binary is executable."""
    with zipfile.ZipFile(path) as wheel:
        names = set(wheel.namelist())
        record_name = next(n for n in names if n.endswith(".dist-info/RECORD"))
        rows = list(csv.reader(io.StringIO(wheel.read(record_name).decode("utf-8"))))
        recorded = {row[0] for row in rows}
        if recorded != names:
            raise SystemExit(f"{path.name}: RECORD does not list exactly the archive members")
        for filename, digest, size in rows:
            if filename == record_name:
                continue
            payload = wheel.read(filename)
            if digest != _record_hash(payload) or size != str(len(payload)):
                raise SystemExit(f"{path.name}: RECORD mismatch for {filename}")
        mode = wheel.getinfo(script_path).external_attr >> 16
        if mode & 0o111 != 0o111:
            raise SystemExit(f"{path.name}: {script_path} is not executable")


def cmd_check_glibc(args: argparse.Namespace) -> int:
    data = pathlib.Path(args.binary).read_bytes()
    machine = struct.unpack_from("<H", data, 18)[0] if data[:4] == b"\x7fELF" else None
    arch = next((a for a, m in ELF_MACHINE.items() if m == machine), None)
    if arch is None:
        print(f"{args.binary}: not an x86_64 or aarch64 ELF binary", file=sys.stderr)
        return 1
    try:
        major, minor = check_gnu_binary(data, arch)
    except BinaryError as exc:
        print(f"{args.binary}: {exc}", file=sys.stderr)
        return 1
    print(
        f"{args.binary}: {arch} requires glibc {major}.{minor} "
        f"(ceiling {MAX_GLIBC[0]}.{MAX_GLIBC[1]})"
    )
    return 0


def cmd_build(args: argparse.Namespace) -> int:
    pure = pathlib.Path(args.pure_wheel)
    artifacts = pathlib.Path(args.artifacts)
    out_dir = pathlib.Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)
    missing = [
        p
        for p, (exe, _, _) in PLATFORMS.items()
        if not (artifacts / f"kernel-{p}" / "bin" / exe).is_file()
    ]
    if missing:
        print(f"missing release binaries for: {', '.join(missing)}", file=sys.stderr)
        return 1
    # Read every tag before writing anything, so a refused binary leaves no
    # partial set of wheels behind.
    tagged = []
    for platform, (exe, _, _) in PLATFORMS.items():
        binary = (artifacts / f"kernel-{platform}" / "bin" / exe).read_bytes()
        try:
            tagged.append((platform, exe, binary, platform_tag(platform, binary)))
        except BinaryError as exc:
            print(f"{platform}: {exc}", file=sys.stderr)
            return 1
    for platform, exe, binary, tag in tagged:
        wheel = build_platform_wheel(pure, exe, binary, tag, out_dir)
        print(f"{platform}: {wheel.name}")
    shutil.copy2(pure, out_dir / pure.name)
    print(f"pure: {pure.name}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    check = sub.add_parser("check-glibc", help="check a Linux gnu binary's glibc floor")
    check.add_argument("binary")
    check.set_defaults(func=cmd_check_glibc)
    build = sub.add_parser("build", help="write the platform wheels")
    build.add_argument("--pure-wheel", required=True)
    build.add_argument("--artifacts", required=True)
    build.add_argument("--out", required=True)
    build.set_defaults(func=cmd_build)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
