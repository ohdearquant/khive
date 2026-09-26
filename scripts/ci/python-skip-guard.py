"""Accept only ENV_GATED_ALLOWLIST skips in a complete pytest -rs report.

Usage: python3 scripts/ci/python-skip-guard.py REPORT
The named allowlist checks both the file and the environment-gate reason.
"""

import re
import sys
from pathlib import Path

ENV_GATED_ALLOWLIST = [
    (
        "python/tests/test_cloud_live.py",
        "requires KHIVE_CLOUD_API_KEY and KHIVE_CLOUD_URL to be set",
    ),
    (
        "python/tests/test_comm_idempotency.py",
        "set KHIVE_TEST_LEGACY_COMM=1 and KKERNEL to a server predating keyed comm",
    ),
]


def check(report: str) -> None:
    skips = 0
    summaries = []
    for line in report.splitlines():
        if line.startswith("SKIPPED"):
            match = re.fullmatch(r"SKIPPED \[(\d+)\] (.+?):\d+: (.*)", line)
            if not match or (match[2], match[3]) not in ENV_GATED_ALLOWLIST:
                raise ValueError(f"disallowed skip: {line}")
            skips += int(match[1])
        if re.search(r"\b\d+ passed\b.*\bin \d+(?:\.\d+)?s\b", line):
            summaries.append(line)
    if len(summaries) != 1:
        raise ValueError("expected exactly one pytest result summary")
    summary = summaries[0]
    if re.search(r"\b\d+ (?:failed|errors?)\b", summary):
        raise ValueError("pytest summary contains failures or errors")
    count = re.search(r"\b(\d+) skipped\b", summary)
    if skips != (int(count[1]) if count else 0):
        raise ValueError("summary skip count does not match the detailed -rs report")


if __name__ == "__main__":
    try:
        if len(sys.argv) != 2:
            raise ValueError("usage: python-skip-guard.py REPORT")
        check(Path(sys.argv[1]).read_text())
    except (ValueError, OSError) as exc:
        print(f"Python skip guard: {exc}", file=sys.stderr)
        raise SystemExit(1)
