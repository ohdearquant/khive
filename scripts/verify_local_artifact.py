#!/usr/bin/env python3
"""Fail-closed runtime verification for a freshly built kkernel artifact."""

from __future__ import annotations

import argparse
from collections import Counter
import json
import math
import os
from pathlib import Path
import re
import select
import shlex
import subprocess
import sys
import tempfile
import time

from build_local_artifact import (
    BuildReceipt,
    BuildReceiptError,
    atomic_write,
    canonical_json,
    load_build_receipt,
    sha256_file,
)


STAMP_SCHEMA = 1
MAX_VERB_COUNT = 1_000_000
MCP_PROTOCOL_VERSION = "2024-11-05"
HEX_32 = re.compile(r"[0-9a-f]{32}\Z")
HEX_64 = re.compile(r"[0-9a-f]{64}\Z")


class VerificationError(RuntimeError):
    pass


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be at least 1")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed) or parsed <= 0:
        raise argparse.ArgumentTypeError("must be a finite number greater than 0")
    return parsed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Probe verbs() on an exact kkernel build artifact before installation."
    )
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--artifact", type=Path)
    source.add_argument("--build-receipt", type=Path)
    parser.add_argument("--packs")
    parser.add_argument("--min-verbs", required=True, type=positive_int)
    parser.add_argument("--stamp", type=Path)
    parser.add_argument(
        "--inspect-stamp",
        type=Path,
        help="validate a verified build receipt and emit shell-safe install fields",
    )
    parser.add_argument("--timeout-seconds", type=positive_float, default=120.0)
    args = parser.parse_args()
    if args.inspect_stamp is not None:
        if args.build_receipt is None:
            parser.error("--inspect-stamp requires --build-receipt")
        if args.artifact is not None or args.packs is not None or args.stamp is not None:
            parser.error(
                "--inspect-stamp cannot be combined with --artifact, --packs, or --stamp"
            )
    else:
        if args.artifact is None and args.build_receipt is None:
            parser.error("one of --artifact or --build-receipt is required")
        if args.packs is None:
            parser.error("--packs is required when verifying an artifact")
    return args


def parse_packs(raw: str) -> list[str]:
    packs = [part.strip() for part in raw.split(",")]
    if not packs or any(not pack for pack in packs):
        raise VerificationError("--packs must be a non-empty comma-separated list")
    if len(set(packs)) != len(packs):
        raise VerificationError("--packs contains duplicate names")
    return packs


def validate_probe(payload: object, required_packs: list[str], minimum: int) -> int:
    if not isinstance(payload, dict):
        raise VerificationError("probe output must be a JSON object")
    results = payload.get("results")
    if not isinstance(results, list) or len(results) != 1:
        raise VerificationError("probe output must contain exactly one result")
    entry = results[0]
    if not isinstance(entry, dict) or entry.get("ok") is not True:
        raise VerificationError("verbs() did not return a successful result")
    if entry.get("tool") != "verbs":
        raise VerificationError("probe result does not identify the verbs() operation")
    if "result" not in entry or "error" in entry:
        raise VerificationError("verbs() returned an invalid result/error shape")
    result = entry["result"]
    if not isinstance(result, dict):
        raise VerificationError("verbs() result must be an object")

    verbs = result.get("verbs")
    total = result.get("total")
    pack_counts = result.get("pack_counts")
    if not isinstance(verbs, list):
        raise VerificationError("verbs() result is missing the verb catalog")
    if (
        not isinstance(total, int)
        or isinstance(total, bool)
        or total < 0
        or total > MAX_VERB_COUNT
    ):
        raise VerificationError("verbs() result has an invalid total")
    if not isinstance(pack_counts, dict):
        raise VerificationError("verbs() result is missing pack_counts")
    if len(verbs) != total:
        raise VerificationError(
            f"verbs() total {total} does not match catalog length {len(verbs)}"
        )

    parsed_counts: dict[str, int] = {}
    for pack, count in pack_counts.items():
        if not isinstance(pack, str) or not pack.strip() or pack != pack.strip():
            raise VerificationError("verbs() pack_counts contains an invalid pack name")
        if not isinstance(count, int) or isinstance(count, bool) or count < 0:
            raise VerificationError(
                f"verbs() pack_counts contains an invalid count for {pack!r}"
            )
        parsed_counts[pack] = count

    missing = [pack for pack in required_packs if pack not in parsed_counts]
    if missing:
        raise VerificationError(
            "verbs() omitted requested packs: " + ", ".join(sorted(missing))
        )
    unexpected = sorted(set(parsed_counts) - set(required_packs))
    if unexpected:
        raise VerificationError(
            "verbs() returned packs outside the requested vocabulary: "
            + ", ".join(unexpected)
        )
    if sum(parsed_counts.values()) != total:
        raise VerificationError(
            f"verbs() pack_counts sum {sum(parsed_counts.values())} does not equal total {total}"
        )

    observed: Counter[str] = Counter()
    observed_verbs: set[str] = set()
    for index, verb in enumerate(verbs):
        pack_name = verb.get("pack") if isinstance(verb, dict) else None
        if (
            not isinstance(verb, dict)
            or not isinstance(pack_name, str)
            or not pack_name.strip()
            or pack_name != pack_name.strip()
        ):
            raise VerificationError(
                f"verbs() catalog entry {index} does not name its pack"
            )
        verb_name = verb.get("verb")
        if (
            not isinstance(verb_name, str)
            or not verb_name.strip()
            or verb_name != verb_name.strip()
        ):
            raise VerificationError(
                f"verbs() catalog entry {index} does not name an exact nonblank verb"
            )
        if verb_name in observed_verbs:
            raise VerificationError(
                f"verbs() catalog contains duplicate verb {verb_name!r}"
            )
        observed_verbs.add(verb_name)
        observed[pack_name] += 1
    unknown = sorted(set(observed) - set(parsed_counts))
    if unknown:
        raise VerificationError(
            "verbs() catalog names packs absent from pack_counts: " + ", ".join(unknown)
        )
    for pack, count in parsed_counts.items():
        if observed[pack] != count:
            raise VerificationError(
                f"verbs() pack_counts[{pack!r}]={count}, catalog contains {observed[pack]}"
            )

    if total < minimum:
        raise VerificationError(
            f"artifact registered {total} verbs; expected at least {minimum}"
        )
    return total


def initialize_request() -> dict[str, object]:
    return {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "khive-local-build-verifier",
                "version": "1",
            },
        },
    }


def initialized_notification() -> dict[str, object]:
    return {
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {},
    }


def tools_call_request() -> dict[str, object]:
    return {
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "request",
            "arguments": {"ops": "verbs()", "format": "json"},
        },
    }


def _probe_timeout(timeout_seconds: float) -> VerificationError:
    return VerificationError(
        f"artifact probe timed out after {timeout_seconds:g} seconds"
    )


def _send_mcp_message(
    process: subprocess.Popen[bytes], message: dict[str, object], stage: str
) -> None:
    if process.stdin is None:
        raise VerificationError(f"artifact probe has no stdin for {stage}")
    encoded = (json.dumps(message, separators=(",", ":")) + "\n").encode("utf-8")
    try:
        process.stdin.write(encoded)
        process.stdin.flush()
    except (BrokenPipeError, OSError) as error:
        raise VerificationError(
            f"artifact probe closed stdin before {stage}: {error}"
        ) from error


def _read_mcp_message(
    process: subprocess.Popen[bytes],
    buffered: bytearray,
    *,
    deadline: float,
    timeout_seconds: float,
    stage: str,
) -> object:
    if process.stdout is None:
        raise VerificationError(f"artifact probe has no stdout for {stage}")
    while b"\n" not in buffered:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise _probe_timeout(timeout_seconds)
        try:
            ready, _, _ = select.select([process.stdout], [], [], remaining)
        except InterruptedError:
            continue
        except (OSError, ValueError) as error:
            raise VerificationError(
                f"artifact probe could not read the {stage} response: {error}"
            ) from error
        if not ready:
            raise _probe_timeout(timeout_seconds)
        try:
            chunk = os.read(process.stdout.fileno(), 64 * 1024)
        except OSError as error:
            raise VerificationError(
                f"artifact probe could not read the {stage} response: {error}"
            ) from error
        if not chunk:
            if buffered:
                raise VerificationError(
                    f"artifact probe closed stdout with an incomplete {stage} response"
                )
            raise VerificationError(
                f"artifact probe closed stdout before the {stage} response"
            )
        buffered.extend(chunk)

    raw_line, _, remainder = buffered.partition(b"\n")
    buffered[:] = remainder
    try:
        line = raw_line.decode("utf-8")
    except UnicodeDecodeError as error:
        raise VerificationError(
            f"artifact probe returned non-UTF-8 JSON for {stage}: {error}"
        ) from error
    if not line.strip():
        raise VerificationError(f"artifact probe returned a blank {stage} response")
    try:
        return json.loads(line)
    except json.JSONDecodeError as error:
        raise VerificationError(
            f"artifact probe returned invalid JSON for {stage}: {error}"
        ) from error


def _successful_mcp_result(response: object, expected_id: int, stage: str) -> object:
    if not isinstance(response, dict) or response.get("jsonrpc") != "2.0":
        raise VerificationError(
            f"artifact probe returned an invalid JSON-RPC {stage} response"
        )
    response_id = response.get("id")
    if (
        not isinstance(response_id, int)
        or isinstance(response_id, bool)
        or response_id != expected_id
    ):
        raise VerificationError(
            f"artifact probe returned the wrong id for {stage}: {response_id!r}"
        )
    has_result = "result" in response
    has_error = "error" in response
    if has_result == has_error:
        raise VerificationError(
            f"artifact probe returned an invalid {stage} result/error shape"
        )
    if has_error:
        raise VerificationError(
            f"artifact probe {stage} returned an MCP error: {response['error']!r}"
        )
    return response["result"]


def validate_initialize_response(response: object) -> None:
    result = _successful_mcp_result(response, 1, "initialize")
    if not isinstance(result, dict):
        raise VerificationError("artifact probe returned an invalid initialize result")
    if result.get("protocolVersion") != MCP_PROTOCOL_VERSION:
        raise VerificationError(
            "artifact probe initialize result has an invalid protocolVersion"
        )
    if not isinstance(result.get("capabilities"), dict):
        raise VerificationError(
            "artifact probe initialize result has invalid capabilities"
        )
    server_info = result.get("serverInfo")
    if not isinstance(server_info, dict):
        raise VerificationError(
            "artifact probe initialize result is missing serverInfo"
        )
    if server_info.get("name") != "khive-mcp":
        raise VerificationError(
            "artifact probe initialize result has an unexpected serverInfo.name"
        )
    version = server_info.get("version")
    if not isinstance(version, str) or not version.strip():
        raise VerificationError(
            "artifact probe initialize result has an invalid serverInfo.version"
        )


def parse_mcp_probe(response: object) -> object:
    result = _successful_mcp_result(response, 2, "tools/call")
    if not isinstance(result, dict):
        raise VerificationError("artifact probe returned an invalid MCP tool result")
    is_error = result.get("isError")
    if is_error is not None and is_error is not False:
        raise VerificationError("artifact probe returned an MCP tool error")
    content = result.get("content")
    if not isinstance(content, list) or len(content) != 1:
        raise VerificationError("artifact probe must return exactly one text content block")
    block = content[0]
    if (
        not isinstance(block, dict)
        or block.get("type") != "text"
        or not isinstance(block.get("text"), str)
    ):
        raise VerificationError("artifact probe returned a malformed text content block")
    try:
        return json.loads(block["text"])
    except json.JSONDecodeError as error:
        raise VerificationError(f"verbs() content is not valid JSON: {error}") from error


def _stop_probe(process: subprocess.Popen[bytes]) -> None:
    if process.stdin is not None:
        try:
            process.stdin.close()
        except OSError:
            pass
        process.stdin = None
    if process.poll() is None:
        try:
            process.kill()
        except ProcessLookupError:
            pass
    if process.stdout is not None:
        process.stdout.close()
    try:
        process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        # SIGKILL normally makes this immediate. Do not replace the verifier's
        # bounded protocol failure with an unbounded cleanup wait.
        pass


def _diagnostic_tail(diagnostic: object) -> str:
    diagnostic.flush()
    diagnostic.seek(0)
    raw = diagnostic.read()
    if not isinstance(raw, bytes):
        return "no diagnostic output"
    rendered = raw.decode("utf-8", errors="replace").strip()
    return rendered[-2000:] or "no diagnostic output"


def run_mcp_probe(
    command: list[str],
    *,
    cwd: Path,
    environment: dict[str, str],
    timeout_seconds: float,
) -> object:
    """Perform the ordered MCP handshake and verbs() call under one deadline."""
    deadline = time.monotonic() + timeout_seconds
    with tempfile.TemporaryFile(mode="w+b") as diagnostic:
        try:
            process = subprocess.Popen(
                command,
                cwd=cwd,
                env=environment,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=diagnostic,
            )
        except OSError as error:
            raise VerificationError(f"artifact probe could not run: {error}") from error

        buffered = bytearray()
        try:
            _send_mcp_message(process, initialize_request(), "initialize request")
            initialize = _read_mcp_message(
                process,
                buffered,
                deadline=deadline,
                timeout_seconds=timeout_seconds,
                stage="initialize",
            )
            validate_initialize_response(initialize)

            _send_mcp_message(
                process,
                initialized_notification(),
                "initialized notification",
            )
            _send_mcp_message(process, tools_call_request(), "tools/call request")
            tools_response = _read_mcp_message(
                process,
                buffered,
                deadline=deadline,
                timeout_seconds=timeout_seconds,
                stage="tools/call",
            )
            payload = parse_mcp_probe(tools_response)

            if process.stdin is not None:
                process.stdin.close()
                process.stdin = None
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise _probe_timeout(timeout_seconds)
            try:
                stdout_tail, _ = process.communicate(timeout=remaining)
            except subprocess.TimeoutExpired as error:
                raise _probe_timeout(timeout_seconds) from error
            if process.returncode != 0:
                raise VerificationError(
                    f"artifact probe exited {process.returncode}: "
                    f"{_diagnostic_tail(diagnostic)}"
                )
            if (bytes(buffered) + stdout_tail).strip():
                raise VerificationError(
                    "artifact probe returned unexpected output after tools/call"
                )
            return payload
        except BaseException:
            _stop_probe(process)
            raise


def write_stamp(
    path: Path,
    artifact: Path,
    artifact_hash: str,
    verb_count: int,
    build_id: str | None,
) -> None:
    if not path.parent.is_dir():
        raise VerificationError(f"stamp directory does not exist: {path.parent}")
    if not 1 <= verb_count <= MAX_VERB_COUNT:
        raise VerificationError("verified verb count is outside the supported range")
    payload = {
        "artifact": str(artifact),
        "build_id": build_id,
        "schema": STAMP_SCHEMA,
        "sha256": artifact_hash,
        "verb_count": verb_count,
    }
    atomic_write(path, canonical_json(payload))


def load_stamp(path: Path) -> tuple[Path, str, int, str | None]:
    try:
        raw = path.read_text(encoding="utf-8")
        payload = json.loads(raw)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise VerificationError(f"cannot read verification stamp {path}: {error}") from error
    if not isinstance(payload, dict):
        raise VerificationError("verification stamp must be a JSON object")
    expected_keys = {"artifact", "build_id", "schema", "sha256", "verb_count"}
    if set(payload) != expected_keys:
        raise VerificationError("verification stamp has an invalid field set")
    if raw != canonical_json(payload):
        raise VerificationError("verification stamp is not canonical JSON")
    schema = payload.get("schema")
    if (
        not isinstance(schema, int)
        or isinstance(schema, bool)
        or schema != STAMP_SCHEMA
    ):
        raise VerificationError("verification stamp has an unsupported schema")

    raw_artifact = payload.get("artifact")
    if not isinstance(raw_artifact, str) or not raw_artifact or "\x00" in raw_artifact:
        raise VerificationError("verification stamp has an invalid artifact path")
    artifact = Path(raw_artifact)
    if not artifact.is_absolute() or str(artifact.resolve()) != raw_artifact:
        raise VerificationError("verification stamp artifact path is not canonical")

    artifact_hash = payload.get("sha256")
    if not isinstance(artifact_hash, str) or HEX_64.fullmatch(artifact_hash) is None:
        raise VerificationError("verification stamp has an invalid SHA-256")
    verb_count = payload.get("verb_count")
    if (
        not isinstance(verb_count, int)
        or isinstance(verb_count, bool)
        or not 1 <= verb_count <= MAX_VERB_COUNT
    ):
        raise VerificationError("verification stamp has an invalid bounded verb count")
    build_id = payload.get("build_id")
    if build_id is not None and (
        not isinstance(build_id, str) or HEX_32.fullmatch(build_id) is None
    ):
        raise VerificationError("verification stamp has an invalid build_id")
    return artifact, artifact_hash, verb_count, build_id


def inspect_stamp(args: argparse.Namespace) -> str:
    receipt_path = args.build_receipt.expanduser().resolve()
    stamp_path = args.inspect_stamp.expanduser().resolve()
    expected_stamp = receipt_path.with_name(f"{receipt_path.name}.verified")
    if stamp_path != expected_stamp:
        raise VerificationError(
            f"verification stamp must be the build-receipt sidecar {expected_stamp}"
        )
    receipt = load_build_receipt(receipt_path)
    artifact, artifact_hash, verb_count, build_id = load_stamp(stamp_path)
    if build_id != receipt.build_id:
        raise VerificationError("verification stamp does not match the current Cargo build")
    if artifact != receipt.artifact:
        raise VerificationError("verification stamp names a different Cargo artifact")
    if artifact_hash != receipt.artifact_hash:
        raise VerificationError("verification stamp hash differs from the Cargo build receipt")
    if verb_count < args.min_verbs:
        raise VerificationError(
            f"verified verb count {verb_count} is below floor {args.min_verbs}"
        )
    return "\n".join(
        (
            f"SRC={shlex.quote(str(artifact))}",
            f"VERIFIED_SHA256={shlex.quote(artifact_hash)}",
            f"VERIFIED_VERBS={shlex.quote(str(verb_count))}",
        )
    )


def verify(args: argparse.Namespace) -> tuple[Path, str, int, int]:
    receipt: BuildReceipt | None = None
    if args.build_receipt is not None:
        receipt_path = args.build_receipt.expanduser().resolve()
        expected_stamp = receipt_path.with_name(f"{receipt_path.name}.verified")
    else:
        artifact = args.artifact.expanduser().resolve()
        expected_stamp = artifact.with_name(f"{artifact.name}.verified")
    stamp = args.stamp.expanduser().resolve() if args.stamp else None
    if stamp is not None and stamp != expected_stamp:
        raise VerificationError(
            f"verification stamp must be the source sidecar {expected_stamp}"
        )
    if stamp is not None:
        try:
            stamp.unlink(missing_ok=True)
        except OSError as error:
            raise VerificationError(
                f"cannot remove stale verification stamp: {error}"
            ) from error

    if args.build_receipt is not None:
        receipt = load_build_receipt(receipt_path, verify_artifact=False)
        artifact = receipt.artifact

    if not artifact.is_file():
        raise VerificationError(f"build artifact is missing: {artifact}")
    if not os.access(artifact, os.X_OK):
        raise VerificationError(f"build artifact is not executable: {artifact}")
    required_packs = parse_packs(args.packs)
    before_hash = sha256_file(artifact)
    if receipt is not None and before_hash != receipt.artifact_hash:
        raise VerificationError(
            "build artifact changed after Cargo reported it: "
            f"recorded={receipt.artifact_hash} current={before_hash}"
        )

    with tempfile.TemporaryDirectory(prefix="khive-build-verify-") as temporary_dir:
        isolated_root = Path(temporary_dir)
        config = isolated_root / "khive.toml"
        config.write_text("", encoding="utf-8")
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("KHIVE_")
        }
        environment.update(
            {
                "HOME": str(isolated_root),
                "KHIVE_CONFIG": str(config),
                "KHIVE_DB": ":memory:",
                "KHIVE_LOG": "error",
                "KHIVE_NO_DAEMON": "1",
                "KHIVE_NO_EMBED": "1",
                "KHIVE_PACKS": ",".join(required_packs),
            }
        )
        command = [
            str(artifact),
            "mcp",
            "--db",
            ":memory:",
            "--config",
            str(config),
            "--no-embed",
            "--log",
            "error",
        ]
        payload = run_mcp_probe(
            command,
            cwd=isolated_root,
            environment=environment,
            timeout_seconds=args.timeout_seconds,
        )
    verb_count = validate_probe(payload, required_packs, args.min_verbs)

    after_hash = sha256_file(artifact)
    if before_hash != after_hash:
        raise VerificationError("build artifact changed while its runtime surface was probed")
    if stamp is not None:
        write_stamp(
            stamp,
            artifact,
            after_hash,
            verb_count,
            receipt.build_id if receipt is not None else None,
        )
    return artifact, after_hash, verb_count, len(required_packs)


def main() -> int:
    try:
        args = parse_args()
        if args.inspect_stamp is not None:
            print(inspect_stamp(args))
            return 0
        artifact, artifact_hash, verb_count, pack_count = verify(args)
    except (BuildReceiptError, VerificationError, OSError, ValueError) as error:
        print(f"==> ERROR: local build verification failed: {error}", file=sys.stderr)
        return 1
    print(
        f"==> Verified build artifact: {artifact} "
        f"({artifact_hash}, {verb_count} verbs across {pack_count} requested packs)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
