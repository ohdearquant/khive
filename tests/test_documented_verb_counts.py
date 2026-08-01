#!/usr/bin/env python3

from pathlib import Path
import tempfile
import unittest

from documented_verb_counts import (
    RegistryCounts,
    registry_counts,
    scan_document,
    scan_repository,
    validate_documented_counts,
)


PACK_COUNTS = {"kg": 20, "comm": 8, "workspace": 0}


def verbs_result() -> dict:
    verbs = [
        {"verb": f"{pack}.{index}", "pack": pack}
        for pack, count in PACK_COUNTS.items()
        for index in range(count)
    ]
    return {"verbs": verbs, "total": len(verbs), "pack_counts": PACK_COUNTS}


class DocumentedVerbCountsTest(unittest.TestCase):
    def test_registry_counts_require_consistent_per_pack_summary(self) -> None:
        counts = registry_counts(verbs_result())
        self.assertEqual(counts.total_verbs, 28)
        self.assertEqual(counts.total_packs, 3)
        self.assertEqual(counts.pack_verbs["workspace"], 0)

        malformed = verbs_result()
        malformed["pack_counts"] = {**PACK_COUNTS, "comm": 7}
        with self.assertRaisesRegex(ValueError, "pack_counts"):
            registry_counts(malformed)

    def test_scanner_covers_requested_claim_forms(self) -> None:
        text = """A 28-verb runtime.
28 verbs across 3 packs.
verbs: 28
All 3 packs load by default.
The server config loads all three (`kg`, `comm`, `workspace`).
## `kg` pack — 20 verbs
| Pack | Verbs |
| --- | --- |
| **comm** | 8 |
| **workspace** | 0 |
"""
        claims = scan_document("README.md", text, PACK_COUNTS)
        forms = {claim.form for claim in claims}
        self.assertTrue({"hyphenated", "spaced", "inverted", "spelled", "per-pack-table"} <= forms)
        self.assertIn(("pack_verbs", "kg", 20), {(c.kind, c.pack, c.value) for c in claims})
        self.assertIn(("pack_verbs", "workspace", 0), {(c.kind, c.pack, c.value) for c in claims})

    def test_pack_path_supplies_named_window(self) -> None:
        claims = scan_document(
            "marketplace/khive/skills/comm/SKILL.md",
            "The surface is eight verbs, all public.\n",
            PACK_COUNTS,
        )
        self.assertEqual([(c.pack, c.value) for c in claims], [("comm", 8)])

        claims = scan_document(
            "crates/khive-pack-comm/docs/design.md",
            "HANDLERS = COMM_HANDLERS (8 entries)\n",
            PACK_COUNTS,
        )
        self.assertEqual([(c.pack, c.value) for c in claims], [("comm", 8)])

        claims = scan_document(
            "AGENTS.md",
            "The `comm` pack contributes\neight verbs to the public surface.\n",
            PACK_COUNTS,
        )
        self.assertEqual([(c.pack, c.value) for c in claims], [("comm", 8)])

    def test_scanner_ignores_subset_and_reference_numbers(self) -> None:
        text = """`propose` is the one verb that requires JSON form.
[ADR-017](docs/adr/ADR-017-pack-standard.md) defines the Pack trait.
The harness has a 7-pack `--packs` default.
"""
        self.assertEqual(
            scan_document("marketplace/khive/skills/kg/SKILL.md", text, PACK_COUNTS),
            [],
        )

    def test_merged_adrs_are_excluded(self) -> None:
        self.assertEqual(
            scan_document("docs/adr/ADR-999-history.md", "2 verbs across 1 pack\n", PACK_COUNTS),
            [],
        )

    def test_repository_validation_reports_stale_claim(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "README.md").write_text(
                "27 verbs across 3 packs.\n"
                "| Pack | Verbs |\n"
                "| --- | --- |\n"
                "| kg | 20 |\n"
                "| comm | 8 |\n"
                "| workspace | 0 |\n",
                encoding="utf-8",
            )
            errors = validate_documented_counts(root, verbs_result())
        self.assertEqual(len(errors), 1)
        self.assertIn("claims total verbs=27, registry says 28", errors[0])

    def test_repository_scan_ignores_adr_history(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            adr = root / "docs/adr/ADR-001-history.md"
            adr.parent.mkdir(parents=True)
            adr.write_text("1 verb across 1 pack\n", encoding="utf-8")
            counts = RegistryCounts(total_verbs=28, pack_verbs=PACK_COUNTS)
            self.assertEqual(scan_repository(root, counts), [])


if __name__ == "__main__":
    unittest.main()
