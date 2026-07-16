#!/usr/bin/env python3
"""Regression tests for ingest_workspace_artifacts.py — pure functions only,
no DB / kkernel round-trip, except the fake-KKernel reconciliation tests at
the bottom (no subprocess either). Covers PR #1049 fix-round-1 findings:

  M1 - DSL $prev-literal escaping (dsl_escape)
  M2 - CRLF round-trip (dsl_escape)
  M3 - 32,768-byte cap on the FINAL encoded payload (the daemon embedder's
       actual limit — byte-counted despite its "chars" error wording), incl.
       invalid-UTF-8 expansion and a multibyte char split at the boundary
       (cap_content)
  H3 - PR annotation scoped to this checkout's project entity, never a
       number-only cross-repository fallback (select_exact_project,
       filter_pr_by_number)

...and PR #1049 fix-round-2 findings:

  H1 - resume reconciliation: a note matching an artifact's key must have
       its required `annotates` edges verified/backfilled, not accepted as
       complete on sight (compute_annotate_targets, missing_annotate_targets,
       reconcile_existing_note)

Run: python3 .khive/scripts/test_ingest_workspace_artifacts.py
"""

from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from ingest_workspace_artifacts import (  # noqa: E402
    MAX_CONTENT_BYTES,
    Artifact,
    Stats,
    cap_content,
    compute_annotate_targets,
    dsl_escape,
    filter_pr_by_number,
    missing_annotate_targets,
    reconcile_existing_note,
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

    def test_live_repro_ascii_doc_over_embed_limit_is_capped(self):
        # Live tranche-2 abort 2026-07-16: a 36,556-byte ASCII doc passed the
        # OLD 48KiB cap untouched, then the daemon embedder rejected the whole
        # create ("text too long: 36556 chars exceeds maximum 32768 chars" —
        # byte-counted despite the wording). The cap must sit at the
        # embedder's actual limit.
        raw = ("a" * 36_556).encode("utf-8")
        text = cap_content(raw)
        self.assertLessEqual(len(text.encode("utf-8")), MAX_CONTENT_BYTES)
        self.assertIn("truncated", text)

    def test_live_repro_multibyte_expansion_is_capped_in_bytes(self):
        # Second live abort same day: a 32,768-CHAR cap still failed at
        # 32,846 (bytes) once multibyte chars expanded. The cap must bound
        # the ENCODED byte length, not the char count.
        raw = ("€" * 26 + "a" * 32_742).encode("utf-8")  # 32,768 chars, 32,820 bytes
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


class ComputeAnnotateTargetsTests(unittest.TestCase):
    def test_workspace_doc_requires_only_workspace(self):
        a = Artifact(artifact_class="workspace_doc", path=Path("/x"), rel_path="x", lane="lane-a")
        self.assertEqual(compute_annotate_targets(a, "ws-1", {}), ["ws-1"])

    def test_codex_verdict_with_pr_hit_requires_both(self):
        a = Artifact(
            artifact_class="codex_verdict",
            path=Path("/x"),
            rel_path="x",
            lane="codex-reviews",
            pr_number=1049,
        )
        self.assertEqual(compute_annotate_targets(a, "ws-1", {1049: "pr-1"}), ["ws-1", "pr-1"])

    def test_codex_verdict_with_pr_miss_requires_only_workspace(self):
        a = Artifact(
            artifact_class="codex_verdict",
            path=Path("/x"),
            rel_path="x",
            lane="codex-reviews",
            pr_number=1049,
        )
        self.assertEqual(compute_annotate_targets(a, "ws-1", {}), ["ws-1"])

    def test_no_workspace_id_yields_no_targets(self):
        a = Artifact(artifact_class="workspace_doc", path=Path("/x"), rel_path="x", lane="lane-a")
        self.assertEqual(compute_annotate_targets(a, None, {}), [])


class MissingAnnotateTargetsTests(unittest.TestCase):
    def test_missing_workspace_edge(self):
        self.assertEqual(missing_annotate_targets(["ws-1", "pr-1"], {"pr-1"}), ["ws-1"])

    def test_missing_pr_edge(self):
        self.assertEqual(missing_annotate_targets(["ws-1", "pr-1"], {"ws-1"}), ["pr-1"])

    def test_nothing_missing(self):
        self.assertEqual(missing_annotate_targets(["ws-1", "pr-1"], {"ws-1", "pr-1"}), [])

    def test_all_missing(self):
        self.assertEqual(missing_annotate_targets(["ws-1", "pr-1"], set()), ["ws-1", "pr-1"])


class FakeKKernel:
    """Minimal ops-string-driven stand-in for KKernel — no subprocess. Answers
    `neighbors` reads from a fixed set of existing target ids and records
    every `link` write it's asked to perform."""

    def __init__(self, existing_annotate_targets: set[str]):
        self.live = True
        self._existing = existing_annotate_targets
        self.link_calls: list[tuple[str, str]] = []

    def read(self, ops: str) -> dict:
        assert ops.startswith("neighbors("), f"unexpected read ops: {ops}"
        hits = [{"id": t} for t in self._existing]
        return {"results": [{"ok": True, "result": hits}]}

    def write(self, ops: str) -> dict:
        assert ops.startswith("link("), f"unexpected write ops: {ops}"
        m = re.search(r'source_id="([^"]*)".*target_id="([^"]*)"', ops)
        assert m, f"could not parse link() ops: {ops}"
        self.link_calls.append((m.group(1), m.group(2)))
        return {"results": [{"ok": True, "result": {"ok": True}}]}


class ReconcileExistingNoteTests(unittest.TestCase):
    """H1 fix-round-2 regressions: a note matching an artifact's key is
    reconciled — missing required `annotates` edges are backfilled — instead
    of being accepted as complete on sight."""

    def test_backfills_missing_workspace_edge(self):
        a = Artifact(artifact_class="workspace_doc", path=Path("/x"), rel_path="x", lane="lane-a")
        kk = FakeKKernel(existing_annotate_targets=set())
        ws_cache = {"lane-a": "ws-1"}
        stats = Stats()
        reconcile_existing_note(kk, a, "note-1", ws_cache, {}, stats)
        self.assertEqual(kk.link_calls, [("note-1", "ws-1")])
        self.assertEqual(stats.edges_backfilled, 1)

    def test_backfills_missing_pr_edge_when_workspace_edge_already_present(self):
        a = Artifact(
            artifact_class="codex_verdict",
            path=Path("/x"),
            rel_path="x",
            lane="codex-reviews",
            pr_number=1049,
        )
        kk = FakeKKernel(existing_annotate_targets={"ws-1"})
        ws_cache = {"codex-reviews": "ws-1"}
        stats = Stats()
        reconcile_existing_note(kk, a, "note-1", ws_cache, {1049: "pr-1"}, stats)
        self.assertEqual(kk.link_calls, [("note-1", "pr-1")])
        self.assertEqual(stats.edges_backfilled, 1)

    def test_no_backfill_when_all_required_edges_present(self):
        a = Artifact(
            artifact_class="codex_verdict",
            path=Path("/x"),
            rel_path="x",
            lane="codex-reviews",
            pr_number=1049,
        )
        kk = FakeKKernel(existing_annotate_targets={"ws-1", "pr-1"})
        ws_cache = {"codex-reviews": "ws-1"}
        stats = Stats()
        reconcile_existing_note(kk, a, "note-1", ws_cache, {1049: "pr-1"}, stats)
        self.assertEqual(kk.link_calls, [])
        self.assertEqual(stats.edges_backfilled, 0)

    def test_failed_backfill_link_raises(self):
        # Round-3 regression: a rejected link write must raise (so the caller
        # never appends the cursor), not silently count as backfilled.
        class RejectingKKernel(FakeKKernel):
            def write(self, ops: str) -> dict:
                assert ops.startswith("link("), f"unexpected write ops: {ops}"
                return {"results": [{"ok": False, "error": "validation failed"}]}

        a = Artifact(artifact_class="workspace_doc", path=Path("/x"), rel_path="x", lane="lane-a")
        kk = RejectingKKernel(existing_annotate_targets=set())
        ws_cache = {"lane-a": "ws-1"}
        stats = Stats()
        with self.assertRaises(RuntimeError):
            reconcile_existing_note(kk, a, "note-1", ws_cache, {}, stats)
        self.assertEqual(stats.edges_backfilled, 0)

    def test_pr_miss_does_not_require_pr_edge(self):
        # No project-scoped pull_request match this run -> only the
        # workspace edge is required, even if it's a codex_verdict.
        a = Artifact(
            artifact_class="codex_verdict",
            path=Path("/x"),
            rel_path="x",
            lane="codex-reviews",
            pr_number=1049,
        )
        kk = FakeKKernel(existing_annotate_targets={"ws-1"})
        ws_cache = {"codex-reviews": "ws-1"}
        stats = Stats()
        reconcile_existing_note(kk, a, "note-1", ws_cache, {}, stats)
        self.assertEqual(kk.link_calls, [])
        self.assertEqual(stats.edges_backfilled, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)


class NulByteTests(unittest.TestCase):
    def test_nul_bytes_replaced_with_ufffd(self):
        # Live tranche-2 abort 2026-07-16: NUL is valid UTF-8, survived
        # replacement decoding, and crashed subprocess argv construction
        # ("embedded null byte"). cap_content must map it to U+FFFD.
        raw = b"before\x00after"
        text = cap_content(raw)
        self.assertNotIn("\x00", text)
        self.assertEqual(text, "before�after")
