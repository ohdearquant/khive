"""Locate the `kkernel` binary: the daemon and command-line tool this client talks to.

A platform wheel of this package installs `kkernel` into the environment's
scripts directory (`bin/`, or `Scripts\\` on Windows), next to the interpreter.
The database itself stays under `~/.khive`, shared by every environment.
"""

from __future__ import annotations

import os
import shutil
import sys
import sysconfig
from pathlib import Path

_EXE = "kkernel.exe" if sys.platform == "win32" else "kkernel"


def _scripts_dirs() -> list[Path]:
    """Directories an installer may have placed this package's scripts in."""
    dirs = [Path(sysconfig.get_path("scripts"))]
    user_scheme = sysconfig.get_preferred_scheme("user")
    dirs.append(Path(sysconfig.get_path("scripts", user_scheme)))
    # `pip install --target DIR` puts the package in DIR and its scripts in DIR/bin.
    dirs.append(Path(__file__).resolve().parent.parent / "bin")
    return list(dict.fromkeys(dirs))


def find_kkernel() -> Path:
    """Return the `kkernel` binary to run.

    Looks, in order, at the `KKERNEL` environment variable, the scripts
    directories this package's installer uses, and `kkernel` on PATH. A
    `KKERNEL` value that is not a file is an error rather than a fallthrough,
    so a mistyped override is never silently replaced by another binary.

    Raises:
        FileNotFoundError: no binary was found; the message names every place
            that was checked.
    """
    override = os.environ.get("KKERNEL")
    if override:
        path = Path(override)
        if not path.is_file():
            raise FileNotFoundError(f"KKERNEL={override} is not a file")
        return path

    looked = []
    for directory in _scripts_dirs():
        candidate = directory / _EXE
        if candidate.is_file():
            return candidate
        looked.append(str(candidate))

    on_path = shutil.which("kkernel")
    if on_path:
        return Path(on_path)
    looked.append("kkernel on PATH")
    raise FileNotFoundError(
        "no kkernel binary found; looked at "
        + ", ".join(looked)
        + ". Install a platform wheel of khive, put kkernel on PATH, or set KKERNEL."
    )
