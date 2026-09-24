"""Exercise the CI sentinel with isolated fake commands, without running Cargo."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


CI = Path(__file__).resolve().parents[1] / "ci.sh"
SOURCE = CI.read_text()
SENTINEL = SOURCE[
    SOURCE.index("sentinel_file_fingerprint() {") : SOURCE.index("phase_tests() {")
]


class EmptyHomeSentinelTests(unittest.TestCase):
    def run_child(self, command):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            operator_home = root / "operator-home"
            operator_home.mkdir()
            witness = operator_home / "preserve-me"
            witness.write_text("untouched")
            env = os.environ.copy()
            env["HOME"] = str(operator_home)
            env["TMPDIR"] = str(root)
            # Exercise the preservation of existing toolchain locations.
            env["CARGO_HOME"] = str(root / "cargo-tools")
            env["RUSTUP_HOME"] = str(root / "rustup-tools")
            result = subprocess.run(
                [
                    "sh", "-c", "set -e\n" + SENTINEL
                    + '\nrun_with_store_sentinel "$@"\n',
                    "sentinel-test", "sh", "-c", command,
                ],
                env=env,
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
            )
            self.assertEqual(witness.read_text(), "untouched")
            self.assertEqual(list(operator_home.iterdir()), [witness])
            self.assertFalse(list(root.glob("khive-ci-home.*")), "private HOME leaked")
            return result

    def test_clean_command_preserves_failure_status(self):
        for status in (0, 37):
            with self.subTest(status=status):
                result = self.run_child(f"exit {status}")
                self.assertEqual(result.returncode, status, result.stderr)

    def test_logs_locks_and_empty_directories_fail(self):
        for command in (
            'mkdir -p "$HOME/.khive/logs"; printf startup > "$HOME/.khive/logs/writer_timeouts.1.ndjson"',
            'mkdir -p "$HOME/.khive"; : > "$HOME/.khive/khived.recovery.lock"',
            'mkdir -p "$HOME/.khive"; : > "$HOME/.khive/khived.recoverer.lock"',
            'mkdir -p "$HOME/empty-directory"',
            'ln -s absent "$HOME/dangling-link"',
        ):
            with self.subTest(command=command):
                result = self.run_child(command)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("left files or directories", result.stderr)

    def test_model_cache_files_fail(self):
        result = self.run_child(
            'mkdir -p "$HOME/.lattice/models/all-minilm-l6-v2"; '
            ': > "$HOME/.lattice/models/all-minilm-l6-v2/model.safetensors"; '
            ': > "$HOME/.lattice/models/all-minilm-l6-v2/vocab.txt"'
        )
        self.assertEqual(result.returncode, 1, "model cache residue must fail the HOME sentinel")
        self.assertIn("left files or directories", result.stderr)


if __name__ == "__main__":
    unittest.main()
