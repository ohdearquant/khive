#!/usr/bin/env python3
"""Regression tests for the pre-install local build verification gate."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
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


class MakefileGateContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.makefile = MAKEFILE.read_text(encoding="utf-8")

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
        move, then bind the installed path to the post-sign digest."""
        local_recipe = self.makefile[self.makefile.index("local: verify-local-artifact") :]

        # A mutating step inside the install gate may never be ignored.
        self.assertNotIn('codesign -s - -f "$$DEST.new" 2>/dev/null || true', local_recipe)
        self.assertNotIn("codesign -s - -f \"$$DEST.new\" || true", local_recipe)
        self.assertIn('if ! codesign -s - -f "$$DEST.new"; then', local_recipe)

        staged_check = local_recipe.index('if [ "$$VERIFIED_SHA256" != "$$COPIED_SHA256" ]')
        sign = local_recipe.index('codesign -s - -f "$$DEST.new"')
        resign_verify = local_recipe.index('--artifact "$$DEST.new"')
        signed_digest = local_recipe.index("SIGNED_SHA256=")
        install = local_recipe.index('mv "$$DEST.new" "$$DEST"')
        installed_check = local_recipe.index(
            'if [ "$$SIGNED_SHA256" != "$$DEST_SHA256" ]'
        )

        # The signed artifact is re-probed with the full pack set and the same
        # verb floor, not merely re-hashed: a hash cannot show the binary still
        # loads every pack.
        self.assertIn('--packs "$(FULL_PACKS)"', local_recipe)
        self.assertLess(staged_check, sign)
        self.assertLess(sign, resign_verify)
        self.assertLess(resign_verify, signed_digest)
        self.assertLess(signed_digest, install)
        self.assertLess(install, installed_check)

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
