#!/usr/bin/env python3
"""Contract tests for the fail-closed writer-path census."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import subprocess
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "writer_census.py"
SOURCE_REVISION = subprocess.run(
    ["git", "rev-parse", "HEAD"],
    cwd=REPO_ROOT,
    check=True,
    capture_output=True,
    text=True,
).stdout.strip()
_parent = subprocess.run(
    ["git", "rev-parse", "--verify", "HEAD~1"],
    cwd=REPO_ROOT,
    check=False,
    capture_output=True,
    text=True,
)
PARENT_REVISION = _parent.stdout.strip() if _parent.returncode == 0 else None


def load_module():
    spec = importlib.util.spec_from_file_location("writer_census", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def inventory(
    *verbs: tuple[str, str], packs: tuple[str, ...] = ()
) -> list[dict]:
    by_pack: dict[str, list[dict]] = {pack: [] for pack in packs}
    for pack, verb in verbs:
        by_pack.setdefault(pack, []).append(
            {"name": verb, "visibility": "verb"}
        )
    return [
        {"name": pack, "verbs": entries}
        for pack, entries in sorted(by_pack.items())
    ]


def manifest(*, control_classification: str = "WRITER") -> dict:
    return {
        "schema_version": "khive.writer-census.manifest.v1",
        "source_revision": SOURCE_REVISION,
        "pack_set": ["comm", "memory", "brain"],
        "inventory": {
            "comm": ["comm.read"],
            "memory": ["memory.recall"],
            "brain": [],
        },
        "control": {
            "verb": "comm.read",
            "required_classification": "WRITER",
        },
        "defaults": {
            "classification": "UNKNOWN",
            "reason": "handler trace incomplete",
            "paths": [
                {
                    "classification": "WRITER-COND",
                    "condition": "event_store_configured",
                    "kind": "dispatch_audit",
                    "symbol": "VerbRegistry::dispatch_with_identity",
                    "evidence": {
                        "path": "crates/khive-runtime/src/pack.rs",
                        "required_patterns": [
                            "async fn append_audit_event_best_effort(",
                            "store.append_event(event).await",
                        ],
                    },
                }
            ],
        },
        "internal_handlers": {
            "brain.record_serve": {
                "classification": "WRITER",
                "reason": "serve ledger batch acquires SqlWriter",
                "paths": [
                    {
                        "classification": "WRITER",
                        "kind": "sqlite",
                        "symbol": "serve_ledger::record_serves -> SqlAccess::writer",
                        "evidence": {
                            "path": "crates/khive-pack-brain/src/serve_ledger.rs",
                            "required_patterns": [
                                "pub(crate) async fn record_serves(",
                                "let mut writer = sql.writer().await",
                            ],
                        },
                    }
                ],
            }
        },
        "overrides": {
            "comm.read": {
                "classification": control_classification,
                "reason": "read flag mutation attempts a SQLite write",
                "paths": [
                    {
                        "classification": "WRITER",
                        "kind": "sqlite",
                        "symbol": "mark_read_target -> try_patch_note_property",
                        "evidence": {
                            "path": "crates/khive-pack-comm/src/handlers.rs",
                            "required_patterns": [
                                "async fn mark_read_target(",
                                ".try_patch_note_property(",
                            ],
                        },
                    }
                ],
            },
            "memory.recall": {
                "classification": "UNKNOWN",
                "reason": "direct handler trace incomplete",
                "nested_dispatches": [
                    {
                        "condition": "brain pack registered and results non-empty",
                        "target": "brain.record_serve",
                        "via_registry": True,
                    }
                ],
            },
        },
    }


class WriterCensusTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.census = load_module()

    def test_missing_manifest_entry_fails_closed_and_output_is_stable(self):
        data = manifest()
        data["inventory"]["memory"] = []
        del data["overrides"]["memory.recall"]
        live = inventory(
            ("comm", "comm.read"),
            ("memory", "memory.recall"),
            packs=("comm", "memory", "brain"),
        )

        first = self.census.build_report(
            data, live, observed_revision=SOURCE_REVISION
        )
        second = self.census.build_report(
            data, live, observed_revision=SOURCE_REVISION
        )

        self.assertEqual(first["status"], "OK")
        by_verb = {entry["verb"]: entry for entry in first["entries"]}
        self.assertEqual(by_verb["memory.recall"]["classification"], "UNKNOWN")
        self.assertEqual(
            by_verb["memory.recall"]["reason"], "manifest entry missing"
        )
        self.assertEqual(
            self.census.canonical_json(first), self.census.canonical_json(second)
        )

    def test_missing_known_positive_voids_run_without_a_table(self):
        report = self.census.build_report(
            manifest(control_classification="UNKNOWN"),
            inventory(
                ("comm", "comm.read"),
                ("memory", "memory.recall"),
                packs=("comm", "memory", "brain"),
            ),
            observed_revision=SOURCE_REVISION,
        )

        self.assertEqual(report["status"], "VOID")
        self.assertNotIn("entries", report)
        self.assertNotIn("summary", report)
        self.assertIn("comm.read", " ".join(report["errors"]))

    def test_nested_dispatch_is_resolved_in_the_public_row(self):
        report = self.census.build_report(
            manifest(),
            inventory(
                ("comm", "comm.read"),
                ("memory", "memory.recall"),
                packs=("comm", "memory", "brain"),
            ),
            observed_revision=SOURCE_REVISION,
        )

        recall = next(
            entry for entry in report["entries"] if entry["verb"] == "memory.recall"
        )
        self.assertEqual(recall["classification"], "UNKNOWN")
        nested = recall["nested_dispatches"]
        self.assertEqual(len(nested), 1)
        self.assertEqual(nested[0]["target"], "brain.record_serve")
        self.assertEqual(nested[0]["resolved_classification"], "WRITER")
        self.assertEqual(
            {path["symbol"] for path in nested[0]["writer_paths"]},
            {
                "VerbRegistry::dispatch_with_identity",
                "serve_ledger::record_serves -> SqlAccess::writer",
            },
        )

    def test_stale_known_positive_evidence_voids_run(self):
        data = manifest()
        data["overrides"]["comm.read"]["paths"][0]["evidence"][
            "required_patterns"
        ].append("this pattern cannot exist in the pinned source")

        report = self.census.build_report(
            data,
            inventory(
                ("comm", "comm.read"),
                ("memory", "memory.recall"),
                packs=("comm", "memory", "brain"),
            ),
            observed_revision=SOURCE_REVISION,
            repo_root=REPO_ROOT,
        )

        self.assertEqual(report["status"], "VOID")
        self.assertNotIn("entries", report)
        self.assertIn("comm.read", " ".join(report["errors"]))

    def test_unreachable_observed_revision_voids_run_via_control(self):
        report = self.census.build_report(
            manifest(),
            inventory(
                ("comm", "comm.read"),
                ("memory", "memory.recall"),
                packs=("comm", "memory", "brain"),
            ),
            observed_revision="b" * 40,
        )

        self.assertEqual(report["status"], "VOID")
        self.assertNotIn("entries", report)
        self.assertIn("control", " ".join(report["errors"]))

    def test_revision_mismatch_reverifies_evidence_at_observed_revision(self):
        if PARENT_REVISION is None:
            self.skipTest("HEAD~1 unavailable (shallow clone)")
        pinned_elsewhere = manifest()
        pinned_elsewhere["source_revision"] = PARENT_REVISION
        report = self.census.build_report(
            pinned_elsewhere,
            inventory(
                ("comm", "comm.read"),
                ("memory", "memory.recall"),
                packs=("comm", "memory", "brain"),
            ),
            observed_revision=SOURCE_REVISION,
        )

        self.assertEqual(report["status"], "OK")
        self.assertEqual(report["control"]["status"], "PASS")
        self.assertIn(
            "re-verified at the observed revision",
            " ".join(report["warnings"]),
        )

    def test_absent_observed_revision_still_voids_run(self):
        report = self.census.build_report(
            manifest(),
            inventory(
                ("comm", "comm.read"),
                ("memory", "memory.recall"),
                packs=("comm", "memory", "brain"),
            ),
            observed_revision=None,
        )

        self.assertEqual(report["status"], "VOID")
        self.assertIn("absent or invalid", " ".join(report["errors"]))

    def test_no_writer_without_surviving_evidence_fails_closed(self):
        stripped = manifest()
        stripped["overrides"]["memory.recall"] = {
            "classification": "NO-WRITER",
            "reason": "declared read-only",
            "trace_complete": True,
            "inherit_default_paths": False,
            "paths": [],
        }
        report = self.census.build_report(
            manifest=stripped,
            observed_inventory=inventory(
                ("comm", "comm.read"),
                ("memory", "memory.recall"),
                packs=("comm", "memory", "brain"),
            ),
            observed_revision=SOURCE_REVISION,
        )

        self.assertEqual(report["status"], "OK")
        entry = next(
            row for row in report["entries"] if row["verb"] == "memory.recall"
        )
        self.assertEqual(entry["classification"], "UNKNOWN")
        self.assertIn(
            "no verified read-only evidence",
            entry["reason"],
        )

    def test_ci_lint_phase_runs_writer_census_contracts(self):
        ci = (REPO_ROOT / "scripts" / "ci.sh").read_text()
        self.assertIn(
            'python3 "$SCRIPT_DIR/tests/test_writer_census.py"', ci
        )

    def test_closed_vocabulary_rejects_unreviewed_classification(self):
        data = manifest()
        data["overrides"]["comm.read"]["classification"] = "MAYBE"

        with self.assertRaisesRegex(self.census.CensusError, "MAYBE"):
            self.census.build_report(
                data,
                inventory(
                    ("comm", "comm.read"),
                    ("memory", "memory.recall"),
                    packs=("comm", "memory", "brain"),
                ),
                observed_revision=SOURCE_REVISION,
            )

    def test_non_string_classification_fails_closed(self):
        data = manifest()
        data["defaults"]["classification"] = []

        with self.assertRaisesRegex(self.census.CensusError, "classification"):
            self.census.build_report(
                data,
                inventory(
                    ("comm", "comm.read"),
                    ("memory", "memory.recall"),
                    packs=("comm", "memory", "brain"),
                ),
                observed_revision=SOURCE_REVISION,
            )

    def test_writer_cond_requires_a_named_condition(self):
        data = manifest()
        data["defaults"]["paths"][0]["condition"] = "  "

        with self.assertRaisesRegex(self.census.CensusError, "condition"):
            self.census.build_report(
                data,
                inventory(
                    ("comm", "comm.read"),
                    ("memory", "memory.recall"),
                    packs=("comm", "memory", "brain"),
                ),
                observed_revision=SOURCE_REVISION,
            )

    def test_no_writer_cannot_survive_verified_writer_evidence(self):
        data = manifest()
        data["overrides"]["memory.recall"] = {
            "classification": "NO-WRITER",
            "reason": "synthetic false read-only claim",
            "trace_complete": True,
            "paths": [],
        }

        report = self.census.build_report(
            data,
            inventory(
                ("comm", "comm.read"),
                ("memory", "memory.recall"),
                packs=("comm", "memory", "brain"),
            ),
            observed_revision=SOURCE_REVISION,
        )

        recall = next(
            entry for entry in report["entries"] if entry["verb"] == "memory.recall"
        )
        self.assertEqual(recall["classification"], "UNKNOWN")
        self.assertIn("contradicted", recall["reason"])

    def test_baseline_keeps_untraced_external_paths_unknown(self):
        baseline = json.loads(
            (REPO_ROOT / "scripts" / "data" / "writer-census-v1.json").read_text()
        )
        unknown = {
            "blob.put",
            "code.ingest",
            "git.branch",
            "git.commit",
            "git.push",
        }
        self.assertEqual(
            {
                verb
                for verb in unknown
                if baseline["overrides"][verb]["classification"] == "UNKNOWN"
            },
            unknown,
        )


if __name__ == "__main__":
    unittest.main()
