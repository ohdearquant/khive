#!/usr/bin/env python3
"""Regression tests for ingest_workspace_artifacts.py — pure functions only,
no DB / kkernel round-trip. Covers PR #1049 fix-round-1 findings:

  M1 - DSL $prev-literal escaping (dsl_escape)
  M2 - CRLF round-trip (dsl_escape)
  M3 - 48KiB cap on the FINAL encoded payload, incl. invalid-UTF-8 expansion
       and a multibyte char split at the boundary (cap_content)
  H3 - PR annotation scoped to this checkout's project entity, never a
       number-only cross-repository fallback (select_exact_project,
       filter_pr_by_number)

Run: python3 .khive/scripts/test_ingest_workspace_artifacts.py
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from ingest_workspace_artifacts import (  # noqa: E402
    MAX_CONTENT_BYTES,
    cap_content,
    dsl_escape,
    filter_pr_by_number,
    select_exact_project,
)


class DslEscapePrevRefTests(unittest.TestCase):
    def test_bare_dollar_prev_escaped(self):
        self.assertEqual(dsl_escape("$prev"), '\\\\$prev')

    def test_dollar_prev_dot_id_escaped(self):
        self.assertEqual(dsl_escape("$prev.id"), '\\\\$prev.id')

    def test_dollar_prev_bracket_index_escaped(self):
        self.assertEqual(dsl_escape("$prev[0].id"), '\\\\$prev[0].id')

    def test_dollar_prev_as_prefix_of_longer_content_still_escaped(self):
        # string_as_prev_ref matches on a $prev.<path> prefix of the WHOLE
        # value; over-escaping here is safe (round-trips to the same literal).
        s = "$prev.anything not a real path but still starts with the prefix"
        self.assertTrue(dsl_escape(s).startswith('\\\\$prev.'))

    def test_prev_not_at_start_is_not_escaped(self):
        s = "see $prev.id in the log"
        self.assertEqual(dsl_escape(s), s)

    def test_dollar_alone_not_escaped(self):
        self.assertEqual(dsl_escape("$5.00 total"), "$5.00 total")

    def test_round_trip_via_json_decode(self):
        # Simulates what khive-request actually does: JSON-decode the
        # dsl_str() literal, then re-check the decoded value against the
        # parser's own escape rule (strip one leading backslash iff what
        # follows is a $prev chain prefix).
        import json

        for original in ("$prev", "$prev.id", "$prev[0].nested", "$prev.a.b.c"):
            literal = f'"{dsl_escape(original)}"'
            decoded = json.loads(literal)
            self.assertTrue(decoded.startswith("\\"))
            rest = decoded[1:]
            self.assertTrue(
                rest == "$prev" or rest.startswith("$prev.") or rest.startswith("$prev[")
            )
            self.assertEqual(rest, original)


class DslEscapeCrlfTests(unittest.TestCase):
    def test_crlf_preserved_as_escape(self):
        self.assertEqual(dsl_escape("a\r\nb"), "a\\r\\nb")

    def test_lone_cr_preserved(self):
        self.assertEqual(dsl_escape("a\rb"), "a\\rb")

    def test_crlf_round_trip_via_json_decode(self):
        import json

        original = "line1\r\nline2\rline3\n"
        literal = f'"{dsl_escape(original)}"'
        decoded = json.loads(literal)
        self.assertEqual(decoded, original)


class CapContentTests(unittest.TestCase):
    def test_under_cap_not_truncated(self):
        raw = ("a" * (MAX_CONTENT_BYTES - 1)).encode("utf-8")
        text = cap_content(raw)
        self.assertEqual(len(text.encode("utf-8")), len(raw))
        self.assertNotIn("truncated", text)

    def test_exactly_at_cap_not_truncated(self):
        raw = ("a" * MAX_CONTENT_BYTES).encode("utf-8")
        text = cap_content(raw)
        self.assertEqual(len(text.encode("utf-8")), MAX_CONTENT_BYTES)
        self.assertNotIn("truncated", text)

    def test_over_cap_valid_utf8_is_truncated_and_capped(self):
        raw = ("a" * (MAX_CONTENT_BYTES + 10)).encode("utf-8")
        text = cap_content(raw)
        self.assertLessEqual(len(text.encode("utf-8")), MAX_CONTENT_BYTES)
        self.assertIn("truncated", text)

    def test_invalid_utf8_at_exactly_cap_length_is_still_capped(self):
        # H3/M3 regression: MAX_CONTENT_BYTES of 0xFF is invalid UTF-8; the
        # OLD code compared len(raw) > MAX_CONTENT_BYTES BEFORE replacement
        # decoding, so an exactly-at-cap invalid file skipped truncation
        # entirely and ballooned to 3x size (each byte -> U+FFFD, 3 bytes).
        raw = b"\xff" * MAX_CONTENT_BYTES
        text = cap_content(raw)
        encoded_len = len(text.encode("utf-8"))
        self.assertLessEqual(
            encoded_len,
            MAX_CONTENT_BYTES,
            f"final encoded payload {encoded_len} bytes exceeds the {MAX_CONTENT_BYTES} cap",
        )

    def test_invalid_utf8_well_over_cap_is_capped(self):
        raw = b"\xff" * (MAX_CONTENT_BYTES + 1024)
        text = cap_content(raw)
        self.assertLessEqual(len(text.encode("utf-8")), MAX_CONTENT_BYTES)

    def test_multibyte_char_split_at_boundary_stays_valid_utf8(self):
        # A 3-byte char (€) straddling the truncation boundary must not
        # produce a malformed/half UTF-8 sequence in the output.
        head = "a" * (MAX_CONTENT_BYTES - 1)
        raw = (head + "€" + "tail-sentinel").encode("utf-8")
        text = cap_content(raw)
        encoded = text.encode("utf-8")
        self.assertLessEqual(len(encoded), MAX_CONTENT_BYTES)
        # round-trips cleanly (no stray surrogate / partial sequence)
        encoded.decode("utf-8")
        self.assertNotIn("tail-sentinel", text)

    def test_sha_key_computed_over_original_bytes_not_capped_text(self):
        # Sanity: cap_content itself does not touch hashing; verify the
        # truncation marker records the ORIGINAL raw length.
        raw = ("a" * (MAX_CONTENT_BYTES + 500)).encode("utf-8")
        text = cap_content(raw)
        self.assertIn(str(len(raw)), text)


class ProjectScopingTests(unittest.TestCase):
    def test_select_exact_project_single_match(self):
        rows = [{"id": "p1", "name": "khive-oss"}, {"id": "p2", "name": "lattice"}]
        self.assertEqual(select_exact_project(rows, "khive-oss"), "p1")

    def test_select_exact_project_no_match_returns_none(self):
        rows = [{"id": "p2", "name": "lattice"}]
        self.assertIsNone(select_exact_project(rows, "khive-oss"))

    def test_select_exact_project_ambiguous_returns_none(self):
        rows = [{"id": "p1", "name": "khive-oss"}, {"id": "p1b", "name": "khive-oss"}]
        self.assertIsNone(select_exact_project(rows, "khive-oss"))

    def test_filter_pr_by_number_scopes_to_project(self):
        rows = [
            {"id": "n1", "properties": {"number": 1049, "project_id": "khive-oss-id"}},
            {"id": "n2", "properties": {"number": 1049, "project_id": "lattice-id"}},
        ]
        by_number = filter_pr_by_number(rows, "khive-oss-id")
        self.assertEqual(by_number, {1049: "n1"})

    def test_filter_pr_by_number_cross_repo_collision_not_used(self):
        # Regression for H3: same PR number in two different repos must
        # never resolve to the wrong project's note.
        rows = [
            {"id": "khive-pr", "properties": {"number": 1041, "project_id": "khive-oss-id"}},
            {"id": "lionagi-pr", "properties": {"number": 1041, "project_id": "lionagi-id"}},
            {"id": "lattice-pr", "properties": {"number": 1041, "project_id": "lattice-id"}},
        ]
        by_number = filter_pr_by_number(rows, "lattice-id")
        self.assertEqual(by_number, {1041: "lattice-pr"})

    def test_filter_pr_by_number_unresolved_project_yields_no_matches(self):
        rows = [{"id": "n1", "properties": {"number": 1049, "project_id": "khive-oss-id"}}]
        self.assertEqual(filter_pr_by_number(rows, None), {})

    def test_filter_pr_by_number_ignores_rows_missing_project_id(self):
        rows = [{"id": "n1", "properties": {"number": 1049}}]
        self.assertEqual(filter_pr_by_number(rows, "khive-oss-id"), {})


if __name__ == "__main__":
    unittest.main(verbosity=2)
