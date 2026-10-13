"""The report hook in conftest.py attaches scratch daemon stderr to failures only."""

from __future__ import annotations

from types import SimpleNamespace

import pytest
from conftest import pytest_runtest_makereport


def _run_hook(item, report) -> list[tuple[str, str]]:
    hook = pytest_runtest_makereport(item, None)
    next(hook)
    with pytest.raises(StopIteration):
        hook.send(SimpleNamespace(get_result=lambda: report))
    return report.sections


def _report(outcome: str) -> SimpleNamespace:
    return SimpleNamespace(
        passed=outcome == "passed",
        skipped=outcome == "skipped",
        failed=outcome == "failed",
        sections=[],
    )


def test_failed_report_carries_every_scratch_daemon_stderr(tmp_path):
    first, second = tmp_path / "a", tmp_path / "b"
    first.mkdir()
    second.mkdir()
    (first / "daemon-1.stderr").write_text("WARN audit append failed error=first-start\n")
    (first / "daemon-2.stderr").write_text("WARN audit append failed error=restart\n")
    (second / "daemon-1.stderr").write_text("WARN audit append failed error=second-root\n")
    (first / "client.stderr").write_text("not a daemon log\n")
    item = SimpleNamespace(scratch_daemon_roots=[first, second])

    sections = _run_hook(item, _report("failed"))

    assert [title for title, _ in sections] == [
        f"scratch daemon stderr: {first / 'daemon-1.stderr'}",
        f"scratch daemon stderr: {first / 'daemon-2.stderr'}",
        f"scratch daemon stderr: {second / 'daemon-1.stderr'}",
    ]
    assert [body for _, body in sections] == [
        "WARN audit append failed error=first-start\n",
        "WARN audit append failed error=restart\n",
        "WARN audit append failed error=second-root\n",
    ]


@pytest.mark.parametrize("outcome", ["passed", "skipped"])
def test_passing_or_skipped_report_carries_no_daemon_stderr(tmp_path, outcome):
    (tmp_path / "daemon-1.stderr").write_text("WARN should not be attached\n")
    item = SimpleNamespace(scratch_daemon_roots=[tmp_path])

    assert _run_hook(item, _report(outcome)) == []


def test_test_without_scratch_daemons_is_untouched():
    assert _run_hook(SimpleNamespace(), _report("failed")) == []
