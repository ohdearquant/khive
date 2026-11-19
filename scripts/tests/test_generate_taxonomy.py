#!/usr/bin/env python3
"""Taxonomy projection and command failures, using synthetic schema responses only."""

from __future__ import annotations

import copy
import importlib.util
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/generate-taxonomy.py"
spec = importlib.util.spec_from_file_location("generate_taxonomy", SCRIPT)
taxonomy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(taxonomy)


def synthetic_schema():
    base = {
        "origin": "base", "source_substrate": "entity", "source": "*",
        "relation": "instance_of", "target_substrate": "entity", "target": "concept",
    }
    typed = {
        "origin": "pack:fixture", "source_substrate": "entity", "source": "concept",
        "source_entity_type": "proof", "relation": "supports", "target_substrate": "entity",
        "target": "concept", "target_entity_type": "claim",
    }
    note = {
        "origin": "pack:fixture", "source_substrate": "note", "source": "fixture_note",
        "relation": "supports", "target_substrate": "note", "target": "observation",
    }
    result = {
        "entity_kinds": ["concept", "fixture_entity"],
        "note_kinds": ["fixture_note", "observation"],
        "edge_relations": ["instance_of", "supports"],
        "packs_loaded": ["fixture", "kg"],
        "endpoint_rules": [base, typed, note, copy.deepcopy(note)],
        "contract_version": "ab" * 32,
    }
    result["counts"] = {field: len(result[field]) for field in taxonomy.ARRAYS}
    return result


def envelope(schema):
    return {
        "results": [{"tool": "schema", "ok": True, "result": schema}],
        "summary": {"total": 1, "succeeded": 1, "failed": 0},
    }


class TaxonomyTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="taxonomy-test-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.document = self.directory / "guide.md"
        self.source = self.directory / "schema.json"
        self.schema = synthetic_schema()
        self.source.write_text(json.dumps(envelope(self.schema)), encoding="utf-8")
        self.original = (
            "# Handwritten introduction\r\n\r\n" + taxonomy.BEGIN + "\n\nold taxonomy\n\n"
            + taxonomy.END + "\n\n## Handwritten footer\r\nKeep this byte-for-byte.\r\n"
        )
        self.document.write_bytes(self.original.encode())

    def run_generator(self, *args, env=None, script=SCRIPT):
        source = [] if "--kkernel" in args else ["--schema-json", str(self.source)]
        return subprocess.run(
            [sys.executable, str(script), *source, "--document", str(self.document), *args],
            cwd=self.directory, env=env, text=True, capture_output=True, timeout=10,
            check=False,
        )

    def generate(self):
        result = self.run_generator("--write")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return self.document.read_bytes()

    def fake_binary(self, response=None, *, status=0, delay=0):
        binary = self.directory / "fake kkernel"
        capture = self.directory / "invocation.json"
        output = json.dumps(envelope(self.schema)) if response is None else response
        binary.write_text(
            f"#!{sys.executable}\n"
            "import json, os, pathlib, stat, sys, time\n"
            "args = sys.argv[1:]\n"
            "config = pathlib.Path(args[args.index('--config') + 1])\n"
            f"pathlib.Path({str(capture)!r}).write_text(json.dumps({{\n"
            "    'args': args, 'cwd': os.getcwd(), 'home': os.environ.get('HOME'),\n"
            "    'env': {k: v for k, v in os.environ.items() if k.startswith('KHIVE_')},\n"
            "    'config': config.read_text(), 'config_mode': stat.S_IMODE(config.stat().st_mode),\n"
            "    'directory_mode': stat.S_IMODE(config.parent.stat().st_mode),\n"
            "}))\n"
            f"time.sleep({delay!r})\n"
            f"print({output!r})\n"
            f"sys.exit({status})\n",
            encoding="utf-8",
        )
        binary.chmod(0o700)
        return binary, capture

    def test_cli_generation_check_and_one_word_drift_preserve_unrelated_bytes(self):
        generated = self.generate()
        prefix, _, suffix = self.original.partition(taxonomy.BEGIN)
        self.assertTrue(generated.startswith((prefix + taxonomy.BEGIN).encode()))
        self.assertTrue(generated.endswith((taxonomy.END + suffix.split(taxonomy.END)[1]).encode()))
        result = self.run_generator()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.document.read_bytes(), generated)

        outside = generated.replace(b"Handwritten introduction", b"Edited introduction")
        self.document.write_bytes(outside)
        self.assertEqual(self.run_generator("--check").returncode, 0)
        changed = outside.replace(b"fixture_entity", b"misspelled_entity", 1)
        self.document.write_bytes(changed)
        result = self.run_generator("--check")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("-", result.stderr)
        self.assertIn("misspelled_entity", result.stderr)
        self.assertEqual(self.document.read_bytes(), changed)
        self.assertEqual(self.generate(), outside)

    def test_projection_preserves_wildcard_owner_substrate_types_and_duplicates(self):
        generated = self.generate().decode()
        rows = [[cell.strip() for cell in line.split("|")[1:-1]]
                for line in generated.splitlines() if line.startswith("| ")]
        self.assertIn(["`base`", "`entity`", "`*`", "—", "`instance_of`",
                       "`entity`", "`concept`", "—"], rows)
        self.assertIn(["`pack:fixture`", "`entity`", "`concept`", "`proof`", "`supports`",
                       "`entity`", "`concept`", "`claim`"], rows)
        self.assertEqual(rows.count(["`pack:fixture`", "`note`", "`fixture_note`", "—",
                                     "`supports`", "`note`", "`observation`", "—"]), 2)
        for value in ("fixture_entity", "fixture_note", "instance_of", "supports", "ab" * 32):
            self.assertIn(f"`{value}`", generated)
        self.assertIn("Endpoint rules (4)", generated)

    def test_drift_prints_the_full_replacement_block_beyond_diff_context(self):
        first = "aaa_full_block_start"
        last = "zzz_full_block_end"
        self.schema["entity_kinds"] = [first, *(f"fixture_{n:02}" for n in range(40)), last]
        self.schema["counts"]["entity_kinds"] = len(self.schema["entity_kinds"])
        self.source.write_text(json.dumps(envelope(self.schema)), encoding="utf-8")
        generated = self.generate().decode()
        block = (taxonomy.BEGIN + generated.split(taxonomy.BEGIN, 1)[1]
                 .split(taxonomy.END, 1)[0] + taxonomy.END)
        changed = generated.replace("fixture_20", "misspelled_20", 1)
        self.document.write_bytes(changed.encode())
        result = self.run_generator("--check")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(block, result.stdout)
        for witness in (first, last, "ab" * 32, "`proof`", "`claim`"):
            self.assertIn(witness, result.stdout)
            self.assertNotIn(witness, result.stderr)
        self.assertEqual(result.stdout.count(taxonomy.BEGIN), 1)
        self.assertEqual(result.stdout.count(taxonomy.END), 1)
        self.assertEqual(self.document.read_bytes(), changed.encode())

    def test_changed_schema_and_hash_require_regeneration(self):
        before = self.generate()
        self.schema["entity_kinds"].append("another_entity")
        self.schema["counts"]["entity_kinds"] += 1
        self.schema["contract_version"] = "cd" * 32
        self.source.write_text(json.dumps(self.schema), encoding="utf-8")
        self.assertEqual(self.run_generator("--check").returncode, 1)
        self.assertEqual(self.document.read_bytes(), before)
        after = self.generate().decode()
        self.assertIn("Entity kinds (3)", after)
        self.assertIn("`another_entity`", after)
        self.assertIn("cd" * 32, after)

    def test_input_order_does_not_change_projection(self):
        before = self.generate()
        for field in taxonomy.ARRAYS:
            self.schema[field].reverse()
        self.source.write_text(json.dumps(self.schema), encoding="utf-8")
        self.assertEqual(self.run_generator("--check").returncode, 0)
        self.assertEqual(self.document.read_bytes(), before)

    def test_missing_duplicate_reversed_and_inline_markers_refuse_without_writing(self):
        for original in (
            "No markers", taxonomy.BEGIN, taxonomy.END + "\n" + taxonomy.BEGIN,
            self.original + taxonomy.BEGIN, self.original + taxonomy.END,
            self.original.replace(taxonomy.BEGIN, "prefix " + taxonomy.BEGIN),
            self.original.replace(taxonomy.END, taxonomy.END + " suffix"),
        ):
            with self.subTest(original=original):
                self.document.write_bytes(original.encode())
                result = self.run_generator("--write")
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("marker", result.stderr)
                self.assertEqual(self.document.read_bytes(), original.encode())

    def test_bad_schema_and_failed_envelopes_refuse_without_writing(self):
        bad_count = copy.deepcopy(self.schema)
        bad_count["counts"]["endpoint_rules"] = 3
        bad_type = copy.deepcopy(self.schema)
        bad_type["endpoint_rules"][2]["source_entity_type"] = "not-an-entity"
        unknown = copy.deepcopy(self.schema)
        unknown["endpoint_rules"][0]["constraint"] = "unrendered"
        bad_origin = copy.deepcopy(self.schema)
        bad_origin["endpoint_rules"][0]["origin"] = "pack:missing"
        bad_hash = copy.deepcopy(self.schema)
        bad_hash["contract_version"] = "short"
        malformed = [None, {}, [], "not-json", bad_count, bad_type, unknown, bad_origin, bad_hash,
                     {"results": [], "summary": {"total": 0, "succeeded": 0, "failed": 0}}]
        for field, value in (("ok", False), ("ok", "true"), ("tool", "verbs")):
            bad = envelope(self.schema)
            bad["results"][0][field] = value
            malformed.append(bad)
        for value in malformed:
            with self.subTest(value=value):
                self.source.write_text(json.dumps(value), encoding="utf-8")
                result = self.run_generator("--write")
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(self.document.read_bytes(), self.original.encode())

    def test_markdown_delimiters_are_escaped_without_losing_values(self):
        self.schema["entity_kinds"] = ["contains|pipe", "`quoted`"]
        rendered = taxonomy.render_schema(self.schema)
        self.assertIn("`contains\\|pipe`", rendered)
        self.assertIn("`` `quoted` ``", rendered)

    def test_live_acquisition_selects_defaults_and_isolates_config_without_changing_parent(self):
        binary, capture = self.fake_binary()
        env = {**os.environ, "KHIVE_PACKS": "foreign", "KHIVE_DB": "/do-not-open.db",
               "KHIVE_CONFIG": "/do-not-read.toml", "KHIVE_NO_EMBED": "1",
               "KHIVE_OUTPUT_FORMAT": "table", "KHIVE_ACTOR": "foreign"}
        before = dict(os.environ)
        result = self.run_generator("--kkernel", str(binary), "--write", env=env)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        observed = json.loads(capture.read_text())
        self.assertEqual(observed["args"][:3], ["exec", "schema()", "--db"])
        args = observed["args"]
        for flag, expected in (("--db", ":memory:"), ("--namespace", "local"),
                               ("--actor", "local"), ("--expect-actor", "local"),
                               ("--presentation", "verbose"), ("--output-format", "json")):
            self.assertEqual(args[args.index(flag) + 1], expected)
        self.assertIn("--strict", args)
        self.assertNotIn("--pack", args)
        self.assertNotIn("--no-embed", args)
        self.assertEqual(observed["env"], {"KHIVE_NO_DAEMON": "1"})
        self.assertEqual(observed["home"], env.get("HOME"))
        self.assertEqual(observed["config"], "")
        self.assertEqual(observed["config_mode"], 0o600)
        self.assertEqual(observed["directory_mode"], 0o700)
        self.assertEqual(Path(args[args.index("--config") + 1]).parent.resolve(),
                         Path(observed["cwd"]).resolve())
        self.assertFalse(Path(observed["cwd"]).exists())
        self.assertEqual(dict(os.environ), before)

    def test_live_failure_timeout_and_unwrapped_output_never_replace_document(self):
        for response, status, delay in ((None, 7, 0), ("invalid json", 0, 0),
                                        (json.dumps(self.schema), 0, 0), (None, 0, 1)):
            with self.subTest(response=response, status=status, delay=delay):
                binary, _ = self.fake_binary(response, status=status, delay=delay)
                args = ["--kkernel", str(binary), "--write"]
                if delay:
                    args += ["--timeout", "0.05"]
                result = self.run_generator(*args)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(self.document.read_bytes(), self.original.encode())
        result = self.run_generator("--kkernel", str(self.directory / "missing"), "--write")
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.document.read_bytes(), self.original.encode())

    def test_ci_smoke_phase_fails_on_drift_before_other_smoke_tests(self):
        binary, _ = self.fake_binary()
        root = self.directory / "ci-copy"
        scripts = root / "scripts"
        scripts.mkdir(parents=True)
        (root / "crates").mkdir()
        shutil.copy2(ROOT / "scripts/ci.sh", scripts / "ci.sh")
        shutil.copy2(SCRIPT, scripts / SCRIPT.name)
        guide = root / "AGENTS.md"
        guide.write_bytes(self.generate())
        shim = self.directory / "bin"
        shim.mkdir()
        log = self.directory / "python-commands"
        python = shim / "python3"
        python.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' \"$1\" >> {shlex.quote(str(log))}\n"
            f"case \"$1\" in */generate-taxonomy.py) exec {shlex.quote(sys.executable)} \"$@\";; esac\n",
            encoding="utf-8",
        )
        python.chmod(0o700)
        env = {**os.environ, "PATH": str(shim) + os.pathsep + os.environ["PATH"],
               "KKERNEL_BINARY": str(binary), "TEST_SCOPE": "full"}

        def run_phase():
            log.write_text("")
            return subprocess.run(["sh", str(scripts / "ci.sh"), "smoke-tests"], env=env,
                                  capture_output=True, text=True, timeout=10, check=False)

        positive = run_phase()
        self.assertEqual(positive.returncode, 0, positive.stdout + positive.stderr)
        self.assertIn("test_generate_taxonomy.py", log.read_text())
        self.assertIn("smoke_test.py", log.read_text())
        changed = guide.read_bytes().replace(b"fixture_entity", b"misspelled_entity", 1)
        guide.write_bytes(changed)
        negative = run_phase()
        self.assertEqual(negative.returncode, 1, negative.stdout + negative.stderr)
        self.assertIn("Taxonomy differs", negative.stderr)
        self.assertNotIn("smoke_test.py", log.read_text())
        self.assertEqual(guide.read_bytes(), changed)


if __name__ == "__main__":
    unittest.main()
