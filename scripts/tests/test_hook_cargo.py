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
    def run_fixture(self, mode, manifest_text, installed, ambient=None):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            scripts = root / "scripts"
            scripts.mkdir()
            (scripts / "hook-cargo.sh").write_bytes(
                (REPO_ROOT / "scripts/hook-cargo.sh").read_bytes()
            )
            crates = root / "crates"
            crates.mkdir()
            (crates / "Cargo.toml").write_text(manifest_text)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            log = root / "cargo-calls"

            def executable(name, body):
                path = bin_dir / name
                path.write_text(f"#!{sys.executable}\n{body}")
                path.chmod(0o755)

            executable("ps", "raise SystemExit(126)\n")
            executable(
                "rustup",
                "import os, sys\n"
                "if sys.argv[1:] != ['toolchain', 'list', '--quiet']:\n"
                "    raise SystemExit(2)\n"
                "print(os.environ['HOOK_INSTALLED_TOOLCHAINS'])\n",
            )
            executable(
                "cargo",
                "import json, os, sys\n"
                "with open(os.environ['HOOK_CALL_LOG'], 'a') as log:\n"
                "    log.write(json.dumps({'args': sys.argv[1:], "
                "'toolchain': os.environ.get('RUSTUP_TOOLCHAIN')}) + '\\n')\n",
            )
            env = {
                **os.environ,
                "PATH": f"{bin_dir}:{os.environ['PATH']}",
                "XDG_CONFIG_HOME": str(root / "config"),
                "HOOK_CALL_LOG": str(log),
                "HOOK_INSTALLED_TOOLCHAINS": installed,
            }
            env.pop("RUSTUP_TOOLCHAIN", None)
            if ambient is not None:
                env["RUSTUP_TOOLCHAIN"] = ambient
            result = subprocess.run(
                ["bash", str(scripts / "hook-cargo.sh"), mode],
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )
            calls = (
                [json.loads(line) for line in log.read_text().splitlines()]
                if log.exists()
                else []
            )
            return result, calls

    def test_fmt_and_clippy_use_manifest_toolchain(self):
        manifest = '[workspace.package]\nrust-version = "1.87.9"\n'
        installed = "1.87.9-aarch64-apple-darwin"
        for mode, args in (
            ("fmt", ["fmt", "--all", "--", "--check"]),
            ("clippy", ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]),
        ):
            with self.subTest(mode=mode):
                result, calls = self.run_fixture(mode, manifest, installed)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls, [{"args": args, "toolchain": "1.87.9"}])

    def test_ambient_toolchain_is_overridden(self):
        result, calls = self.run_fixture(
            "clippy",
            '[workspace.package]\nrust-version = "1.87.9"\n',
            "1.87.9-aarch64-apple-darwin",
            ambient="nightly",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[0]["toolchain"], "1.87.9")

    def test_missing_rust_version_refuses_before_cargo(self):
        result, calls = self.run_fixture(
            "fmt", '[workspace.package]\nversion = "0.9.0"\n', "1.87.9-aarch64-apple-darwin"
        )
        self.assertEqual(result.returncode, 3)
        self.assertIn("[workspace.package].rust-version", result.stderr)
        self.assertEqual(calls, [])

    def test_uninstalled_rust_version_refuses_before_cargo(self):
        result, calls = self.run_fixture(
            "fmt", '[workspace.package]\nrust-version = "1.87.9"\n', "1.95.0-aarch64-apple-darwin"
        )
        self.assertEqual(result.returncode, 3)
        self.assertIn("Rust toolchain 1.87.9 is not installed", result.stderr)
        self.assertIn("rustup toolchain install 1.87.9", result.stderr)
        self.assertEqual(calls, [])

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
                executable(
                    "rustup",
                    "import os, sys\n"
                    "if sys.argv[1:] != ['toolchain', 'list', '--quiet']:\n"
                    "    raise SystemExit(2)\n"
                    "print(os.environ['HOOK_INSTALLED_TOOLCHAINS'])\n",
                )
                manifest = (REPO_ROOT / "crates/Cargo.toml").read_text()
                toolchain = next(
                    line.split('"')[1]
                    for line in manifest.splitlines()
                    if line.startswith('rust-version = "')
                )
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
                        "HOOK_INSTALLED_TOOLCHAINS": f"{toolchain}-aarch64-apple-darwin",
                    },
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 37, result.stderr)
                expected = [["cargo", *cargo_args]]
                if mode == "clippy":
                    expected = [
                        ["flock", "-o", "-s", "-w", "1800", shared,
                         "flock", "-o", "-w", "1800", exclusive, "cargo", *cargo_args],
                        ["flock", "-o", "-w", "1800", exclusive, "cargo", *cargo_args],
                        ["cargo", *cargo_args],
                    ]
                self.assertEqual(
                    [json.loads(line) for line in log.read_text().splitlines()],
                    expected,
                )


if __name__ == "__main__":
    unittest.main()
