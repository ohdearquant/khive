"""Live integration test against a real khive-cloud deployment.

Skipped unless both KHIVE_CLOUD_API_KEY and KHIVE_CLOUD_URL are set. Asserts
envelope/result SHAPE only — `whoami`'s field set is changing server-side, so
no field is pinned.
"""

from __future__ import annotations

import os

import pytest

httpx = pytest.importorskip("httpx")
mcp = pytest.importorskip("mcp")

from khive import HttpTransport, Khive
from khive.mcp import mcp_list_tools

pytestmark = pytest.mark.skipif(
    not (os.environ.get("KHIVE_CLOUD_API_KEY") and os.environ.get("KHIVE_CLOUD_URL")),
    reason="requires KHIVE_CLOUD_API_KEY and KHIVE_CLOUD_URL to be set",
)


def _live_db() -> Khive:
    return Khive(
        transport=HttpTransport(os.environ["KHIVE_CLOUD_URL"], os.environ["KHIVE_CLOUD_API_KEY"])
    )


def test_live_whoami():
    result = _live_db().whoami()
    assert isinstance(result, dict)


def test_live_search_entity():
    results = _live_db().search("test", kind="entity")
    assert isinstance(results, list)


def test_live_mcp_tools_list():
    names = mcp_list_tools(os.environ["KHIVE_CLOUD_URL"], os.environ["KHIVE_CLOUD_API_KEY"])
    assert "request" in names


def test_live_raw_accepts_dsl_text():
    results = _live_db().raw("whoami()")
    assert len(results) == 1 and results[0].ok
