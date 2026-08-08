#!/usr/bin/env python3
"""Build kkernel and record the exact executable path reported by Cargo."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
import tempfile


RECEIPT_SCHEMA = 1
HEX_32 = re.compile(r"[0-9a-f]{32}\Z")
HEX_64 = re.compile(r"[0-9a-f]{64}\Z")


class BuildReceiptError(RuntimeError):
    pass


@dataclass(frozen=True)
class BuildReceipt:
    build_id: str
    artifact: Path
    artifact_hash: str


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical_json(payload: object) -> str:
    return json.dumps(payload, sort_keys=True, separators=(",", ":")) + "\n"


def atomic_write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.", dir=path.parent, text=True
    )
    temporary_path = Path(temporary_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary_path, path)
    finally:
        temporary_path.unlink(missing_ok=True)


def _canonical_absolute_path(raw: object, field: str) -> Path:
    if not isinstance(raw, str) or not raw or "\x00" in raw:
        raise BuildReceiptError(f"build receipt has an invalid {field}")
    path = Path(raw)
    if not path.is_absolute():
        raise BuildReceiptError(f"build receipt {field} must be absolute")
    canonical = path.resolve()
    if str(canonical) != raw:
        raise BuildReceiptError(f"build receipt {field} is not canonical")
    return canonical


def load_build_receipt(path: Path, *, verify_artifact: bool = True) -> BuildReceipt:
    try:
        raw = path.read_text(encoding="utf-8")
        payload = json.loads(raw)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise BuildReceiptError(f"cannot read build receipt {path}: {error}") from error
    if not isinstance(payload, dict):
        raise BuildReceiptError("build receipt must be a JSON object")
    expected_keys = {"artifact", "build_id", "schema", "sha256"}
    if set(payload) != expected_keys:
        raise BuildReceiptError("build receipt has an invalid field set")
    if raw != canonical_json(payload):
        raise BuildReceiptError("build receipt is not canonical JSON")
    schema = payload.get("schema")
    if (
        not isinstance(schema, int)
        or isinstance(schema, bool)
        or schema != RECEIPT_SCHEMA
    ):
        raise BuildReceiptError("build receipt has an unsupported schema")

    build_id = payload.get("build_id")
    if not isinstance(build_id, str) or HEX_32.fullmatch(build_id) is None:
        raise BuildReceiptError("build receipt has an invalid build_id")
    artifact_hash = payload.get("sha256")
    if not isinstance(artifact_hash, str) or HEX_64.fullmatch(artifact_hash) is None:
        raise BuildReceiptError("build receipt has an invalid SHA-256")
    artifact = _canonical_absolute_path(payload.get("artifact"), "artifact path")

    if verify_artifact:
        if not artifact.is_file():
            raise BuildReceiptError(f"build artifact is missing: {artifact}")
        if not os.access(artifact, os.X_OK):
            raise BuildReceiptError(f"build artifact is not executable: {artifact}")
        current_hash = sha256_file(artifact)
        if current_hash != artifact_hash:
            raise BuildReceiptError(
                "build artifact no longer matches the Cargo build receipt: "
                f"recorded={artifact_hash} current={current_hash}"
            )
    return BuildReceipt(build_id, artifact, artifact_hash)


def write_build_receipt(path: Path, artifact: Path) -> BuildReceipt:
    canonical_artifact = artifact.resolve()
    if not canonical_artifact.is_file():
        raise BuildReceiptError(
            f"Cargo-reported build artifact is missing: {canonical_artifact}"
        )
    if not os.access(canonical_artifact, os.X_OK):
        raise BuildReceiptError(
            f"Cargo-reported build artifact is not executable: {canonical_artifact}"
        )
    receipt = BuildReceipt(
        build_id=secrets.token_hex(16),
        artifact=canonical_artifact,
        artifact_hash=sha256_file(canonical_artifact),
    )
    payload = {
        "artifact": str(receipt.artifact),
        "build_id": receipt.build_id,
        "schema": RECEIPT_SCHEMA,
        "sha256": receipt.artifact_hash,
    }
    atomic_write(path, canonical_json(payload))
    return receipt


def cargo_build(
    *,
    cargo: str,
    manifest_path: Path,
    package: str,
    features: str,
) -> Path:
    command = [
        cargo,
        "build",
        "--release",
        "-p",
        package,
        "--features",
        features,
        "--manifest-path",
        str(manifest_path.resolve()),
        "--message-format=json-render-diagnostics",
    ]
    try:
        process = subprocess.Popen(
            command,
            stdout=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
    except OSError as error:
        raise BuildReceiptError(f"could not launch Cargo: {error}") from error
    if process.stdout is None:
        process.kill()
        process.wait()
        raise BuildReceiptError("could not capture Cargo artifact messages")

    artifacts: list[Path] = []
    parse_error: BuildReceiptError | None = None
    build_finished = False
    for line_number, line in enumerate(process.stdout, start=1):
        if not line.strip():
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError as error:
            if parse_error is None:
                parse_error = BuildReceiptError(
                    f"Cargo emitted invalid JSON on line {line_number}: {error}"
                )
            continue
        if not isinstance(message, dict):
            if parse_error is None:
                parse_error = BuildReceiptError(
                    f"Cargo emitted a non-object message on line {line_number}"
                )
            continue

        reason = message.get("reason")
        if reason == "compiler-message":
            diagnostic = message.get("message")
            if isinstance(diagnostic, dict):
                rendered = diagnostic.get("rendered")
                if isinstance(rendered, str) and rendered:
                    print(rendered, file=sys.stderr, end="" if rendered.endswith("\n") else "\n")
        elif reason == "compiler-artifact":
            target = message.get("target")
            executable = message.get("executable")
            if (
                isinstance(target, dict)
                and target.get("name") == package
                and isinstance(target.get("kind"), list)
                and "bin" in target["kind"]
                and isinstance(executable, str)
                and executable
            ):
                artifacts.append(Path(executable).resolve())
        elif reason == "build-finished":
            build_finished = message.get("success") is True

    return_code = process.wait()
    if return_code != 0:
        raise BuildReceiptError(f"Cargo build exited with status {return_code}")
    if parse_error is not None:
        raise parse_error
    if not build_finished:
        raise BuildReceiptError("Cargo did not report a successful build-finished event")
    if len(artifacts) != 1:
        rendered = ", ".join(str(path) for path in artifacts) or "none"
        raise BuildReceiptError(
            "Cargo did not report exactly one executable compiler-artifact event for "
            f"binary target {package!r}: {rendered}"
        )
    return artifacts[0]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build kkernel and record Cargo's exact executable artifact."
    )
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--manifest-path", type=Path, default=Path("crates/Cargo.toml"))
    parser.add_argument("--package", default="kkernel")
    parser.add_argument("--features", default="channel-email,channel-telegram")
    parser.add_argument("--receipt", required=True, type=Path)
    parser.add_argument(
        "--print-artifact",
        action="store_true",
        help="validate an existing receipt and print its artifact path",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    receipt_path = args.receipt.expanduser().resolve()
    try:
        if args.print_artifact:
            receipt = load_build_receipt(receipt_path)
            print(receipt.artifact)
            return 0

        receipt_path.parent.mkdir(parents=True, exist_ok=True)
        receipt_path.unlink(missing_ok=True)
        receipt_path.with_name(f"{receipt_path.name}.verified").unlink(missing_ok=True)
        if not args.cargo:
            raise BuildReceiptError("--cargo must not be empty")
        if not args.package:
            raise BuildReceiptError("--package must not be empty")
        artifact = cargo_build(
            cargo=args.cargo,
            manifest_path=args.manifest_path,
            package=args.package,
            features=args.features,
        )
        receipt = write_build_receipt(receipt_path, artifact)
        print(
            f"Cargo artifact: {receipt.artifact} "
            f"(sha256={receipt.artifact_hash}, build_id={receipt.build_id})"
        )
        return 0
    except (BuildReceiptError, OSError, ValueError) as error:
        print(f"build-local: ERROR: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
