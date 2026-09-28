#!/usr/bin/env python3
"""Fixture-contract tests for the pipeline daemon benchmark (stdlib unittest)."""

from __future__ import annotations

import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
import bench_pipeline_daemon as bpd


TOPIC = "memory recall"


def _on_topic(index):
    return {"content": f"{TOPIC} note {index}"}


def _off_topic(index):
    return {"content": f"other topic note {index}"}


class RecallResponseTests(unittest.TestCase):
    def _query(self, payload):
        with mock.patch.object(bpd, "_call_verb", return_value=payload):
            _, contents = bpd._query_once(None, TOPIC)
        return contents

    def _assert_rejected(self, payload, message):
        with mock.patch.object(bpd, "_call_verb", return_value=payload):
            with self.assertRaisesRegex(RuntimeError, message):
                bpd._query_once(None, TOPIC)

    def test_twenty_distinct_on_topic_hits_exceed_limit(self):
        self._assert_rejected({"results": [_on_topic(i) for i in range(20)]}, "exceeding limit")

    def test_on_topic_hit_at_rank_eleven_exceeds_limit(self):
        rows = [_on_topic(i) for i in range(6)]
        rows += [_off_topic(i) for i in range(4)]
        rows.append(_on_topic(6))
        self._assert_rejected({"results": rows}, "exceeding limit")

    def test_repeated_on_topic_hit_is_rejected(self):
        self._assert_rejected({"results": [_on_topic(0)] * bpd.TOP_K}, "duplicate content")

    def test_missing_content_is_rejected(self):
        self._assert_rejected({"results": [_on_topic(0), {"id": "missing"}]}, "string content")

    def test_empty_results_does_not_fall_back_to_items(self):
        self._assert_rejected(
            {"results": [], "items": [_on_topic(i) for i in range(7)]},
            "exactly one",
        )

    def test_other_malformed_shapes_are_rejected(self):
        malformed = [
            (None, "list or object"),
            ({}, "exactly one"),
            ({"results": None}, "must be a list"),
            ({"results": [None]}, "string content"),
            ({"results": [{"content": 1}]}, "string content"),
        ]
        for payload, message in malformed:
            with self.subTest(payload=payload):
                self._assert_rejected(payload, message)

    def test_seven_on_topic_and_three_off_topic_score_point_seven(self):
        rows = [_on_topic(i) for i in range(7)]
        rows += [_off_topic(i) for i in range(3)]
        contents = self._query({"results": rows})
        self.assertEqual(bpd.precision_at_k(contents, TOPIC), 0.7)

    def test_single_on_topic_hit_scores_point_one(self):
        contents = self._query([_on_topic(0)])
        self.assertEqual(bpd.precision_at_k(contents, TOPIC), 0.1)

    def test_empty_list_scores_zero(self):
        contents = self._query({"items": []})
        self.assertEqual(bpd.precision_at_k(contents, TOPIC), 0.0)

    def test_query_requests_top_k_and_preserves_fusion_strategy(self):
        with mock.patch.object(bpd, "_call_verb", return_value={"results": []}) as call:
            bpd._query_once(None, TOPIC, fusion_strategy="vector_only")
        call.assert_called_once_with(None, "memory.recall", {
            "query": TOPIC,
            "limit": bpd.TOP_K,
            "full_content": True,
            "fusion_strategy": "vector_only",
        })


if __name__ == "__main__":
    unittest.main()
