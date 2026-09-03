"""Direct coverage for the transport-shared envelope normalization functions
in `khive.envelope` — decoding, envelope-shape rejection, minimal aborted
entries, per-entry `OpResult` validation, and per-op error flattening."""

from __future__ import annotations

import re

import pytest

from khive.envelope import (
    _decode_json_text,
    _envelope_from_payload,
    _is_minimal_aborted_entry,
    _stringify_op_errors,
    _validate_envelope_results,
)
from khive.errors import TransportError

# Sentinel URL used in every negative assertion below, so a match on it
# proves the raised error actually names the URL this module was given —
# not just any string that happens to contain "url" verbatim.
_SENTINEL_URL = "http://sentinel.invalid/x"


def test_decode_json_text_valid():
    assert _decode_json_text('{"a": 1}', "http://x") == {"a": 1}


def test_decode_json_text_malformed_raises_naming_url():
    with pytest.raises(TransportError, match=re.escape(_SENTINEL_URL)):
        _decode_json_text("{not json", _SENTINEL_URL)


def test_envelope_from_payload_accepts_dict_with_list_results():
    payload = {"results": [{"ok": True, "tool": "whoami"}]}
    assert _envelope_from_payload(payload, "url") is payload


@pytest.mark.parametrize(
    "payload",
    [
        [1, 2, 3],
        "a string",
        42,
        None,
        {"no_results": True},
        {"results": "not-a-list"},
        {"results": {"a": 1}},
    ],
)
def test_envelope_from_payload_rejects_non_envelope_shapes(payload):
    with pytest.raises(TransportError, match=re.escape(_SENTINEL_URL)):
        _envelope_from_payload(payload, _SENTINEL_URL)


def test_is_minimal_aborted_entry_true():
    assert _is_minimal_aborted_entry({"ok": False, "aborted": True}) is True


def test_is_minimal_aborted_entry_false_when_tool_present():
    assert _is_minimal_aborted_entry({"ok": False, "aborted": True, "tool": "x"}) is False


def test_is_minimal_aborted_entry_false_when_ok_not_false():
    assert _is_minimal_aborted_entry({"ok": True, "aborted": True}) is False


def test_is_minimal_aborted_entry_false_when_aborted_missing():
    assert _is_minimal_aborted_entry({"ok": False}) is False


def test_is_minimal_aborted_entry_false_for_non_dict():
    assert _is_minimal_aborted_entry("not-a-dict") is False
    assert _is_minimal_aborted_entry(None) is False
    assert _is_minimal_aborted_entry([1, 2]) is False


def test_validate_envelope_results_accepts_valid_op_results():
    envelope = {"results": [{"ok": True, "tool": "whoami", "result": {"id": 1}, "error": None}]}
    out = _validate_envelope_results(envelope, "url")
    assert out is envelope


def test_validate_envelope_results_admits_minimal_aborted_entry_with_empty_tool():
    envelope = {"results": [{"ok": False, "aborted": True}]}
    out = _validate_envelope_results(envelope, "url")
    assert out["results"][0] == {"ok": False, "aborted": True, "tool": ""}


def test_validate_envelope_results_rejects_non_dict_entry():
    with pytest.raises(TransportError, match=f"{re.escape(_SENTINEL_URL)}.*index 0"):
        _validate_envelope_results({"results": [42]}, _SENTINEL_URL)


def test_validate_envelope_results_rejects_entry_missing_ok():
    with pytest.raises(TransportError, match=f"{re.escape(_SENTINEL_URL)}.*index 0"):
        _validate_envelope_results({"results": [{"tool": "whoami"}]}, _SENTINEL_URL)


def test_validate_envelope_results_rejects_entry_missing_tool():
    with pytest.raises(TransportError, match=f"{re.escape(_SENTINEL_URL)}.*index 0"):
        _validate_envelope_results({"results": [{"ok": True}]}, _SENTINEL_URL)


def test_stringify_op_errors_code_and_message():
    envelope = {"results": [{"ok": False, "tool": "v", "error": {"code": "x", "message": "m"}}]}
    out = _stringify_op_errors(envelope, "url")
    assert out["results"][0]["error"] == "x: m"


def test_stringify_op_errors_message_only():
    envelope = {"results": [{"ok": False, "tool": "v", "error": {"message": "m"}}]}
    out = _stringify_op_errors(envelope, "url")
    assert out["results"][0]["error"] == "m"


def test_stringify_op_errors_non_string_code_raises():
    envelope = {"results": [{"ok": False, "tool": "v", "error": {"code": 1, "message": "m"}}]}
    with pytest.raises(TransportError, match=f"{re.escape(_SENTINEL_URL)}.*index 0"):
        _stringify_op_errors(envelope, _SENTINEL_URL)


def test_stringify_op_errors_non_string_message_raises():
    envelope = {"results": [{"ok": False, "tool": "v", "error": {"code": "x", "message": 1}}]}
    with pytest.raises(TransportError, match=f"{re.escape(_SENTINEL_URL)}.*index 0"):
        _stringify_op_errors(envelope, _SENTINEL_URL)


def test_stringify_op_errors_missing_message_raises():
    envelope = {"results": [{"ok": False, "tool": "v", "error": {"code": "x"}}]}
    with pytest.raises(TransportError, match=f"{re.escape(_SENTINEL_URL)}.*index 0"):
        _stringify_op_errors(envelope, _SENTINEL_URL)


def test_stringify_op_errors_non_dict_envelope_returned_unchanged():
    assert _stringify_op_errors([1, 2, 3], "url") == [1, 2, 3]
    assert _stringify_op_errors(None, "url") is None


def test_stringify_op_errors_skips_non_dict_entry():
    envelope = {"results": [42, "also-not-a-dict"]}
    out = _stringify_op_errors(envelope, "url")
    assert out["results"] == [42, "also-not-a-dict"]


def test_stringify_op_errors_leaves_string_error_alone():
    envelope = {"results": [{"ok": False, "tool": "v", "error": "already a string"}]}
    out = _stringify_op_errors(envelope, "url")
    assert out["results"][0]["error"] == "already a string"


def test_chain_abort_envelope_through_both_steps():
    envelope = {
        "results": [
            {
                "ok": False,
                "tool": "nope",
                "error": {"code": "unknown_verb", "message": "no such verb"},
            },
            {"ok": False, "aborted": True},
        ]
    }
    stringified = _stringify_op_errors(envelope, "url")
    validated = _validate_envelope_results(stringified, "url")
    assert validated["results"][1] == {"ok": False, "aborted": True, "tool": ""}
    assert validated["results"][0]["error"] == "unknown_verb: no such verb"
