"""HttpTransport against a fake khive-cloud REST endpoint (offline, no daemon)."""

from __future__ import annotations

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
