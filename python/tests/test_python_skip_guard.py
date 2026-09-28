"""The Python CI report must not hide missing dependencies as allowed skips."""

import subprocess
import sys
from pathlib import Path

import pytest

GUARD = Path(__file__).parents[2] / "scripts/ci/python-skip-guard.py"
LIVE = "SKIPPED [4] python/tests/test_cloud_live.py:32: requires KHIVE_CLOUD_API_KEY and KHIVE_CLOUD_URL to be set\n"
LEGACY = "SKIPPED [2] python/tests/test_comm_idempotency.py:180: set KHIVE_TEST_LEGACY_COMM=1 and KKERNEL to a server predating keyed comm\n"


def run_guard(tmp_path, report):
    path = tmp_path / "pytest.txt"
    path.write_text(report)
    return subprocess.run(
        [sys.executable, str(GUARD), str(path)], capture_output=True, text=True, check=False
    )


def test_skip_guard_accepts_allowed_report(tmp_path):
    result = run_guard(tmp_path, LIVE + LEGACY + "67 passed, 6 skipped in 25.04s\n")
    assert result.returncode == 0, result.stderr


@pytest.mark.parametrize(
    "report,expected",
    [
        (
            "SKIPPED [1] python/tests/test_http_transport.py:12: could not import 'httpx'\n4 passed, 1 skipped in 1.00s\n",
            "test_http_transport.py",
        ),
        (
            "SKIPPED [1] python/tests/test_dsl_contract.py:101: missing source\n4 passed, 1 skipped in 1.00s\n",
            "test_dsl_contract.py",
        ),
        (
            "SKIPPED [1] python/tests/test_cloud_live.py:12: could not import 'httpx'\n4 passed, 1 skipped in 1.00s\n",
            "test_cloud_live.py",
        ),
        (LIVE, "summary"),
        ("67 passed, 4 skipped in 1.00s\n", "skip count"),
    ],
)
def test_skip_guard_rejects_incomplete_or_unexpected_report(tmp_path, report, expected):
    result = run_guard(tmp_path, report)
    assert result.returncode != 0
    assert expected in result.stderr
