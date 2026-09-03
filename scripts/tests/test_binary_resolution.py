#!/usr/bin/env python3
"""Harness tests for kkernel binary resolution in the contract client.

tests/kkernel_binary.py mirrors scripts/ci.sh: KKERNEL_BINARY if set, else
CARGO_TARGET_DIR (absolute, or relative to crates/), else crates/target. The
contract client must follow the same rule so a CI step that builds into a
custom target directory runs the binary it just built rather than a stale
default one. These exercise the harness, not product verbs, so they live with
the other harness tests rather than in the contract suite.
"""

from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[2]
CONTRACT_ROOT = REPO_ROOT / "tests" / "khive-contract"
SHARED_RESOLVER = REPO_ROOT / "tests" / "kkernel_binary.py"
BINARY_ENV = ("KKERNEL_BINARY", "KHIVE_MCP_BINARY", "CARGO_TARGET_DIR")

sys.path.insert(0, str(CONTRACT_ROOT))

from khive_contract.client import _resolve_binary  # noqa: E402


def _shared_resolver():
    spec = importlib.util.spec_from_file_location("kkernel_binary", SHARED_RESOLVER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ResolveBinaryTests(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp_path = Path(tmp.name)
        env = mock.patch.dict(os.environ)
        env.start()
        self.addCleanup(env.stop)
        for name in BINARY_ENV:
            os.environ.pop(name, None)

    def test_custom_cargo_target_dir_selects_its_release_binary(self) -> None:
        release = self.tmp_path / "custom-target" / "release" / "kkernel"
        release.parent.mkdir(parents=True)
        release.write_bytes(b"")
        os.environ["CARGO_TARGET_DIR"] = str(self.tmp_path / "custom-target")

        self.assertEqual(_resolve_binary(None), release)

    def test_custom_cargo_target_dir_falls_back_to_its_debug_binary(self) -> None:
        debug = self.tmp_path / "custom-target" / "debug" / "kkernel"
        debug.parent.mkdir(parents=True)
        debug.write_bytes(b"")
        os.environ["CARGO_TARGET_DIR"] = str(self.tmp_path / "custom-target")

        self.assertEqual(_resolve_binary(None), debug)

    def test_custom_cargo_target_dir_without_a_binary_is_not_found(self) -> None:
        os.environ["CARGO_TARGET_DIR"] = str(self.tmp_path / "empty-target")

        with self.assertRaises(FileNotFoundError):
            _resolve_binary(None)

    def test_empty_cargo_target_dir_reads_as_unset(self) -> None:
        # scripts/ci.sh expands ${CARGO_TARGET_DIR:-...}, which treats "" as
        # unset; the shared resolver must select the same default binary.
        resolver = _shared_resolver()
        self.assertEqual(
            resolver.resolve_binary_path({"CARGO_TARGET_DIR": ""}),
            resolver.resolve_binary_path({}),
        )
        self.assertTrue(
            resolver.resolve_binary_path({"CARGO_TARGET_DIR": ""}).endswith(
                "/crates/target/release/kkernel"
            )
        )


if __name__ == "__main__":
    unittest.main()
