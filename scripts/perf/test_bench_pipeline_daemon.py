#!/usr/bin/env python3
"""Response-shape regressions for the deterministic pipeline quality gate."""

import unittest
from unittest.mock import patch

import bench_pipeline_daemon as pipeline


class RecallScoreInputTests(unittest.TestCase):
    def score(self, response):
        with patch.object(pipeline, "_call_verb", return_value=response):
            _, contents = pipeline._query_once(None, "query")
        return pipeline.precision_at_k(contents, "topic")

    def test_excess_hits_cannot_raise_precision_above_one(self):
        rows = [{"content": f"topic note {i}"} for i in range(20)]
        with self.assertRaisesRegex(ValueError, "requested limit"):
            self.score({"results": rows})

    def test_eleventh_hit_cannot_rescue_first_ten_below_floor(self):
        rows = ([{"content": f"topic note {i}"} for i in range(6)]
                + [{"content": f"other note {i}"} for i in range(4)]
                + [{"content": "topic note 11"}])
        with self.assertRaisesRegex(ValueError, "requested limit"):
            self.score({"results": rows})

    def test_repeated_hit_cannot_count_ten_times(self):
        with self.assertRaisesRegex(ValueError, "duplicate recall content"):
            self.score({"results": [{"content": "topic note 1"}] * 10})

    def test_missing_content_and_conflicting_list_fields_fail(self):
        with self.assertRaisesRegex(ValueError, "string content"):
            self.score({"results": [{"id": "missing-content"}]})
        with self.assertRaisesRegex(ValueError, "exactly one"):
            self.score({"results": [], "items": [{"content": f"topic {i}"} for i in range(7)]})

    def test_distinct_short_and_empty_controls_keep_their_scores(self):
        rows = ([{"content": f"topic note {i}"} for i in range(7)]
                + [{"content": f"other note {i}"} for i in range(3)])
        self.assertEqual(self.score({"results": rows}), 0.7)
        self.assertEqual(self.score([{"content": "topic note 1"}]), 0.1)
        self.assertEqual(self.score({"results": []}), 0.0)


if __name__ == "__main__":
    unittest.main()
