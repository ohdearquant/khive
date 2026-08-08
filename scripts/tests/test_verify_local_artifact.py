#!/usr/bin/env python3
"""Regression tests for the pre-install local build verification gate."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
BUILD_SCRIPT = REPO_ROOT / "scripts" / "build_local_artifact.py"
VERIFY_SCRIPT = REPO_ROOT / "scripts" / "verify_local_artifact.py"
MAKEFILE = REPO_ROOT / "Makefile"
CI_SCRIPT = REPO_ROOT / "scripts" / "ci.sh"
TEST_PACKS = "kg,workspace,formal"


def canonical_json(payload: object) -> str:
    return json.dumps(payload, sort_keys=True, separators=(",", ":")) + "\n"


def write_build_receipt(
    path: Path, artifact: Path, *, build_id: str = "a" * 32
) -> None:
    path.write_text(
        canonical_json(
            {
                "artifact": str(artifact.resolve()),
                "build_id": build_id,
                "schema": 1,
                "sha256": hashlib.sha256(artifact.read_bytes()).hexdigest(),
            }
        ),
        encoding="utf-8",
    )


FAKE_ARTIFACT = r"""#!/usr/bin/env python3
import json
import os
from pathlib import Path
import select
import sys
import time

def read_message():
    raw = bytearray()
    while True:
        chunk = os.read(sys.stdin.fileno(), 1)
        if not chunk:
            return None
        if chunk == b"\n":
            break
        raw.extend(chunk)
    return json.loads(raw.decode("utf-8"))

mode = os.environ.get("FAKE_MODE", "ok")
record = os.environ.get("FAKE_RECORD")
config_path = Path(sys.argv[5]) if len(sys.argv) > 5 else Path("missing")
if record:
    Path(record).write_text(json.dumps({
        "args": sys.argv[1:],
        "cwd": os.getcwd(),
        "home": os.environ.get("HOME"),
        "config": os.environ.get("KHIVE_CONFIG"),
        "config_was_file": config_path.is_file(),
        "db": os.environ.get("KHIVE_DB"),
        "log": os.environ.get("KHIVE_LOG"),
        "no_daemon": os.environ.get("KHIVE_NO_DAEMON"),
        "no_embed": os.environ.get("KHIVE_NO_EMBED"),
        "packs": os.environ.get("KHIVE_PACKS"),
        "leak": os.environ.get("KHIVE_LEAK_SENTINEL"),
    }))

expected_shape = (
    len(sys.argv) == 9
    and sys.argv[1:5] == ["mcp", "--db", ":memory:", "--config"]
    and sys.argv[6:] == ["--no-embed", "--log", "error"]
    and config_path.is_file()
    and Path(os.environ["HOME"]).resolve() == Path.cwd().resolve()
    and os.environ.get("KHIVE_CONFIG") == str(config_path)
    and os.environ.get("KHIVE_DB") == ":memory:"
    and os.environ.get("KHIVE_NO_DAEMON") == "1"
    and os.environ.get("KHIVE_NO_EMBED") == "1"
    and os.environ.get("KHIVE_LEAK_SENTINEL") is None
)
if not expected_shape:
    print("probe was not isolated or did not use the expected CLI shape", file=sys.stderr)
    raise SystemExit(19)
if mode == "error":
    print("synthetic probe failure", file=sys.stderr)
    raise SystemExit(7)
if mode == "timeout":
    time.sleep(2)
if mode == "mutate":
    with Path(__file__).open("a", encoding="utf-8") as handle:
        handle.write("\n# mutation during probe\n")

initialize = read_message()
expected_initialize = (
    isinstance(initialize, dict)
    and initialize.get("jsonrpc") == "2.0"
    and initialize.get("id") == 1
    and initialize.get("method") == "initialize"
    and initialize.get("params", {}).get("protocolVersion") == "2024-11-05"
)
if not expected_initialize:
    print("probe did not send a valid initialize request", file=sys.stderr)
    raise SystemExit(20)

init_failure_modes = {
    "init-error",
    "init-missing",
    "init-malformed-json",
    "init-malformed-shape",
}
if mode not in init_failure_modes and select.select(
    [sys.stdin.fileno()], [], [], 0
)[0]:
    print("probe sent messages before initialize completed", file=sys.stderr)
    raise SystemExit(21)

if mode == "split-timeout":
    time.sleep(0.3)
if mode == "init-error":
    print(json.dumps({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {"code": -32000, "message": "synthetic initialize failure"},
    }), flush=True)
elif mode == "init-missing":
    pass
elif mode == "init-malformed-json":
    print("not-json", flush=True)
elif mode == "init-malformed-shape":
    print(json.dumps({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"serverInfo": {"name": "khive-mcp", "version": "fixture"}},
    }), flush=True)
else:
    print(json.dumps({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "khive-mcp", "version": "fixture"},
        },
    }), flush=True)

initialized = read_message()
tools_call = read_message()
expected_requests = (
    isinstance(initialized, dict)
    and initialized.get("method") == "notifications/initialized"
    and isinstance(tools_call, dict)
    and tools_call.get("id") == 2
    and tools_call.get("method") == "tools/call"
    and tools_call.get("params", {}).get("name") == "request"
    and tools_call.get("params", {}).get("arguments")
    == {"ops": "verbs()", "format": "json"}
)
if not expected_requests:
    print("probe did not use the expected MCP request sequence", file=sys.stderr)
    raise SystemExit(22)
if mode == "malformed":
    print("not-json", flush=True)
    raise SystemExit(0)
if mode == "split-timeout":
    time.sleep(0.3)

packs = os.environ["KHIVE_PACKS"].split(",")
total = 2 if mode == "low" else 3
pack_counts = {pack: 0 for pack in packs}
pack_counts[packs[0]] = total
if mode == "missing-pack":
    del pack_counts[packs[-1]]
if mode == "count-mismatch":
    pack_counts[packs[0]] += 1
verbs = [
    {"verb": f"{packs[0]}.verb-{index}", "pack": packs[0]}
    for index in range(total)
]
if mode == "missing-verb":
    del verbs[0]["verb"]
if mode == "blank-verb":
    verbs[0]["verb"] = "   "
if mode == "duplicate-verb":
    verbs[1]["verb"] = verbs[0]["verb"]
    verbs[1]["pack"] = packs[1]
    pack_counts[packs[0]] -= 1
    pack_counts[packs[1]] = 1
if mode == "whitespace-pack":
    pack_counts[packs[0]] = 0
    pack_counts["   "] = total
    for verb in verbs:
        verb["pack"] = "   "
if mode == "padded-verb":
    verbs[0]["verb"] = f" {verbs[0]['verb']} "
probe_payload = {
    "results": [{
        "ok": mode != "unsuccessful",
        "tool": "verbs",
        "result": {"verbs": verbs, "total": total, "pack_counts": pack_counts},
    }]
}
if mode == "mixed-result-error":
    probe_payload["results"][0]["error"] = "synthetic error alongside result"
content = [{"type": "text", "text": json.dumps(probe_payload)}]
if mode == "extra-content":
    content.append({"type": "image", "data": "AA==", "mimeType": "image/png"})
if mode == "malformed-extra-content":
    content.append({"unexpected": True})
print(json.dumps({
    "jsonrpc": "2.0",
    "id": 2,
    "result": {
        "content": content,
        "isError": False,
    },
}), flush=True)
"""


FAKE_CARGO = r"""#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

mode = os.environ.get("FAKE_CARGO_MODE", "ok")
artifact = os.environ.get("FAKE_CARGO_ARTIFACT")
record = os.environ.get("FAKE_CARGO_RECORD")
if record:
    Path(record).write_text(json.dumps(sys.argv[1:]))
if mode == "error":
    raise SystemExit(7)
if mode == "malformed":
    print("not-json")
    raise SystemExit(0)
if mode != "missing":
    print(json.dumps({
        "reason": "compiler-artifact",
        "target": {"name": "kkernel", "kind": ["lib"]},
        "executable": None,
    }))
    print(json.dumps({
        "reason": "compiler-artifact",
        "target": {"name": "kkernel", "kind": ["bin"]},
        "executable": artifact,
    }))
if mode == "duplicate-event":
    print(json.dumps({
        "reason": "compiler-artifact",
        "target": {"name": "kkernel", "kind": ["bin"]},
        "executable": artifact,
    }))
if mode == "multiple":
    print(json.dumps({
        "reason": "compiler-artifact",
        "target": {"name": "kkernel", "kind": ["bin"]},
        "executable": artifact + ".other",
    }))
print(json.dumps({"reason": "build-finished", "success": True}))
"""


class VerifyLocalArtifactTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.artifact = self.root / "kkernel"
        self.artifact.write_text(textwrap.dedent(FAKE_ARTIFACT), encoding="utf-8")
        self.artifact.chmod(0o755)
        self.stamp = self.root / "kkernel.verified"
        self.record = self.root / "record.json"

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def run_probe(
        self,
        mode: str = "ok",
        *,
        minimum: int = 3,
        timeout: float = 5.0,
    ) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                "FAKE_MODE": mode,
                "FAKE_RECORD": str(self.record),
                "KHIVE_LEAK_SENTINEL": "must-be-cleared",
            }
        )
        return subprocess.run(
            [
                sys.executable,
                str(VERIFY_SCRIPT),
                "--artifact",
                str(self.artifact),
                "--packs",
                TEST_PACKS,
                "--min-verbs",
                str(minimum),
                "--stamp",
                str(self.stamp),
                "--timeout-seconds",
                str(timeout),
            ],
            cwd=REPO_ROOT,
            env=environment,
            capture_output=True,
            text=True,
            check=False,
        )

    def run_receipt_probe(
        self, receipt: Path, stamp: Path
    ) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                "FAKE_MODE": "ok",
                "FAKE_RECORD": str(self.record),
            }
        )
        return subprocess.run(
            [
                sys.executable,
                str(VERIFY_SCRIPT),
                "--build-receipt",
                str(receipt),
                "--packs",
                TEST_PACKS,
                "--min-verbs",
                "3",
                "--stamp",
                str(stamp),
                "--timeout-seconds",
                "5",
            ],
            cwd=REPO_ROOT,
            env=environment,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_success_records_exact_artifact_hash_and_isolated_probe(self) -> None:
        completed = self.run_probe()
        self.assertEqual(completed.returncode, 0, completed.stderr)
        stamp = json.loads(self.stamp.read_text(encoding="utf-8"))
        self.assertEqual(
            stamp["sha256"],
            hashlib.sha256(self.artifact.read_bytes()).hexdigest(),
        )
        self.assertEqual(stamp["artifact"], str(self.artifact.resolve()))
        self.assertEqual(stamp["verb_count"], 3)
        self.assertIsNone(stamp["build_id"])
        self.assertEqual(
            self.stamp.read_text(encoding="utf-8"), canonical_json(stamp)
        )

        record = json.loads(self.record.read_text(encoding="utf-8"))
        self.assertEqual(record["packs"], TEST_PACKS)
        self.assertEqual(record["db"], ":memory:")
        self.assertEqual(record["no_daemon"], "1")
        self.assertEqual(record["no_embed"], "1")
        self.assertTrue(record["config_was_file"])
        self.assertEqual(Path(record["home"]).resolve(), Path(record["cwd"]).resolve())
        self.assertIsNone(record["leak"])

    def test_every_probe_or_contract_failure_removes_stale_stamp(self) -> None:
        for mode in (
            "low",
            "missing-pack",
            "count-mismatch",
            "malformed",
            "unsuccessful",
            "error",
        ):
            with self.subTest(mode=mode):
                self.stamp.write_text("stale 999\n", encoding="utf-8")
                completed = self.run_probe(mode)
                self.assertNotEqual(completed.returncode, 0, completed.stdout)
                self.assertFalse(self.stamp.exists())

    def test_initialize_failure_missing_and_malformed_responses_fail_closed(self) -> None:
        cases = (
            ("init-error", "initialize returned an MCP error", 5.0),
            ("init-missing", "timed out", 0.2),
            ("init-malformed-json", "invalid JSON for initialize", 5.0),
            ("init-malformed-shape", "invalid protocolVersion", 5.0),
        )
        for mode, expected, timeout in cases:
            with self.subTest(mode=mode):
                self.stamp.write_text("stale 999\n", encoding="utf-8")
                completed = self.run_probe(mode, timeout=timeout)
                self.assertNotEqual(completed.returncode, 0, completed.stdout)
                self.assertIn(expected, completed.stderr)
                self.assertFalse(self.stamp.exists())

    def test_catalog_requires_nonblank_globally_unique_verb_names(self) -> None:
        cases = (
            ("missing-verb", "does not name an exact nonblank verb"),
            ("blank-verb", "does not name an exact nonblank verb"),
            ("duplicate-verb", "contains duplicate verb"),
        )
        for mode, expected in cases:
            with self.subTest(mode=mode):
                self.stamp.write_text("stale 999\n", encoding="utf-8")
                completed = self.run_probe(mode)
                self.assertNotEqual(completed.returncode, 0, completed.stdout)
                self.assertIn(expected, completed.stderr)
                self.assertFalse(self.stamp.exists())

    def test_mixed_success_error_operation_shape_fails_closed(self) -> None:
        self.stamp.write_text("stale 999\n", encoding="utf-8")
        completed = self.run_probe("mixed-result-error")
        self.assertNotEqual(completed.returncode, 0, completed.stdout)
        self.assertIn("invalid result/error shape", completed.stderr)
        self.assertFalse(self.stamp.exists())

    def test_extra_or_malformed_mcp_content_blocks_fail_closed(self) -> None:
        for mode in ("extra-content", "malformed-extra-content"):
            with self.subTest(mode=mode):
                self.stamp.write_text("stale 999\n", encoding="utf-8")
                completed = self.run_probe(mode)
                self.assertNotEqual(completed.returncode, 0, completed.stdout)
                self.assertIn("exactly one text content block", completed.stderr)
                self.assertFalse(self.stamp.exists())

    def test_pack_and_verb_names_use_exact_nonblank_vocabulary(self) -> None:
        cases = (
            ("whitespace-pack", "invalid pack name"),
            ("padded-verb", "exact nonblank verb"),
        )
        for mode, expected in cases:
            with self.subTest(mode=mode):
                self.stamp.write_text("stale 999\n", encoding="utf-8")
                completed = self.run_probe(mode)
                self.assertNotEqual(completed.returncode, 0, completed.stdout)
                self.assertIn(expected, completed.stderr)
                self.assertFalse(self.stamp.exists())

    def test_timeout_is_a_failure_and_removes_stale_stamp(self) -> None:
        self.stamp.write_text("stale 999\n", encoding="utf-8")
        completed = self.run_probe("timeout", timeout=0.1)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("timed out", completed.stderr)
        self.assertFalse(self.stamp.exists())

    def test_timeout_is_one_absolute_handshake_deadline(self) -> None:
        completed = self.run_probe("split-timeout", timeout=0.45)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("timed out", completed.stderr)
        self.assertFalse(self.stamp.exists())

    def test_artifact_mutation_during_probe_fails_closed(self) -> None:
        completed = self.run_probe("mutate")
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("changed while", completed.stderr)
        self.assertFalse(self.stamp.exists())

    def test_missing_or_non_executable_artifacts_fail_closed(self) -> None:
        self.artifact.unlink()
        missing = self.run_probe()
        self.assertNotEqual(missing.returncode, 0)
        self.assertIn("missing", missing.stderr)

        self.artifact.write_text("not executable", encoding="utf-8")
        self.artifact.chmod(0o644)
        non_executable = self.run_probe()
        self.assertNotEqual(non_executable.returncode, 0)
        self.assertIn("not executable", non_executable.stderr)

    def test_stamp_is_restricted_to_the_artifact_sidecar(self) -> None:
        protected = self.root / "unrelated"
        protected.write_text("keep", encoding="utf-8")
        completed = subprocess.run(
            [
                sys.executable,
                str(VERIFY_SCRIPT),
                "--artifact",
                str(self.artifact),
                "--packs",
                TEST_PACKS,
                "--min-verbs",
                "3",
                "--stamp",
                str(protected),
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(protected.read_text(encoding="utf-8"), "keep")

    def test_build_receipt_binds_probe_stamp_and_install_fields(self) -> None:
        artifact_with_spaces = self.root / "kkernel with ' quote"
        self.artifact.rename(artifact_with_spaces)
        self.artifact = artifact_with_spaces
        receipt = self.root / "build.json"
        stamp = self.root / "build.json.verified"
        write_build_receipt(receipt, self.artifact)

        completed = self.run_receipt_probe(receipt, stamp)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        stamp_payload = json.loads(stamp.read_text(encoding="utf-8"))
        self.assertEqual(stamp_payload["build_id"], "a" * 32)
        self.assertEqual(stamp_payload["artifact"], str(self.artifact.resolve()))

        inspected = subprocess.run(
            [
                sys.executable,
                str(VERIFY_SCRIPT),
                "--build-receipt",
                str(receipt),
                "--inspect-stamp",
                str(stamp),
                "--min-verbs",
                "3",
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(inspected.returncode, 0, inspected.stderr)
        round_trip = subprocess.run(
            [
                "/bin/sh",
                "-c",
                'eval "$1"; printf "%s\\n%s\\n%s\\n" "$SRC" "$VERIFIED_SHA256" "$VERIFIED_VERBS"',
                "sh",
                inspected.stdout,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(round_trip.returncode, 0, round_trip.stderr)
        fields = round_trip.stdout.splitlines()
        self.assertEqual(fields[0], str(self.artifact.resolve()))
        self.assertEqual(
            fields[1], hashlib.sha256(self.artifact.read_bytes()).hexdigest()
        )
        self.assertEqual(fields[2], "3")

        write_build_receipt(receipt, self.artifact, build_id="b" * 32)
        stale = subprocess.run(
            [
                sys.executable,
                str(VERIFY_SCRIPT),
                "--build-receipt",
                str(receipt),
                "--inspect-stamp",
                str(stamp),
                "--min-verbs",
                "3",
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(stale.returncode, 0)
        self.assertIn("current Cargo build", stale.stderr)

    def test_stamp_count_is_canonical_bounded_and_never_shell_parsed(self) -> None:
        receipt = self.root / "build.json"
        stamp = self.root / "build.json.verified"
        write_build_receipt(receipt, self.artifact)
        completed = self.run_receipt_probe(receipt, stamp)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        valid_payload = json.loads(stamp.read_text(encoding="utf-8"))

        cases = (
            (10**100, "bounded verb count"),
            ("000003", "bounded verb count"),
            (True, "bounded verb count"),
        )
        for verb_count, expected in cases:
            with self.subTest(verb_count=verb_count):
                payload = {**valid_payload, "verb_count": verb_count}
                stamp.write_text(canonical_json(payload), encoding="utf-8")
                inspected = subprocess.run(
                    [
                        sys.executable,
                        str(VERIFY_SCRIPT),
                        "--build-receipt",
                        str(receipt),
                        "--inspect-stamp",
                        str(stamp),
                        "--min-verbs",
                        "3",
                    ],
                    cwd=REPO_ROOT,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertNotEqual(inspected.returncode, 0)
                self.assertIn(expected, inspected.stderr)

        stamp.write_text(json.dumps(valid_payload, indent=2), encoding="utf-8")
        noncanonical = subprocess.run(
            [
                sys.executable,
                str(VERIFY_SCRIPT),
                "--build-receipt",
                str(receipt),
                "--inspect-stamp",
                str(stamp),
                "--min-verbs",
                "3",
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(noncanonical.returncode, 0)
        self.assertIn("canonical JSON", noncanonical.stderr)

    def test_receipt_hash_closes_build_verify_and_verify_install_windows(self) -> None:
        receipt = self.root / "build.json"
        stamp = self.root / "build.json.verified"
        write_build_receipt(receipt, self.artifact)
        self.artifact.write_text("changed after Cargo", encoding="utf-8")
        stamp.write_text("stale", encoding="utf-8")
        before_probe = self.run_receipt_probe(receipt, stamp)
        self.assertNotEqual(before_probe.returncode, 0)
        self.assertIn("changed after Cargo", before_probe.stderr)
        self.assertFalse(stamp.exists())

        self.artifact.write_text(textwrap.dedent(FAKE_ARTIFACT), encoding="utf-8")
        self.artifact.chmod(0o755)
        write_build_receipt(receipt, self.artifact)
        verified = self.run_receipt_probe(receipt, stamp)
        self.assertEqual(verified.returncode, 0, verified.stderr)
        self.artifact.write_text("changed before install", encoding="utf-8")
        inspected = subprocess.run(
            [
                sys.executable,
                str(VERIFY_SCRIPT),
                "--build-receipt",
                str(receipt),
                "--inspect-stamp",
                str(stamp),
                "--min-verbs",
                "3",
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(inspected.returncode, 0)
        self.assertIn("no longer matches", inspected.stderr)

    def test_timeout_must_be_finite(self) -> None:
        for timeout in ("nan", "inf", "-inf"):
            with self.subTest(timeout=timeout):
                completed = subprocess.run(
                    [
                        sys.executable,
                        str(VERIFY_SCRIPT),
                        "--artifact",
                        str(self.artifact),
                        "--packs",
                        TEST_PACKS,
                        "--min-verbs",
                        "3",
                        f"--timeout-seconds={timeout}",
                    ],
                    cwd=REPO_ROOT,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(completed.returncode, 2)
                self.assertIn("finite number greater than 0", completed.stderr)


class BuildLocalArtifactTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.fake_cargo = self.root / "fake cargo"
        self.fake_cargo.write_text(textwrap.dedent(FAKE_CARGO), encoding="utf-8")
        self.fake_cargo.chmod(0o755)
        self.receipt = self.root / "state" / "local-build.json"
        self.record = self.root / "cargo-args.json"

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def run_build(
        self, artifact: Path, *, mode: str = "ok"
    ) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                "CARGO_TARGET_DIR": str(self.root / "configured-but-not-guessed"),
                "FAKE_CARGO_ARTIFACT": str(artifact),
                "FAKE_CARGO_MODE": mode,
                "FAKE_CARGO_RECORD": str(self.record),
            }
        )
        return subprocess.run(
            [
                sys.executable,
                str(BUILD_SCRIPT),
                "--cargo",
                str(self.fake_cargo),
                "--manifest-path",
                str(REPO_ROOT / "crates" / "Cargo.toml"),
                "--package",
                "kkernel",
                "--features",
                "channel-email,channel-telegram",
                "--receipt",
                str(self.receipt),
            ],
            cwd=REPO_ROOT,
            env=environment,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_cargo_reported_target_dir_and_target_triple_are_authoritative(self) -> None:
        artifact = (
            self.root
            / "user configured target"
            / "aarch64-unknown-linux-gnu"
            / "release"
            / "kkernel"
        )
        artifact.parent.mkdir(parents=True)
        artifact.write_text("exact cross-target fixture", encoding="utf-8")
        artifact.chmod(0o755)

        completed = self.run_build(artifact)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        receipt = json.loads(self.receipt.read_text(encoding="utf-8"))
        self.assertEqual(receipt["artifact"], str(artifact.resolve()))
        self.assertEqual(
            receipt["sha256"], hashlib.sha256(artifact.read_bytes()).hexdigest()
        )
        self.assertEqual(
            self.receipt.read_text(encoding="utf-8"), canonical_json(receipt)
        )
        cargo_args = json.loads(self.record.read_text(encoding="utf-8"))
        self.assertIn("--message-format=json-render-diagnostics", cargo_args)

        printed = subprocess.run(
            [
                sys.executable,
                str(BUILD_SCRIPT),
                "--receipt",
                str(self.receipt),
                "--print-artifact",
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(printed.returncode, 0, printed.stderr)
        self.assertEqual(printed.stdout.strip(), str(artifact.resolve()))

    def test_every_cargo_contract_failure_removes_stale_receipts(self) -> None:
        artifact = self.root / "kkernel"
        artifact.write_text("fixture", encoding="utf-8")
        artifact.chmod(0o755)
        for mode in ("error", "malformed", "missing", "multiple"):
            with self.subTest(mode=mode):
                self.receipt.parent.mkdir(parents=True, exist_ok=True)
                self.receipt.write_text("stale", encoding="utf-8")
                verified = self.receipt.with_name(f"{self.receipt.name}.verified")
                verified.write_text("stale", encoding="utf-8")
                completed = self.run_build(artifact, mode=mode)
                self.assertNotEqual(completed.returncode, 0, completed.stdout)
                self.assertFalse(self.receipt.exists())
                self.assertFalse(verified.exists())

    def test_duplicate_matching_cargo_artifact_events_fail_closed(self) -> None:
        artifact = self.root / "kkernel"
        artifact.write_text("fixture", encoding="utf-8")
        artifact.chmod(0o755)
        completed = self.run_build(artifact, mode="duplicate-event")
        self.assertNotEqual(completed.returncode, 0, completed.stdout)
        self.assertIn("exactly one executable compiler-artifact event", completed.stderr)
        self.assertFalse(self.receipt.exists())


class RecipeRun:
    """Outcome of executing the real `local:` recipe in a sandbox."""

    def __init__(
        self,
        *,
        rc: int,
        output: str,
        installed: bool,
        installed_bytes: bytes | None,
        signed_bytes: bytes | None,
        staging_file_left_behind: bool,
        codesign_invocations: int,
        signed_probe_invocations: int,
    ) -> None:
        self.rc = rc
        self.output = output
        self.installed = installed
        self.installed_bytes = installed_bytes
        self.signed_bytes = signed_bytes
        self.staging_file_left_behind = staging_file_left_behind
        self.codesign_invocations = codesign_invocations
        self.signed_probe_invocations = signed_probe_invocations


PRE_EXISTING_INSTALL = b"#!/bin/sh\necho PRE-EXISTING\n"


class MakefileGateContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.makefile = MAKEFILE.read_text(encoding="utf-8")

    # -- behavioural harness -------------------------------------------------
    #
    # Runs the ACTUAL `local:` recipe text extracted from the Makefile, with
    # every irreversible edge redirected into a temp tree: HOME (and therefore
    # DEST) points at the sandbox, and the verifier is a stub whose exit code
    # the test controls. Nothing here touches the real installed daemon.

    @staticmethod
    def _extract_local_recipe(makefile_text: str) -> list[str]:
        """Return the shell commands make would run for `local:`, expanded."""
        variables = dict(
            re.findall(r"^([A-Z_][A-Z0-9_]*)\s*:=\s*(.*)$", makefile_text, re.M)
        )
        body = makefile_text[makefile_text.index("local: verify-local-artifact") :]
        body = body[body.index("\n") + 1 :]

        lines: list[str] = []
        for raw in body.split("\n"):
            if not raw.startswith("\t"):
                if raw.strip() == "":
                    continue
                break
            lines.append(raw[1:])

        # Re-join make lines: a trailing backslash continues the SAME shell.
        commands: list[str] = []
        current: list[str] = []
        for line in lines:
            current.append(line)
            if not line.rstrip().endswith("\\"):
                commands.append("\n".join(current))
                current = []
        if current:
            commands.append("\n".join(current))

        expanded: list[str] = []
        for cmd in commands:
            cmd = cmd.lstrip("@")
            # Variable values may themselves reference variables
            # (LOCAL_VERIFY_STAMP is built from LOCAL_BUILD_RECEIPT), so expand
            # to a fixpoint. A single pass leaves a stray `$(NAME)` that the
            # shell then reads as command substitution.
            for _ in range(10):
                before = cmd
                for name, value in variables.items():
                    cmd = cmd.replace(f"$({name})", value.strip())
                if cmd == before:
                    break
            else:  # pragma: no cover - only on a cyclic definition
                raise AssertionError(f"variable expansion did not converge: {cmd!r}")
            leftover = re.search(r"\$\([A-Z_][A-Z0-9_]*\)", cmd)
            self_check = leftover.group(0) if leftover else None
            assert self_check is None, f"unexpanded make variable {self_check} in recipe"
            # make collapses `$$` to a single `$` before handing to the shell.
            cmd = cmd.replace("$$", "$")
            expanded.append(cmd)
        return expanded

    def _run_local_recipe(
        self,
        *,
        signed_probe_exit: int = 0,
        codesign_exit: int = 0,
        makefile_text: str | None = None,
        fixture_shape: str = "bare",
    ) -> RecipeRun:
        recipe_makefile = makefile_text or self.makefile
        commands = self._extract_local_recipe(recipe_makefile)
        self.assertTrue(commands, "no recipe extracted from the Makefile")

        with tempfile.TemporaryDirectory() as tmp:
            fixture = Path(tmp)
            root = fixture / "workspace"
            root.mkdir()
            (root / "Makefile").write_text(recipe_makefile, encoding="utf-8")
            if fixture_shape in ("git-dir", "real-make"):
                subprocess.run(
                    ["git", "init", "-q", str(root)],
                    check=True,
                    capture_output=True,
                    text=True,
                )
            elif fixture_shape == "git-file":
                subprocess.run(
                    [
                        "git",
                        "init",
                        "-q",
                        f"--separate-git-dir={fixture / 'git-dir'}",
                        str(root),
                    ],
                    check=True,
                    capture_output=True,
                    text=True,
                )
            elif fixture_shape != "bare":
                raise AssertionError(f"unknown recipe fixture shape: {fixture_shape}")

            home = root / "home"
            dest_dir = home / ".cargo" / "bin"
            dest_dir.mkdir(parents=True)
            dest = dest_dir / "kkernel"
            dest.write_bytes(PRE_EXISTING_INSTALL)
            dest.chmod(0o755)

            src = root / "build" / "kkernel"
            src.parent.mkdir(parents=True)
            src.write_bytes(b"#!/bin/sh\necho FRESH-BUILD 0.0.0-test\n")
            src.chmod(0o755)
            src_sha = hashlib.sha256(src.read_bytes()).hexdigest()
            signed_probe_record = root / "signed-probe-invocations"
            codesign_record = root / "codesign-invocations"

            # Stub verifier: serves the stamp inspection, and its --artifact
            # (post-signing) probe exits with the code this test chose.
            (root / "scripts").mkdir()
            (root / "scripts" / "verify_local_artifact.py").write_text(
                textwrap.dedent(
                    f"""\
                    import os
                    import sys
                    argv = sys.argv[1:]
                    if "--inspect-stamp" in argv:
                        print('SRC={src}')
                        print('VERIFIED_SHA256={src_sha}')
                        print('VERIFIED_VERBS=90')
                        sys.exit(0)
                    if "--artifact" in argv:
                        with open(
                            os.environ["SIGNED_PROBE_RECORD"], "a", encoding="utf-8"
                        ) as record:
                            record.write("called\\n")
                        sys.exit({signed_probe_exit})
                    sys.exit(0)
                    """
                ),
                encoding="utf-8",
            )

            # codesign genuinely REWRITES the file, which is the whole reason
            # the pre-sign verification does not cover the installed bytes.
            stub_bin = root / "stub-bin"
            stub_bin.mkdir()
            (stub_bin / "codesign").write_text(
                textwrap.dedent(
                    f"""\
                    #!/bin/sh
                    printf 'called\\n' >> "$CODESIGN_RECORD"
                    [ {codesign_exit} -ne 0 ] && exit {codesign_exit}
                    for a in "$@"; do target="$a"; done
                    printf '\\n# signed\\n' >> "$target"
                    exit 0
                    """
                ),
                encoding="utf-8",
            )
            (root / "scripts" / "build_local_artifact.py").write_text(
                "raise SystemExit(0)\n", encoding="utf-8"
            )
            for noop in ("pkill", "pgrep"):
                (stub_bin / noop).write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
            for tool in ("codesign", "pkill", "pgrep"):
                (stub_bin / tool).chmod(0o755)

            env = dict(os.environ)
            env["HOME"] = str(home)
            env["PATH"] = f"{stub_bin}:{env.get('PATH', '')}"
            env["PYTHONDONTWRITEBYTECODE"] = "1"
            env["CODESIGN_RECORD"] = str(codesign_record)
            env["SIGNED_PROBE_RECORD"] = str(signed_probe_record)
            env.pop("MAKEFLAGS", None)
            env.pop("MAKELEVEL", None)

            rc = 0
            chunks: list[str] = []
            signed_bytes: bytes | None = None
            if fixture_shape == "real-make":
                proc = subprocess.run(
                    ["make", "-f", "Makefile", "local"],
                    cwd=root,
                    env=env,
                    capture_output=True,
                    text=True,
                )
                chunks.append(
                    f"$ make -f Makefile local\n[rc={proc.returncode}]\n"
                    f"{proc.stdout}{proc.stderr}"
                )
                rc = 0 if proc.returncode == 0 else 1
            else:
                for cmd in commands:
                    proc = subprocess.run(
                        ["sh", "-c", cmd],
                        cwd=root,
                        env=env,
                        capture_output=True,
                        text=True,
                    )
                    chunks.append(
                        f"$ {cmd[:120]}...\n[rc={proc.returncode}]\n"
                        f"{proc.stdout}{proc.stderr}"
                    )
                    rc = proc.returncode
                    if rc != 0:
                        break

            staged = dest_dir / "kkernel.new"
            installed_bytes = dest.read_bytes() if dest.exists() else None
            # What SHOULD have landed: the source with the signature appended.
            if codesign_exit == 0:
                signed_bytes = src.read_bytes() + b"\n# signed\n"

            codesign_invocations = (
                len(codesign_record.read_text(encoding="utf-8").splitlines())
                if codesign_record.exists()
                else 0
            )
            signed_probe_invocations = (
                len(signed_probe_record.read_text(encoding="utf-8").splitlines())
                if signed_probe_record.exists()
                else 0
            )

            return RecipeRun(
                rc=rc,
                output="\n".join(chunks),
                installed=installed_bytes != PRE_EXISTING_INSTALL,
                installed_bytes=installed_bytes,
                signed_bytes=signed_bytes,
                staging_file_left_behind=staged.exists(),
                codesign_invocations=codesign_invocations,
                signed_probe_invocations=signed_probe_invocations,
            )

    @staticmethod
    def _verification_behavior(run: RecipeRun) -> tuple[object, ...]:
        return (
            run.rc,
            run.installed,
            run.installed_bytes,
            run.signed_bytes,
            run.staging_file_left_behind,
            run.codesign_invocations,
            run.signed_probe_invocations,
        )

    def _condition_signed_probe(self, condition: str) -> str:
        start = self.makefile.index(
            '\techo "==> Re-verifying the SIGNED artifact'
        )
        end = self.makefile.index("\tSIGNED_SHA256=", start)
        probe = self.makefile[start:end]
        nested_probe = "".join(
            f"\t  {line[1:]}" if line.startswith("\t") else line
            for line in probe.splitlines(keepends=True)
        )
        return (
            self.makefile[:start]
            + f"\tif {condition}; then \\\n"
            + nested_probe
            + "\tfi; \\\n"
            + self.makefile[end:]
        )

    def _run_local_recipe_variants(
        self,
        *,
        signed_probe_exit: int = 0,
        codesign_exit: int = 0,
        makefile_text: str | None = None,
    ) -> dict[str, RecipeRun]:
        runs = {
            shape: self._run_local_recipe(
                signed_probe_exit=signed_probe_exit,
                codesign_exit=codesign_exit,
                makefile_text=makefile_text,
                fixture_shape=shape,
            )
            for shape in ("bare", "git-dir", "git-file")
        }
        runs["real-make"] = self._run_local_recipe(
            signed_probe_exit=signed_probe_exit,
            codesign_exit=codesign_exit,
            makefile_text=makefile_text,
            fixture_shape="real-make",
        )
        bare_behavior = self._verification_behavior(runs["bare"])
        for shape in ("git-dir", "git-file", "real-make"):
            self.assertEqual(
                self._verification_behavior(runs[shape]),
                bare_behavior,
                "the local recipe's verification behavior changed with the "
                f"checkout environment ({shape})\n"
                f"bare run:\n{runs['bare'].output}\n"
                f"{shape} run:\n{runs[shape].output}",
            )
        return runs

    def test_parity_rejects_git_conditioned_signed_probe(self) -> None:
        forged = self._condition_signed_probe("[ ! -e .git ]")

        with self.assertRaisesRegex(
            AssertionError,
            r"verification behavior changed with the checkout environment \(git-dir\)",
        ):
            self._run_local_recipe_variants(
                signed_probe_exit=1, makefile_text=forged
            )

    def test_parity_rejects_makelevel_conditioned_signed_probe(self) -> None:
        forged = self._condition_signed_probe('[ -z "$$MAKELEVEL" ]')

        with self.assertRaisesRegex(
            AssertionError,
            r"verification behavior changed with the checkout environment \(real-make\)",
        ):
            self._run_local_recipe_variants(
                signed_probe_exit=1, makefile_text=forged
            )

    def test_extracted_recipe_ignores_harness_fingerprints(self) -> None:
        recipe = "\n".join(self._extract_local_recipe(self.makefile))

        self.assertNotIn("CODESIGN_RECORD", recipe)
        self.assertNotIn("SIGNED_PROBE_RECORD", recipe)

    def test_local_dependency_chain_is_build_then_verify_then_install(self) -> None:
        self.assertIn("verify-local-artifact: build-local\n", self.makefile)
        self.assertIn("local: verify-local-artifact\n", self.makefile)
        self.assertIn("LOCAL_VERB_FLOOR := 90", self.makefile)

        local_recipe = self.makefile[self.makefile.index("local: verify-local-artifact") :]
        self.assertNotIn("cargo build", local_recipe)
        stamp_check = local_recipe.index('--inspect-stamp "$(LOCAL_VERIFY_STAMP)"')
        assignments = local_recipe.index('eval "$$VERIFIED_ASSIGNMENTS"')
        hash_check = local_recipe.index(
            'if [ "$$VERIFIED_SHA256" != "$$SRC_SHA256" ]'
        )
        copy = local_recipe.index('cp "$$SRC" "$$DEST.new"')
        staged_check = local_recipe.index(
            'if [ "$$VERIFIED_SHA256" != "$$COPIED_SHA256" ]'
        )
        install = local_recipe.index('mv "$$DEST.new" "$$DEST"')
        daemon_stop = local_recipe.index("pkill -f 'kkernel mcp --daemon'")
        self.assertNotIn('"$$VERIFIED_VERBS" -lt', local_recipe)
        self.assertLess(stamp_check, assignments)
        self.assertLess(assignments, hash_check)
        self.assertLess(hash_check, copy)
        self.assertLess(copy, staged_check)
        self.assertLess(staged_check, install)
        self.assertLess(install, daemon_stop)

    def test_signing_cannot_slip_unverified_bytes_past_the_install_gate(self) -> None:
        """`codesign` rewrites the staged file, so every pre-sign check is void
        for the bytes that actually get installed. The gate must therefore fail
        closed on a signing error and re-probe the SIGNED artifact before the
        move, then bind the installed path to the post-sign digest.

        This is deliberately a BEHAVIOURAL test. An earlier version of this
        guard asserted only that certain substrings appeared in a certain
        order, which a recipe that merely echoed those substrings satisfied
        while probing nothing. Text cannot distinguish a probe from a mention
        of a probe, so the recipe is executed instead.
        """
        for shape, control in self._run_local_recipe_variants(
            signed_probe_exit=0
        ).items():
            with self.subTest(fixture_shape=shape, probe="passes"):
                self.assertTrue(
                    control.installed,
                    "positive control failed: the recipe must install when every probe "
                    f"passes, but the installed binary was unchanged. rc={control.rc}\n"
                    f"{control.output}",
                )
                self.assertEqual(control.rc, 0, control.output)
                self.assertEqual(
                    control.installed_bytes,
                    control.signed_bytes,
                    "control: the installed bytes must be exactly the signed, probed bytes",
                )
                self.assertEqual(control.codesign_invocations, 1, control.output)
                self.assertEqual(control.signed_probe_invocations, 1, control.output)

        for shape, mutant in self._run_local_recipe_variants(
            signed_probe_exit=1
        ).items():
            with self.subTest(fixture_shape=shape, probe="fails"):
                self.assertFalse(
                    mutant.installed,
                    "the post-signing probe FAILED and the binary was installed anyway: "
                    "unverified bytes reached the install path\n" + mutant.output,
                )
                self.assertNotEqual(
                    mutant.rc,
                    0,
                    "a failed signed-artifact probe must exit nonzero\n" + mutant.output,
                )
                self.assertFalse(
                    mutant.staging_file_left_behind,
                    "the failure path must remove the staged file, not leave it for a "
                    "later run to pick up\n" + mutant.output,
                )
                self.assertEqual(mutant.codesign_invocations, 1, mutant.output)
                self.assertEqual(mutant.signed_probe_invocations, 1, mutant.output)

    def test_a_signing_failure_cannot_reach_the_install_path(self) -> None:
        """`codesign` failure was previously swallowed by `|| true`. Signing is
        a mutating step inside the install gate, so its failure must abort."""
        for shape, control in self._run_local_recipe_variants(
            codesign_exit=0
        ).items():
            with self.subTest(fixture_shape=shape, codesign="passes"):
                self.assertTrue(
                    control.installed, "positive control failed\n" + control.output
                )
                self.assertEqual(control.codesign_invocations, 1, control.output)
                self.assertEqual(control.signed_probe_invocations, 1, control.output)

        for shape, mutant in self._run_local_recipe_variants(
            codesign_exit=1
        ).items():
            with self.subTest(fixture_shape=shape, codesign="fails"):
                self.assertFalse(
                    mutant.installed,
                    "codesign failed and the binary was installed anyway\n" + mutant.output,
                )
                self.assertNotEqual(mutant.rc, 0, mutant.output)
                self.assertEqual(mutant.codesign_invocations, 1, mutant.output)
                self.assertEqual(mutant.signed_probe_invocations, 0, mutant.output)

    def test_the_recipe_harness_can_detect_a_reverted_fix(self) -> None:
        """Mutation control for the harness itself. A finding-only instrument
        that never reddens proves nothing, so point it at the pre-fix recipe
        and require it to catch the defect that motivated this PR."""
        pre_fix = self.makefile.replace(
            'if ! codesign -s - -f "$$DEST.new"; then \\\n'
            '\t  echo "==> ERROR: codesign failed on $$DEST.new — refusing to install"; \\\n'
            '\t  rm -f "$$DEST.new"; \\\n'
            "\t  exit 1; \\\n"
            "\tfi; \\\n",
            'codesign -s - -f "$$DEST.new" 2>/dev/null || true; \\\n',
        )
        self.assertNotEqual(
            pre_fix, self.makefile, "the fix text was not found; update this control"
        )
        # Strip the post-signing re-probe as well, reproducing the old recipe.
        start = pre_fix.index('echo "==> Re-verifying the SIGNED artifact')
        end = pre_fix.index("SIGNED_SHA256=")
        pre_fix = pre_fix[:start] + pre_fix[end:]

        for shape, reverted in self._run_local_recipe_variants(
            signed_probe_exit=1, makefile_text=pre_fix
        ).items():
            with self.subTest(fixture_shape=shape):
                self.assertTrue(
                    reverted.installed,
                    "the harness did not reproduce the original defect, so a green "
                    "result from it is not evidence\n" + reverted.output,
                )

    def test_cargo_receipt_drives_verifier_and_ci_runs_regression_suite(self) -> None:
        self.assertIn("scripts/build_local_artifact.py", self.makefile)
        self.assertIn("scripts/verify_local_artifact.py", self.makefile)
        self.assertIn('--build-receipt "$(LOCAL_BUILD_RECEIPT)"', self.makefile)
        self.assertIn('--min-verbs "$(LOCAL_VERB_FLOOR)"', self.makefile)
        self.assertIn('--stamp "$(LOCAL_VERIFY_STAMP)"', self.makefile)
        self.assertNotIn("LOCAL_ARTIFACT", self.makefile)
        self.assertNotIn("/release/kkernel", self.makefile)
        self.assertIn(
            'python3 "$SCRIPT_DIR/tests/test_verify_local_artifact.py"',
            CI_SCRIPT.read_text(encoding="utf-8"),
        )


if __name__ == "__main__":
    unittest.main()
