"""HttpTransport against a fake khive-cloud REST endpoint (offline, no daemon)."""

from __future__ import annotations

import asyncio
import json
import threading
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

httpx = pytest.importorskip("httpx")

from khive import (
    AuthError,
    BatchError,
    HttpTransport,
    Khive,
    RateLimited,
    ServerError,
    TransportError,
    op,
)
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


def test_raw_accepts_dsl_text(db: Khive):
    results = db.raw("whoami()")
    assert [(r.ok, r.tool) for r in results] == [(True, "whoami")]
    assert results[0].result == {"namespace": "local"}


def test_raw_accepts_dsl_batch_text(db: Khive):
    results = db.raw("[whoami(), stats()]")
    assert [(r.ok, r.tool) for r in results] == [(True, "whoami"), (True, "stats")]


def test_raw_accepts_dsl_chain_text(db: Khive):
    results = db.raw("[whoami() | stats()]")
    assert [(r.ok, r.tool) for r in results] == [(True, "whoami"), (True, "stats")]


def test_raw_accepts_dsl_text_beside_op_dicts(db: Khive):
    results = db.raw(["whoami()", op("nope")])
    assert [(r.ok, r.tool) for r in results] == [(True, "whoami"), (False, "nope")]


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


def test_connection_error_raises_transport_error():
    handle = Khive(transport=HttpTransport("http://127.0.0.1:1", "key", timeout=2.0))
    with pytest.raises(TransportError):
        handle.stats()


def test_connection_error_on_metrics_raises_transport_error():
    handle = Khive(transport=HttpTransport("http://127.0.0.1:1", "key", timeout=2.0))
    with pytest.raises(TransportError):
        handle.metrics()


class _MalformedHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    body: bytes = b""

    def log_message(self, format: str, *args: object) -> None:
        pass

    def do_GET(self) -> None:
        self._reply()

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        if length:
            self.rfile.read(length)
        self._reply()

    def _reply(self) -> None:
        body = type(self).body
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@contextmanager
def _malformed_server(body: bytes):
    handler = type("_Handler", (_MalformedHandler,), {"body": body})
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = server.server_address[1]
        yield f"http://127.0.0.1:{port}"
    finally:
        server.shutdown()
        thread.join(timeout=5)


def test_non_json_2xx_body_raises_transport_error():
    with _malformed_server(b"not json at all") as url:
        handle = Khive(transport=HttpTransport(url, "key"))
        with pytest.raises(TransportError):
            handle.stats()


def test_json_2xx_body_missing_results_raises_transport_error():
    with _malformed_server(json.dumps({"ok": True}).encode()) as url:
        handle = Khive(transport=HttpTransport(url, "key"))
        with pytest.raises(TransportError):
            handle.stats()


def test_json_2xx_body_that_is_a_list_raises_transport_error():
    with _malformed_server(json.dumps(["not", "an", "envelope"]).encode()) as url:
        handle = Khive(transport=HttpTransport(url, "key"))
        with pytest.raises(TransportError):
            handle.stats()


def test_non_dict_result_entry_raises_transport_error():
    body = json.dumps({"results": [42]}).encode()
    with _malformed_server(body) as url:
        handle = Khive(transport=HttpTransport(url, "key"))
        with pytest.raises(TransportError):
            handle.stats()


def test_result_entry_missing_required_fields_raises_transport_error():
    body = json.dumps({"results": [{"result": {"x": 1}}]}).encode()
    with _malformed_server(body) as url:
        handle = Khive(transport=HttpTransport(url, "key"))
        with pytest.raises(TransportError):
            handle.stats()


def test_send_dsl_round_trips(rest_server, api_key):
    transport = HttpTransport(rest_server.url, api_key)
    envelope = transport.send_dsl("stats()", timeout=5.0)
    assert envelope["ok"] is True
    assert envelope["result"]["results"][0]["result"] == {
        "entities": 1,
        "edges": 0,
        "notes": 0,
    }


def test_send_dsl_with_delimiters_sent_verbatim(rest_server, api_key):
    transport = HttpTransport(rest_server.url, api_key)
    dsl = 'search(query="a, b (c) [d]")'
    envelope = transport.send_dsl(dsl, timeout=5.0)
    assert envelope["result"]["results"][0] == {
        "ok": True,
        "tool": "search",
        "result": {"items": []},
    }


def test_chained_abort_returns_minimal_entry(db: Khive):
    results = db.raw("[nope() | stats()]")
    assert [(r.ok, r.tool) for r in results] == [(False, "nope"), (False, "")]
    assert results[1].result is None
    assert results[1].error is None
    assert results[1].model_extra.get("aborted") is True


def test_chained_abort_same_object_sync_and_async(rest_server, api_key):
    """A chain whose second op aborts produces the identical caller-visible
    aborted entry whether it is dispatched through the sync or the async
    HTTP transport — both funnel through the same `_validate_envelope_results`
    normalization."""
    dsl = "[nope() | stats()]"

    async def _async_side():
        from khive import AsyncHttpTransport

        transport = AsyncHttpTransport(rest_server.url, api_key)
        try:
            response = await transport.round_trip({"ops": json.dumps(dsl)}, timeout=5.0)
        finally:
            await transport.aclose()
        return response["result"]["results"][1]

    sync_transport = HttpTransport(rest_server.url, api_key)
    sync_entry = sync_transport.send_dsl(dsl, timeout=5.0)["result"]["results"][1]
    async_entry = asyncio.run(_async_side())

    assert sync_entry == async_entry == {"ok": False, "aborted": True, "tool": ""}


def test_malformed_error_object_bad_code_type_raises_transport_error():
    body = json.dumps(
        {"results": [{"ok": False, "tool": "x", "error": {"code": [], "message": {}}}]}
    ).encode()
    with _malformed_server(body) as url:
        handle = Khive(transport=HttpTransport(url, "key"))
        with pytest.raises(TransportError):
            handle.raw([op("x")])


def test_malformed_error_object_missing_message_raises_transport_error():
    body = json.dumps({"results": [{"ok": False, "tool": "x", "error": {"code": "boom"}}]}).encode()
    with _malformed_server(body) as url:
        handle = Khive(transport=HttpTransport(url, "key"))
        with pytest.raises(TransportError):
            handle.raw([op("x")])


def test_non_loopback_http_base_url_refused():
    with pytest.raises(ValueError, match="http.*example.test"):
        HttpTransport("http://example.test", "key")


def test_loopback_http_base_url_admitted(rest_server, api_key):
    handle = Khive(transport=HttpTransport(rest_server.url, api_key))
    assert handle.stats() == {"entities": 1, "edges": 0, "notes": 0}


def test_allow_insecure_admits_non_loopback_http_base_url():
    # No connection is made; this only proves construction is not refused.
    transport = HttpTransport("http://example.test", "key", allow_insecure=True)
    assert transport._base_url == "http://example.test"
