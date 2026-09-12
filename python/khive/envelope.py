"""Validation for request results and plan-only responses from the daemon.

This module provides the steps a transport needs to turn raw response
bytes into validated `OpResult` entries: decode JSON, check the result is a
request-envelope shape, preserve validated per-op error objects, admit the daemon's minimal
aborted-chain-entry shape, and validate every entry against `OpResult`. It
is transport-agnostic and calls no transport itself — a transport that
wants a malformed body, a malformed envelope, or a malformed per-op entry
to raise the same error class calls these functions explicitly.
"""

from __future__ import annotations

import json
from typing import Any

from pydantic import ValidationError

from .errors import TransportError
from .models import OpError, OpResult


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
    runs after it (`_validate_op_errors`/`_validate_envelope_results`)."""
    if not isinstance(payload, dict) or not isinstance(payload.get("results"), list):
        raise TransportError(f"response from {url} is not a request envelope: {str(payload)[:200]}")
    return payload


def _plan_from_payload(payload: Any, url: str) -> dict[str, Any]:
    """Validate a plan without normalizing its fields or interpreting admission."""
    if not isinstance(payload, dict) or type(payload.get("parsed")) is not bool:
        raise TransportError(f"response from {url} is not a plan: parsed must be a boolean")
    limits = payload.get("limits")
    if not isinstance(limits, dict) or any(
        type(limits.get(key)) is not int or limits[key] < 0
        for key in ("max_ops", "max_depth", "max_input_len")
    ):
        raise TransportError(f"response from {url} has malformed plan limits")
    if payload["parsed"] is False:
        if not isinstance(payload.get("error"), str) or "stages" in payload:
            raise TransportError(f"response from {url} has a malformed plan parse error")
        return payload
    stages = payload.get("stages")
    if (
        payload.get("mode") not in ("single", "parallel", "chain")
        or type(payload.get("stage_count")) is not int
        or not isinstance(stages, list)
        or payload["stage_count"] != len(stages)
    ):
        raise TransportError(f"response from {url} has malformed plan stages")
    for index, stage in enumerate(stages):
        if (
            not isinstance(stage, dict)
            or type(stage.get("index")) is not int
            or stage["index"] != index
            or not isinstance(stage.get("verb"), str)
            or "pack" not in stage
            or (stage["pack"] is not None and not isinstance(stage["pack"], str))
            or type(stage.get("known")) is not bool
            or not isinstance(stage.get("args"), dict)
            or not isinstance(stage.get("prev_refs"), list)
            or any(not isinstance(ref, str) for ref in stage["prev_refs"])
        ):
            raise TransportError(f"response from {url} has a malformed plan stage at index {index}")
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

    Error objects remain dictionaries in the returned envelope. Validation
    never flattens or infers finality from their fields.

    A minimal aborted entry (see `_is_minimal_aborted_entry`) is normalized
    in place to carry `tool: ""` before validation, keeping every entry satisfying
    `OpResult.tool: str` exactly, so every transport hands the caller the
    same object.
    """
    for index, entry in enumerate(envelope["results"]):
        if _is_minimal_aborted_entry(entry):
            entry["tool"] = ""
        try:
            OpResult.model_validate(entry)
        except ValidationError as exc:
            raise TransportError(
                f"response from {url} has a malformed result at index {index}: {exc}"
            ) from exc
    return envelope


def _validate_op_errors(envelope: Any, url: str) -> Any:
    """Validate disposition and domain result without replacing error payloads."""
    if not isinstance(envelope, dict):
        return envelope
    for index, entry in enumerate(envelope.get("results", [])):
        if not isinstance(entry, dict):
            continue
        err = entry.get("error")
        if not isinstance(err, dict):
            continue
        try:
            OpError.model_validate(err)
        except ValidationError as exc:
            raise TransportError(
                f"response from {url} has a malformed error object at index {index}: {exc}"
            ) from exc
    return envelope


def _validate_frame_error_detail(response: dict[str, Any], url: str) -> OpError | None:
    """Expose additive daemon error details while admitting legacy text-only frames."""
    detail = response.get("error_detail")
    if detail is None:
        return None
    try:
        return OpError.model_validate(detail)
    except ValidationError as exc:
        raise TransportError(f"response from {url} has malformed error_detail: {exc}") from exc
