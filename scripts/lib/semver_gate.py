#!/usr/bin/env python3
"""Validate and summarize the local cargo-semver-checks release gate."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


# v0.50.0 adds rustdoc JSON v60/v61 support, covering the repository's
# current Rust 1.98 toolchain. Future format changes must raise this floor.
MIN_CHECKER_VERSION = (0, 50, 0)
_VERSION_RE = re.compile(r"\bcargo-semver-checks\s+v?(\d+)\.(\d+)\.(\d+)\b")
_ANSI_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
_CHECKED_RE = re.compile(r"\bChecked\s+\[[^\]]+\]\s+(\d+)\s+checks:")


def parse_checker_version(output: str) -> tuple[int, int, int]:
    match = _VERSION_RE.search(output)
    if match is None:
        raise ValueError(
            "could not parse `cargo-semver-checks --version` output: "
            f"{output.strip()!r}"
        )
    return tuple(int(part) for part in match.groups())


def validate_checker_version(output: str) -> tuple[int, int, int]:
    version = parse_checker_version(output)
    if version < MIN_CHECKER_VERSION:
        required = ".".join(str(part) for part in MIN_CHECKER_VERSION)
        actual = ".".join(str(part) for part in version)
        raise ValueError(
            f"cargo-semver-checks {actual} is too old; khive requires >= {required} "
            "for the current rustdoc JSON generation"
        )
    return version


def summarize_log(text: str) -> tuple[int, int]:
    clean = _ANSI_RE.sub("", text)
    selected_checks = [int(match.group(1)) for match in _CHECKED_RE.finditer(clean)]
    if not selected_checks:
        raise ValueError(
            "cargo-semver-checks completed without any recognizable per-crate "
            "`Checked ... N checks:` records"
        )
    return len(selected_checks), sum(selected_checks)


def _check_version(raw_version: str) -> int:
    try:
        version = validate_checker_version(raw_version)
    except ValueError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    actual = ".".join(str(part) for part in version)
    print(f"    cargo-semver-checks {actual} compatibility floor OK")
    return 0


def _summarize(path: Path) -> int:
    try:
        crates, evaluated = summarize_log(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        print(f"ERROR: cannot summarize SemVer gate: {exc}", file=sys.stderr)
        return 1

    if evaluated == 0:
        print(
            f"    SemVer gate completed: {crates} crates checked, 0 lints "
            "evaluated — VACUOUS PASS for this version delta"
        )
    else:
        print(
            f"    SemVer gate completed: {crates} crates checked, "
            f"{evaluated} lints evaluated"
        )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    version_parser = subparsers.add_parser("check-version")
    version_parser.add_argument("version_output")

    summary_parser = subparsers.add_parser("summarize")
    summary_parser.add_argument("log", type=Path)

    args = parser.parse_args(argv)
    if args.command == "check-version":
        return _check_version(args.version_output)
    return _summarize(args.log)


if __name__ == "__main__":
    raise SystemExit(main())
