"""JSON schema definitions for khive-mcp verb response shapes.

All schemas follow the verbose-presentation envelope produced by
the `request` tool (ADR-016 / ADR-027).
"""

from __future__ import annotations

from typing import Any

import jsonschema

# ---------------------------------------------------------------------------
# Request envelope (outer wrapper from `request` tool)
# ---------------------------------------------------------------------------

REQUEST_ENVELOPE_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["results"],
    "properties": {
        "results": {
            "type": "array",
            "items": {
                "type": "object",
                "required": ["ok"],
                "properties": {
                    "ok": {"type": "boolean"},
                    "tool": {"type": "string"},
                    "result": {},
                    "error": {"type": "string"},
                },
            },
        }
    },
}

# ---------------------------------------------------------------------------
# Per-op result schemas
# ---------------------------------------------------------------------------

ENTITY_RECORD_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["id", "kind", "entity_kind", "name", "namespace"],
    "properties": {
        "id": {"type": "string"},
        "kind": {"type": "string", "const": "entity"},
        "entity_kind": {"type": "string"},
        "name": {"type": "string"},
        "namespace": {"type": "string"},
        "description": {"type": ["string", "null"]},
        "tags": {"type": "array", "items": {"type": "string"}},
        "properties": {"type": ["object", "null"]},
        "created_at": {"type": "string"},
        "updated_at": {"type": "string"},
    },
}

NOTE_RECORD_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["id", "kind", "note_kind", "content", "namespace"],
    "properties": {
        "id": {"type": "string"},
        "kind": {"type": "string", "const": "note"},
        "note_kind": {"type": "string"},
        "content": {"type": "string"},
        "namespace": {"type": "string"},
        "salience": {"type": ["number", "null"]},
        "decay_factor": {"type": ["number", "null"]},
        "created_at": {"type": "string"},
        "updated_at": {"type": "string"},
    },
}

EDGE_RECORD_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["id", "kind", "source_id", "target_id", "relation"],
    "properties": {
        "id": {"type": "string"},
        "kind": {"type": "string", "const": "edge"},
        "source_id": {"type": "string"},
        "target_id": {"type": "string"},
        "relation": {"type": "string"},
        "weight": {"type": ["number", "null"]},
        "namespace": {"type": "string"},
    },
}

# get() wraps the record in a kind/data envelope
GET_RESPONSE_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["kind", "data"],
    "properties": {
        "kind": {"type": "string", "enum": ["entity", "note", "edge"]},
        "data": {"type": "object"},
    },
}

LIST_RESPONSE_SCHEMA: dict[str, Any] = {
    "type": "array",
}

SEARCH_RESPONSE_SCHEMA: dict[str, Any] = {
    "type": "array",
    "items": {
        "type": "object",
        "required": ["id"],
        "properties": {
            "id": {"type": "string"},
            "score": {"type": ["number", "null"]},
        },
    },
}

LINK_RESPONSE_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["id", "source_id", "target_id", "relation"],
    "properties": {
        "id": {"type": "string"},
        "source_id": {"type": "string"},
        "target_id": {"type": "string"},
        "relation": {"type": "string"},
        "weight": {"type": ["number", "null"]},
    },
}

MERGE_RESPONSE_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["kept_id", "removed_id"],
    "properties": {
        "kept_id": {"type": "string"},
        "removed_id": {"type": "string"},
    },
}

DELETE_RESPONSE_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["deleted"],
    "properties": {
        "deleted": {"type": "boolean"},
        "id": {"type": "string"},
    },
}

RECALL_RESPONSE_SCHEMA: dict[str, Any] = {
    "type": "array",
    "items": {
        "type": "object",
        "required": ["id"],
    },
}

REMEMBER_RESPONSE_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["id"],
    "properties": {
        "id": {"type": "string"},
    },
}

# ---------------------------------------------------------------------------
# Validation helpers
# ---------------------------------------------------------------------------


def validate(instance: Any, schema: dict[str, Any], context: str = "") -> None:
    """Assert *instance* conforms to *schema*, raising AssertionError with context."""
    try:
        jsonschema.validate(instance=instance, schema=schema)
    except jsonschema.ValidationError as exc:
        prefix = f"[{context}] " if context else ""
        raise AssertionError(f"{prefix}Schema validation failed: {exc.message}") from exc


def assert_envelope(envelope: dict[str, Any]) -> None:
    """Assert top-level request envelope shape."""
    validate(envelope, REQUEST_ENVELOPE_SCHEMA, context="envelope")


def assert_entity(result: Any, context: str = "entity") -> None:
    validate(result, ENTITY_RECORD_SCHEMA, context=context)


def assert_note(result: Any, context: str = "note") -> None:
    validate(result, NOTE_RECORD_SCHEMA, context=context)


def assert_edge(result: Any, context: str = "edge") -> None:
    validate(result, EDGE_RECORD_SCHEMA, context=context)


def assert_get_response(result: Any) -> None:
    validate(result, GET_RESPONSE_SCHEMA, context="get")


def assert_list_response(result: Any) -> None:
    validate(result, LIST_RESPONSE_SCHEMA, context="list")


def assert_search_response(result: Any) -> None:
    validate(result, SEARCH_RESPONSE_SCHEMA, context="search")


def assert_link_response(result: Any) -> None:
    validate(result, LINK_RESPONSE_SCHEMA, context="link")


def assert_merge_response(result: Any) -> None:
    validate(result, MERGE_RESPONSE_SCHEMA, context="merge")


def assert_delete_response(result: Any) -> None:
    validate(result, DELETE_RESPONSE_SCHEMA, context="delete")


def assert_recall_response(result: Any) -> None:
    validate(result, RECALL_RESPONSE_SCHEMA, context="recall")


def assert_remember_response(result: Any) -> None:
    validate(result, REMEMBER_RESPONSE_SCHEMA, context="remember")
