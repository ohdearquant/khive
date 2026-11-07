"""Pure calibration checks using synthetic hits and scripted recall responses."""

import unittest

import bank


IDS = (
    "00000000-0000-0000-0000-000000000001",
    "00000000-0000-0000-0000-000000000002",
    "00000000-0000-0000-0000-000000000003",
)
MISSING = object()
BAD_SCORES = (
    ("missing", MISSING),
    ("null", None),
    ("text", "0.25"),
    ("false", False),
    ("true", True),
    ("nan", float("nan")),
    ("positive_infinity", float("inf")),
    ("negative_infinity", float("-inf")),
)


def hit(ident, score=MISSING):
    result = {"id": ident}
    if score is not MISSING:
        result["rank_score"] = score
    return result


class ScriptedBridge:
    def __init__(self, responses):
        self.responses = list(responses)
        self.calls = []

    def one(self, operation):
        self.calls.append(operation)
        if not self.responses:
            raise AssertionError("unexpected extra recall")
        return self.responses.pop(0)


class CalibrationScoreTests(unittest.TestCase):
    def test_missing_rank_score_is_refused(self):
        valid = [hit(IDS[0], 0.25)]
        self.assertEqual(bank.compare(valid, valid), {
            "overlap@10": 1.0,
            "same_order": True,
            "max_abs_rank_score_diff": 0.0,
        })
        for side in ("left", "right"):
            with self.subTest(side=side):
                x, y = [hit(IDS[0])], valid
                if side == "right":
                    x, y = y, x
                with self.assertRaises(bank.BankError, msg="calibration_missing_rank_score_is_refused"):
                    bank.compare(x, y)

    def test_invalid_scores_are_refused_on_both_sides_even_without_shared_ids(self):
        for label, score in BAD_SCORES:
            for side in ("left", "right"):
                for shared in (False, True):
                    with self.subTest(score=label, side=side, shared=shared):
                        x = [hit(IDS[0], 0.25), hit(IDS[1], score)]
                        y = [hit(IDS[0], 0.25), hit(IDS[1] if shared else IDS[2], 0.5)]
                        if side == "right":
                            x, y = y, x
                        with self.assertRaisesRegex(bank.BankError, "rank_score"):
                            bank.compare(x, y)

    def test_invalid_duplicate_hit_is_not_hidden_by_a_later_valid_score(self):
        x = [hit(IDS[0]), hit(IDS[0], 0.25)]
        with self.assertRaisesRegex(bank.BankError, "rank_score"):
            bank.compare(x, [hit(IDS[0], 0.25)])

    def test_valid_id_preference_order_and_duplicate_behavior_are_unchanged(self):
        first = {"id": "short-id", "full_id": IDS[0], "rank_score": 0.25}
        x = [first, hit(IDS[1], 1), hit(IDS[0], 0.5)]
        y = [hit(IDS[0], 0.5), hit(IDS[1], 1)]
        self.assertEqual(bank.compare(x, y), {
            "overlap@10": 1.0,
            "same_order": True,
            "max_abs_rank_score_diff": 0.0,
        })

    def test_finite_scores_have_no_new_range_or_float_coercion_rule(self):
        large = 10 ** 400
        self.assertEqual(bank.compare([hit(IDS[0], large)], [hit(IDS[0], large + 1)]), {
            "overlap@10": 1.0,
            "same_order": True,
            "max_abs_rank_score_diff": 1,
        })
        self.assertEqual(bank.compare([hit(IDS[0], -2)], [hit(IDS[0], 1.5)]), {
            "overlap@10": 1.0,
            "same_order": True,
            "max_abs_rank_score_diff": 3.5,
        })

    def test_out_of_float_range_difference_is_refused(self):
        with self.assertRaisesRegex(bank.BankError, "out of float range"):
            bank.compare([hit(IDS[0], 10 ** 400)], [hit(IDS[0], 0.5)])

    def test_refusal_names_the_rejected_value(self):
        for label, score, shown in (("null", None, "None"), ("text", "0.25", "'0.25'"), ("true", True, "True")):
            with self.subTest(score=label):
                with self.assertRaisesRegex(bank.BankError, "got " + shown.replace("(", "\\(")):
                    bank.compare([hit(IDS[0], score)], [hit(IDS[0], 0.25)])

    def test_empty_and_valid_no_overlap_comparisons_keep_zero_difference(self):
        pairs = (
            ([], []),
            ([], [hit(IDS[0], 0.25)]),
            ([hit(IDS[0], 0.25)], []),
            ([hit(IDS[0], 0.25)], [hit(IDS[1], 0.5)]),
        )
        for x, y in pairs:
            with self.subTest(left=len(x), right=len(y)):
                self.assertEqual(bank.compare(x, y), {
                    "overlap@10": 0.0,
                    "same_order": not x and not y,
                    "max_abs_rank_score_diff": 0.0,
                })

    def test_calibrate_case_keeps_numeric_aba_differences_and_profile_calls(self):
        live = [hit(IDS[0], 1), hit(IDS[1], 0.25)]
        replayed = [hit(IDS[1], 0.75), hit(IDS[0], 0.75)]
        second_live = [hit(IDS[0], 0.875), hit(IDS[1], 0.375)]
        br = ScriptedBridge([live, {"results": replayed}, second_live])
        case = {"case_id": "synthetic-numeric", "query": "small synthetic query",
                "tags": ["namespace:synthetic-scope"]}
        result = bank.calibrate_case(br, case, "calibration-live-profile", "calibration-replay-profile")
        self.assertEqual(result, {
            "query_id": "synthetic-numeric",
            "overlap@10": 1.0,
            "same_order": False,
            "max_abs_rank_score_diff": 0.5,
            "aa": {"overlap@10": 1.0, "same_order": True, "max_abs_rank_score_diff": 0.125},
            "replayed_vs_second_live": {
                "overlap@10": 1.0, "same_order": False, "max_abs_rank_score_diff": 0.375,
            },
        })
        self.assertEqual(len(br.calls), 3)
        self.assertEqual(br.calls[0], br.calls[2])
        self.assertIn("calibration-live-profile", br.calls[0])
        self.assertNotIn("calibration-replay-profile", br.calls[0])
        self.assertIn("calibration-replay-profile", br.calls[1])
        self.assertNotIn("calibration-live-profile", br.calls[1])
        self.assertEqual(br.responses, [])

    def test_calibrate_case_refuses_each_bad_score_in_each_response(self):
        case = {"case_id": "synthetic-invalid", "query": "small synthetic query"}
        for label, score in BAD_SCORES:
            for phase in range(3):
                with self.subTest(score=label, phase=phase):
                    responses = [[hit(IDS[0], 0.25), hit(IDS[1], 0.5)] for _ in range(3)]
                    responses[phase][1] = hit(IDS[1], score)
                    br = ScriptedBridge(responses)
                    with self.assertRaisesRegex(bank.BankError, "rank_score"):
                        bank.calibrate_case(br, case, "live", "replayed")

    def test_calibrate_case_keeps_empty_and_no_overlap_results(self):
        case = {"case_id": "synthetic-empty", "query": "small synthetic query"}
        for responses in (
            [[], [], []],
            [[hit(IDS[0], 0.25)], [hit(IDS[1], 0.5)], [hit(IDS[2], 0.75)]],
        ):
            with self.subTest(empty=not responses[0]):
                result = bank.calibrate_case(ScriptedBridge(responses), case, "live", "replayed")
                for comparison in (result, result["aa"], result["replayed_vs_second_live"]):
                    self.assertEqual(comparison["overlap@10"], 0.0)
                    self.assertEqual(comparison["max_abs_rank_score_diff"], 0.0)
