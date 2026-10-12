"""`find_kkernel` resolution order: KKERNEL, then the installed scripts directory, then PATH."""

from __future__ import annotations

import stat
import sys
from pathlib import Path

import pytest

from khive import binary
from khive.binary import find_kkernel

EXE = "kkernel.exe" if sys.platform == "win32" else "kkernel"


def _make_binary(directory: Path) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / EXE
    path.write_bytes(b"#!/bin/sh\nexit 0\n")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


@pytest.fixture()
def isolated(tmp_path, monkeypatch):
    """No KKERNEL, empty scripts directories, an empty PATH."""
    scripts = tmp_path / "scripts"
    scripts.mkdir()
    monkeypatch.delenv("KKERNEL", raising=False)
    monkeypatch.setattr(binary, "_scripts_dirs", lambda: [scripts])
    path_dir = tmp_path / "path"
    path_dir.mkdir()
    monkeypatch.setenv("PATH", str(path_dir))
    return scripts, path_dir


def test_override_wins_over_installed_and_path(isolated, tmp_path, monkeypatch):
    scripts, path_dir = isolated
    _make_binary(scripts)
    _make_binary(path_dir)
    chosen = _make_binary(tmp_path / "override")
    monkeypatch.setenv("KKERNEL", str(chosen))
    assert find_kkernel() == chosen


def test_override_that_is_not_a_file_is_an_error(isolated, tmp_path, monkeypatch):
    scripts, _ = isolated
    _make_binary(scripts)
    monkeypatch.setenv("KKERNEL", str(tmp_path / "missing"))
    with pytest.raises(FileNotFoundError, match="KKERNEL="):
        find_kkernel()


def test_installed_binary_wins_over_path(isolated):
    scripts, path_dir = isolated
    installed = _make_binary(scripts)
    _make_binary(path_dir)
    assert find_kkernel() == installed


def test_path_is_the_fallback(isolated):
    _, path_dir = isolated
    on_path = _make_binary(path_dir)
    assert find_kkernel() == on_path


def test_not_found_names_every_place_checked(isolated):
    scripts, _ = isolated
    with pytest.raises(FileNotFoundError) as excinfo:
        find_kkernel()
    message = str(excinfo.value)
    assert str(scripts / EXE) in message
    assert "kkernel on PATH" in message


def test_default_dirs_include_this_interpreters_scripts_dir():
    import sysconfig

    assert Path(sysconfig.get_path("scripts")) in binary._scripts_dirs()
    assert all(isinstance(d, Path) for d in binary._scripts_dirs())
