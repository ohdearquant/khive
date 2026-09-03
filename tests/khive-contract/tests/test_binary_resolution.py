"""The contract client resolves the kkernel binary through the shared harness rule.

tests/kkernel_binary.py mirrors scripts/ci.sh: KKERNEL_BINARY if set, else
CARGO_TARGET_DIR (absolute, or relative to crates/), else crates/target. The
client must follow the same rule so a CI step that builds into a custom target
directory runs the binary it just built rather than a stale default one.
"""

import pytest

from khive_contract.client import _resolve_binary


def _clear_binary_env(monkeypatch):
    for name in ("KKERNEL_BINARY", "KHIVE_MCP_BINARY"):
        monkeypatch.delenv(name, raising=False)


def test_custom_cargo_target_dir_selects_its_release_binary(tmp_path, monkeypatch):
    _clear_binary_env(monkeypatch)
    release = tmp_path / "custom-target" / "release" / "kkernel"
    release.parent.mkdir(parents=True)
    release.write_bytes(b"")
    monkeypatch.setenv("CARGO_TARGET_DIR", str(tmp_path / "custom-target"))

    assert _resolve_binary(None) == release


def test_custom_cargo_target_dir_falls_back_to_its_debug_binary(tmp_path, monkeypatch):
    _clear_binary_env(monkeypatch)
    debug = tmp_path / "custom-target" / "debug" / "kkernel"
    debug.parent.mkdir(parents=True)
    debug.write_bytes(b"")
    monkeypatch.setenv("CARGO_TARGET_DIR", str(tmp_path / "custom-target"))

    assert _resolve_binary(None) == debug


def test_custom_cargo_target_dir_without_a_binary_is_not_found(tmp_path, monkeypatch):
    _clear_binary_env(monkeypatch)
    monkeypatch.setenv("CARGO_TARGET_DIR", str(tmp_path / "empty-target"))

    with pytest.raises(FileNotFoundError):
        _resolve_binary(None)
