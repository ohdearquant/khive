#!/usr/bin/env python3
"""A sandbox denying ps must not abort Cargo hooks or bypass their locks."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]


class CargoHookTests(unittest.TestCase):
    def test_denied_process_inspection_preserves_both_lock_modes(self):
        for mode, cargo_args in (
            ("fmt", ["fmt", "--all", "--", "--check"]),
            ("clippy", ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]),
        ):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                bin_dir = root / "bin"
                bin_dir.mkdir()
                config = root / "config" / "khive"
                config.mkdir(parents=True)
                shared = str(root / "shared lock")
                exclusive = str(root / "exclusive lock")
                (config / "cargo-hook-locks").write_text(
                    f"shared {shared}\nexclusive {exclusive}\n"
                )
                log = root / "calls"

                def executable(name, body):
                    path = bin_dir / name
                    path.write_text(f"#!{sys.executable}\n{body}")
                    path.chmod(0o755)

                executable("ps", "raise SystemExit(126)\n")
                executable("lsof", "raise SystemExit(1)\n")
                record = (
                    "import json, os, sys\n"
                    "with open(os.environ['HOOK_CALL_LOG'], 'a') as log:\n"
                    "    log.write(json.dumps([os.path.basename(sys.argv[0]), "
                    "*sys.argv[1:]]) + '\\n')\n"
                )
                executable(
                    "flock",
                    record
                    + "args = sys.argv[1:]\n"
                    + "command = args[args.index('1800') + 2:]\n"
                    + "os.execvp(command[0], command)\n",
                )
                # A nonzero Cargo exit also has to survive both wrappers.
                executable("cargo", record + "raise SystemExit(37)\n")
                result = subprocess.run(
                    ["bash", str(REPO_ROOT / "scripts/hook-cargo.sh"), mode],
                    env={
                        **os.environ,
                        "PATH": f"{bin_dir}:{os.environ['PATH']}",
                        "XDG_CONFIG_HOME": str(config.parent),
                        "HOOK_CALL_LOG": str(log),
                    },
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 37, result.stderr)
                self.assertEqual(
                    [json.loads(line) for line in log.read_text().splitlines()],
                    [
                        ["flock", "-o", "-s", "-w", "1800", shared,
                         "flock", "-o", "-w", "1800", exclusive, "cargo", *cargo_args],
                        ["flock", "-o", "-w", "1800", exclusive, "cargo", *cargo_args],
                        ["cargo", *cargo_args],
                    ],
                )


if __name__ == "__main__":
    unittest.main()
