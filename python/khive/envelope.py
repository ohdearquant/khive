"""Normalizes a request envelope from any transport into the shape `OpResult`
validates, so every transport hands the caller the same object.

A transport decodes raw bytes into JSON, checks the result is a request
envelope shape, flattens each per-op error object to the plain string
`OpResult.error` expects, and admits the daemon's minimal aborted-chain-entry
shape before validating every entry against `OpResult` — shared here so a
malformed body, a malformed envelope, or a malformed per-op entry is the
same error class on every transport that calls into this module.
"""

from __future__ import annotations

import json
from typing import Any

from pydantic import ValidationError

from .errors import TransportError
from .models import OpResult


def _decode_json_text(text: str, url: str) -> Any:
    """Decodes raw response/tool-result text as JSON, or raises `TransportError`."""
    try:
        return json.loads(text)
    except ValueError as exc:
        raise TransportError(f"malformed JSON body from {url}: {exc}") from exc


def _envelope_from_payload(payload: Any, url: str) -> dict[str, Any]:
    """Rejects a decoded JSON payload that is not a request envelope shape:
    a dict whose `results` member is a list. Shared by every transport so
    they all agree on this check, not just on the per-op normalization that
    runs after it (`_stringify_op_errors`/`_validate_envelope_results`)."""
    if not isinstance(payload, dict) or not isinstance(payload.get("results"), list):
        raise TransportError(f"response from {url} is not a request envelope: {str(payload)[:200]}")
    return payload


def _is_minimal_aborted_entry(entry: Any) -> bool:
    """Whether `entry` is the daemon's minimal aborted-chain-entry shape:
    `{"ok": false, "aborted": true}`, with no `tool` — an op that was never
    dispatched because an earlier op in the same chain failed. `OpResult`
    requires `tool`, so this shape needs its own admission rule rather than
    going through `OpResult.model_validate` like an ordinary entry."""
    return (
        isinstance(entry, dict)
        and entry.get("ok") is False
        and entry.get("aborted") is True
        and "tool" not in entry
    )


def _validate_envelope_results(envelope: dict[str, Any], url: str) -> dict[str, Any]:
    """Reject an envelope whose result entries do not match `OpResult`.

    Runs after `_stringify_op_errors`, so a per-op error is already the
    plain string `OpResult.error` expects, not a transport-native
    `{"code","message"}` object — a top-level-only check would otherwise let
    e.g. `{"results": [42]}` or an entry missing `ok`/`tool` reach the caller
    as a successful response.

    A minimal aborted entry (see `_is_minimal_aborted_entry`) is admitted
    without going through `OpResult`, but normalized in place to carry
    `tool: ""` first, keeping every entry (aborted or not) satisfying
    `OpResult.tool: str` exactly, so every transport hands the caller the
    same object.
    """
    for index, entry in enumerate(envelope["results"]):
        if _is_minimal_aborted_entry(entry):
            entry["tool"] = ""
            continue
        try:
            OpResult.model_validate(entry)
        except ValidationError as exc:
            raise TransportError(
                f"response from {url} has a malformed result at index {index}: {exc}"
            ) from exc
    return envelope


def _stringify_op_errors(envelope: Any, url: str) -> Any:
    """Flatten a transport's `{"code","message"}` per-op error objects to a
    string, in place, so each entry still validates against
    `OpResult.error: str | None`.

    Validates the error object's shape first: `code`, when present, and
    `message` must both be strings — anything else is a malformed entry,
    not a value to flatten and pass along.
    """
    if not isinstance(envelope, dict):
        return envelope
    for index, entry in enumerate(envelope.get("results", [])):
        if not isinstance(entry, dict):
            continue
        err = entry.get("error")
        if not isinstance(err, dict):
            continue
        code = err.get("code")
        if code is not None and not isinstance(code, str):
            raise TransportError(
                f"response from {url} has a malformed error object at index {index}: "
                f"'code' must be a string, got {type(code).__name__}"
            )
        message = err.get("message")
        if not isinstance(message, str):
            raise TransportError(
                f"response from {url} has a malformed error object at index {index}: "
                f"'message' must be a string, got {type(message).__name__}"
            )
        entry["error"] = f"{code}: {message}" if code else message
    return envelope
