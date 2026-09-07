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


class SmokeChildEnvTests(unittest.TestCase):
    """tests/smoke_test.py builds every smoke child's environment through
    smoke_child_env; no KHIVE_* setting of the parent may reach a child, or
    the parent's config, namespace, actor or output format silently changes
    what the smoke suite measures."""

    def _smoke_child_env(self):
        spec = importlib.util.spec_from_file_location(
            "smoke_test_module", REPO_ROOT / "tests" / "smoke_test.py"
        )
        module = importlib.util.module_from_spec(spec)
        # smoke_test.py imports its sibling helpers by bare name.
        sys.path.insert(0, str(REPO_ROOT / "tests"))
        try:
            spec.loader.exec_module(module)
        finally:
            sys.path.remove(str(REPO_ROOT / "tests"))
        return module.smoke_child_env

    def test_child_env_strips_every_khive_variable(self):
        smoke_child_env = self._smoke_child_env()
        source = {
            "PATH": "/usr/bin",
            "KHIVE_PACKS": "kg,gtd",
            "KHIVE_CONFIG": "/somewhere/config.toml",
            "KHIVE_OUTPUT_FORMAT": "table",
            "KHIVE_NAMESPACE": "other",
            "KHIVE_ACTOR": "someone",
            "KHIVE_NO_DAEMON": "0",
        }
        env = smoke_child_env(source)
        leaked = sorted(k for k in env if k.startswith("KHIVE_") and k != "KHIVE_NO_DAEMON")
        self.assertEqual(leaked, [])
        self.assertEqual(env["KHIVE_NO_DAEMON"], "1")
        self.assertEqual(env["PATH"], "/usr/bin")
        self.assertIn("HOME", env)
        self.assertEqual(source["KHIVE_ACTOR"], "someone", "the source mapping is not mutated")


if __name__ == "__main__":
    unittest.main()
