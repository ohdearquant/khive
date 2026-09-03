"""Shared kkernel binary path resolution for the Python test harnesses.

Mirrors `kkernel_binary_path` in scripts/ci.sh: KKERNEL_BINARY if set explicitly,
else CARGO_TARGET_DIR (absolute, or relative to crates/) if set, else
crates/target; then /release/kkernel. Every harness that needs to spawn the
built binary imports this instead of resolving the path itself, so a custom
CARGO_TARGET_DIR is honored the same way regardless of which harness process
runs it.
"""

import os
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def resolve_binary_path(env=None) -> str:
    env = os.environ if env is None else env
    if explicit := env.get("KKERNEL_BINARY"):
        return explicit

    target_dir = Path(env.get("CARGO_TARGET_DIR", "target"))
    if not target_dir.is_absolute():
        target_dir = REPO_ROOT / "crates" / target_dir
    return str(target_dir / "release" / "kkernel")
