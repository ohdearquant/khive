"""HttpTransport against a fake khive-cloud REST endpoint (offline, no daemon)."""

from __future__ import annotations

import pytest

httpx = pytest.importorskip("httpx")

from khive import AuthError, BatchError, HttpTransport, Khive, RateLimited, ServerError, op
from khive.errors import http_op_error_code


@pytest.fixture()
def db(rest_server, api_key) -> Khive:
    return Khive(transport=HttpTransport(rest_server.url, api_key))


def test_handshake_never_posts(rest_server, api_key):
    # An empty ops POST to /v1/request 400s in the fake server, so a
    # successful handshake proves it went to GET /health instead.
    handle = Khive(transport=HttpTransport(rest_server.url, api_key))
    served = handle.session.handshake()
    assert served == f"http:{rest_server.url}"


def test_stats_round_trip(db: Khive):
    assert db.stats() == {"entities": 1, "edges": 0, "notes": 0}


def test_metrics_returns_health_body(db: Khive):
    assert db.metrics() == {"status": "ok"}


def test_raw_returns_per_op_entries(db: Khive):
    results = db.raw([op("stats"), op("nope")])
    assert len(results) == 2
    assert results[0].ok and results[0].result == {"entities": 1, "edges": 0, "notes": 0}
    assert not results[1].ok
    assert "verb_not_found" in (results[1].error or "")
    assert http_op_error_code(results[1].error) == "verb_not_found"


def test_batch_raises_batch_error_on_embedded_failure(db: Khive):
    with pytest.raises(BatchError) as excinfo:
        db.batch([op("stats"), op("nope")])
    err = excinfo.value
    assert len(err.results) == 2 and len(err.failures) == 1
    assert err.failures[0][0] == 1, "the failing op must be identified by index"


def test_search_round_trips(db: Khive):
    assert db.search("anything", kind="entity") == []


def test_wrong_key_raises_auth_error(rest_server):
    handle = Khive(transport=HttpTransport(rest_server.url, "wrong-key"))
    with pytest.raises(AuthError) as exc_info:
        handle.stats()
    assert exc_info.value.status == 401


def test_rate_limited_raises_rate_limited(db: Khive):
    with pytest.raises(RateLimited) as exc_info:
        db.raw([op("rate_limited")])
    assert exc_info.value.status == 429


def test_server_error_raises_server_error(db: Khive):
    with pytest.raises(ServerError) as exc_info:
        db.raw([op("boom")])
    assert exc_info.value.status == 500


def test_api_key_never_in_repr_or_error_text(rest_server, api_key):
    transport = HttpTransport(rest_server.url, api_key)
    assert api_key not in repr(transport)
    try:
        Khive(transport=HttpTransport(rest_server.url, "wrong-key")).stats()
        pytest.fail("expected AuthError")
    except AuthError as exc:
        assert api_key not in str(exc)


def test_base_url_trailing_slash_normalised(rest_server, api_key):
    handle = Khive(transport=HttpTransport(rest_server.url + "/", api_key))
    assert handle.stats() == {"entities": 1, "edges": 0, "notes": 0}
