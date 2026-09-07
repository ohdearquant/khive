"""Source-shaped responses exercised through the native Session and facade."""

from __future__ import annotations

import copy
import json

import pytest

from khive import BatchError, Khive, OperationError, Transport, op
from khive.envelope import _validate_envelope_results
from khive.errors import TransportError

NOTE_ID = "00000000-0000-4000-8000-000000000001"
NEXT_ID = "00000000-0000-4000-8000-000000000002"


class ResponseTransport(Transport):
    def __init__(self, results):
        self.results = results

    def round_trip(self, frame, timeout):
        response = {"ok": True, "served_config_id": "test-config"}
        if not frame.get("metrics_only"):
            response["result"] = json.dumps({"results": self.results})
        return response


def client_for(results):
    return Khive(transport=ResponseTransport(results), actor_id="test-client")


ROWS = {
    "entities": {"id": NOTE_ID, "kind": "concept", "name": "sample"},
    "notes": {"id": NOTE_ID, "kind": "observation", "content": "sample"},
    "edges": {
        "id": NOTE_ID,
        "relation": "extends",
        "source_id": NOTE_ID,
        "target_id": NEXT_ID,
        "weight": 1.0,
    },
}


def list_page(db, substrate, **kwargs):
    if substrate == "edges":
        return db.graph.edges(**kwargs)
    return getattr(db, substrate).list(**kwargs)


@pytest.mark.parametrize("substrate", ROWS)
@pytest.mark.parametrize("cursor", [False, True])
def test_native_pages_keep_records_and_metadata(substrate, cursor):
    cap = {"entities": 500, "notes": 200, "edges": 1000}[substrate]
    payload = {
        substrate if cursor else "items": [ROWS[substrate]],
        "requested_limit": 2000,
        "effective_limit": cap,
        "limit_clamped": True,
    }
    if cursor:
        payload["next_after"] = NEXT_ID
    db = client_for([{"ok": True, "tool": "list", "result": payload}])
    page = list_page(db, substrate, limit=2000, **({"after": ""} if cursor else {}))
    assert [item.id for item in page.items] == [NOTE_ID]
    assert page.requested_limit == 2000
    assert page.effective_limit == cap
    assert page.limit_clamped is True
    assert page.next_after == (NEXT_ID if cursor else None)
    assert page.next_offset is None
    assert page.total is None


@pytest.mark.parametrize("cursor", [False, True])
def test_empty_filtered_page_preserves_incomplete_scan(cursor):
    payload = {
        "notes" if cursor else "items": [],
        "scan_incomplete": True,
        "requested_limit": 1,
        "effective_limit": 1,
        "limit_clamped": False,
    }
    if cursor:
        payload["next_after"] = NEXT_ID
    db = client_for([{"ok": True, "tool": "list", "result": payload}])
    page = db.notes.list(limit=1, tags=["cursor-test"], **({"after": ""} if cursor else {}))
    assert page.items == []
    assert page.scan_incomplete is True
    assert page.next_after == (NEXT_ID if cursor else None)


@pytest.mark.parametrize("container", ["items", "results"])
def test_explicit_offset_metadata_is_preserved(container):
    payload = {container: [ROWS["notes"]], "total": 10, "next_offset": 7}
    page = client_for([{"ok": True, "tool": "list", "result": payload}]).notes.list()
    assert page.total == 10 and page.next_offset == 7
    assert [item.id for item in page.items] == [NOTE_ID]


ERRORS = [
    "entity not found",
    {
        "kind": "storage",
        "code": "writer_task_terminated",
        "stage": "writer_task_terminated",
        "message": "writer task terminated",
        "retryable": False,
        "request_state": "side_effects_unknown",
        "task_terminated": True,
    },
    {
        "kind": "unavailable",
        "code": "writer_queue_saturated",
        "stage": "writer_queue_saturated",
        "message": "write queue full",
        "retryable": True,
        "timeout_ms": 100,
        "capability": None,
        "operation": None,
        "scope": "writer_admission",
        "retry_after_ms": 100,
    },
    {"kind": "not_found", "message": "entity not found", "code": None, "details": None},
    {
        "kind": "conflict",
        "message": "conflict",
        "code": "version_conflict",
        "details": {"current_version": "2"},
        "request_state": "unknown",
        "future_metadata": {"attempt": 2},
    },
]


def error_payload(value):
    return value.model_dump(exclude_unset=True) if hasattr(value, "model_dump") else value


@pytest.mark.parametrize("error", ERRORS)
@pytest.mark.parametrize("validate_envelope", [False, True])
def test_error_objects_survive_raw_and_partial_batch(error, validate_envelope):
    entries = [
        {"ok": True, "tool": "stats", "result": {"count": 1}},
        {"ok": False, "tool": "get", "error": copy.deepcopy(error)},
    ]
    if validate_envelope:
        entries = _validate_envelope_results({"results": entries}, "fixture")["results"]
    db = client_for(entries)
    ops = [op("stats"), op("get", id=NOTE_ID)]
    results = db.raw(ops)
    assert results[0].result == {"count": 1}
    assert error_payload(results[1].error) == error
    with pytest.raises(BatchError) as raised:
        db.batch(ops)
    assert raised.value.results == entries
    index, tool, failure = raised.value.failures[0]
    assert (index, tool) == (1, "get")
    assert error_payload(failure) == error
    assert (error if isinstance(error, str) else error["message"]) in str(raised.value)


def test_single_operation_preserves_unknown_write_error():
    error = ERRORS[1]
    db = client_for([{"ok": False, "tool": "get", "error": error}])
    with pytest.raises(OperationError) as raised:
        db.get(NOTE_ID)
    assert error_payload(raised.value.error) == error
    assert raised.value.error.request_state == "side_effects_unknown"


@pytest.mark.parametrize(
    "aborted",
    [
        {"ok": False, "aborted": True},
        {"ok": False, "tool": "get", "aborted": True},
        {"ok": False, "tool": "get", "aborted": True, "message": "prior operation failed"},
    ],
)
def test_aborted_entries_remain_unexecuted_in_raw_and_batch(aborted):
    entries = [{"ok": False, "tool": "get", "error": "not found"}, aborted]
    db = client_for(entries)
    ops = [op("get", id=NOTE_ID), op("get", id=NEXT_ID)]
    results = db.raw(ops)
    assert results[1].aborted is True
    assert results[1].error is None
    assert results[1].tool == aborted.get("tool", "")
    with pytest.raises(BatchError) as raised:
        db.batch(ops)
    assert [failure[0] for failure in raised.value.failures] == [0, 1]
    assert raised.value.results[1]["aborted"] is True


@pytest.mark.parametrize(
    "patch",
    [{"message": 1}, {"code": 1}, {"retryable": "false"}, {"request_state": False}],
)
def test_malformed_error_fields_refused_by_session(patch):
    error = {**ERRORS[1], **patch}
    db = client_for([{"ok": False, "tool": "get", "error": error}])
    with pytest.raises(TransportError, match="malformed"):
        db.raw([op("get", id=NOTE_ID)])


def test_live_three_note_cursor_walk(scratch_daemon):
    db = Khive(socket_path=str(scratch_daemon["socket"]), actor_id="test-client")
    # A tag identifies just this fixture in the shared scratch daemon.
    created = [
        db.notes.create(subject="cursor", content=f"cursor record {i}", tags=["cursor-test"])
        for i in range(3)
    ]
    cursor = ""
    seen = []
    for index in range(3):
        page = db.notes.list(limit=1, after=cursor, tags=["cursor-test"])
        assert len(page.items) == 1
        assert page.effective_limit == 1
        assert page.scan_incomplete is not True
        seen.append(page.items[0].id)
        if index < 2:
            assert page.next_after is not None
            cursor = page.next_after
        else:
            assert page.next_after is None
    assert len(set(seen)) == 3
    assert set(seen) == {note.id for note in created}


def test_live_missing_id_keeps_string_error_and_index(scratch_daemon):
    db = Khive(socket_path=str(scratch_daemon["socket"]), actor_id="test-client")
    with pytest.raises(BatchError) as raised:
        db.batch([op("stats"), op("get", id="00000000-0000-0000-0000-000000000000")])
    index, tool, error = raised.value.failures[0]
    assert (index, tool) == (1, "get")
    assert isinstance(error, str) and error
