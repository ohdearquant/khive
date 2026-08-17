#!/usr/bin/env python3

import hashlib
import json
import unittest


EXPECTED_CANONICAL = (
    b'{"checkpoint_sha256":"1111111111111111111111111111111111111111111111111111111111111111",'
    b'"dimensions":4,'
    b'"inference":{"provider":"lattice-embed","version":"0.9.0"},'
    b'"model_name":"qwen3.5-vlm-pooled-visual",'
    b'"model_revision":"weights-r1",'
    b'"normalization":"l2",'
    b'"pooling":"mean_visual_tokens",'
    b'"preprocessing":{"alignment":32,"matte_rgb":[128,128,128],"max_side":448,'
    b'"resample":"lanczos3","revision":"moodboard-qwen35-srgb-pad32-max448-v1"},'
    b'"prompt":{"revision":"moodboard-style-retrieval-v1",'
    b'"sha256":"2222222222222222222222222222222222222222222222222222222222222222"},'
    b'"schema_version":"moodboard.visual-descriptor.v1"}'
)
EXPECTED_FINGERPRINT = (
    "b57fb3cf43da387cde12425e6d7d442af269ba37ecabfbe4c975cb80abdf56e5"
)
EXPECTED_SPACE_KEY = f"moodboard_{EXPECTED_FINGERPRINT}_4"


class MoodboardDescriptorGoldenTest(unittest.TestCase):
    def test_python_canonicalization_matches_rust_golden(self) -> None:
        core = {
            "schema_version": "moodboard.visual-descriptor.v1",
            "model_name": "qwen3.5-vlm-pooled-visual",
            "model_revision": "weights-r1",
            "checkpoint_sha256": "1" * 64,
            "inference": {"provider": "lattice-embed", "version": "0.9.0"},
            "preprocessing": {
                "revision": "moodboard-qwen35-srgb-pad32-max448-v1",
                "max_side": 448,
                "alignment": 32,
                "matte_rgb": [128, 128, 128],
                "resample": "lanczos3",
            },
            "prompt": {
                "revision": "moodboard-style-retrieval-v1",
                "sha256": "2" * 64,
            },
            "pooling": "mean_visual_tokens",
            "dimensions": 4,
            "normalization": "l2",
        }
        canonical = json.dumps(
            core,
            ensure_ascii=False,
            separators=(",", ":"),
            sort_keys=True,
        ).encode("utf-8")

        self.assertEqual(canonical, EXPECTED_CANONICAL)
        self.assertEqual(hashlib.sha256(canonical).hexdigest(), EXPECTED_FINGERPRINT)
        self.assertEqual(
            f"moodboard_{hashlib.sha256(canonical).hexdigest()}_{core['dimensions']}",
            EXPECTED_SPACE_KEY,
        )


if __name__ == "__main__":
    unittest.main()
