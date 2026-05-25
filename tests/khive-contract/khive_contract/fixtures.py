"""Canonical constants and closed sets for khive contract tests.

These are facts derived from the ADRs — not generated at runtime.
"""

from __future__ import annotations

# ---------------------------------------------------------------------------
# Entity kind taxonomy (ADR-001)
# ---------------------------------------------------------------------------

ENTITY_KINDS: frozenset[str] = frozenset(
    {
        "concept",
        "person",
        "project",
        "tool",
        "document",
        "event",
        "location",
        "organization",
        "resource",
        "tag",
    }
)

# ---------------------------------------------------------------------------
# Note kind taxonomy (ADR-013)
# ---------------------------------------------------------------------------

NOTE_KINDS: frozenset[str] = frozenset(
    {
        "observation",
        "question",
        "hypothesis",
        "conclusion",
        "reference",
    }
)

# ---------------------------------------------------------------------------
# Edge relation ontology (ADR-002)
# ---------------------------------------------------------------------------

EDGE_RELATIONS: frozenset[str] = frozenset(
    {
        "extends",
        "implements",
        "depends_on",
        "uses",
        "produces",
        "relates_to",
        "contradicts",
        "supersedes",
        "annotates",
        "contains",
        "part_of",
        "enables",
        "blocks",
    }
)

# annotates has source-must-be-note constraint (ADR-002 §annotates)
ANNOTATES_SOURCE_MUST_BE_NOTE = True

# ---------------------------------------------------------------------------
# Product verb manifest (ADR-023 / ADR-025 / ADR-027)
# ---------------------------------------------------------------------------

KG_VERBS: frozenset[str] = frozenset(
    {
        "create",
        "get",
        "list",
        "update",
        "delete",
        "merge",
        "search",
        "link",
        "neighbors",
        "traverse",
        "query",
    }
)

GTD_VERBS: frozenset[str] = frozenset(
    {
        "assign",
        "next",
        "complete",
        "tasks",
        "transition",
    }
)

MEMORY_VERBS: frozenset[str] = frozenset(
    {
        "remember",
        "recall",
    }
)

DISCOVERABLE_PRODUCT_VERBS: frozenset[str] = KG_VERBS | GTD_VERBS | MEMORY_VERBS

# The play spec says "15 product verbs"; the baseline exposes 18.
# DISCOVERABLE_PRODUCT_VERBS (18) subsumes the stated minimum (15).
PLAY_SPEC_MINIMUM_VERB_COUNT = 15

# ---------------------------------------------------------------------------
# Golden snapshot scrub keys
# Volatile fields to replace with "<redacted>" before saving golden files.
# ---------------------------------------------------------------------------

GOLDEN_SCRUB_KEYS: frozenset[str] = frozenset(
    {
        "id",
        "created_at",
        "updated_at",
        "timestamp",
        "embedding_id",
    }
)

# ---------------------------------------------------------------------------
# Sample payload builders (lightweight — no MCP calls)
# ---------------------------------------------------------------------------


def make_entity_args(
    entity_kind: str = "concept",
    name: str | None = None,
    namespace: str = "default",
    **kwargs,
) -> dict:
    """Return args dict for create(kind="entity", ...) — does NOT call MCP."""
    import uuid

    args: dict = {
        "kind": "entity",
        "entity_kind": entity_kind,
        "name": name or f"{entity_kind}_{uuid.uuid4().hex[:8]}",
        "namespace": namespace,
    }
    args.update(kwargs)
    return args


def make_note_args(
    note_kind: str = "observation",
    content: str | None = None,
    namespace: str = "default",
    salience: float | None = 0.5,
    **kwargs,
) -> dict:
    """Return args dict for create(kind="note", ...) — does NOT call MCP."""
    import uuid

    args: dict = {
        "kind": "note",
        "note_kind": note_kind,
        "content": content or f"note {note_kind} {uuid.uuid4().hex[:8]}",
        "namespace": namespace,
    }
    if salience is not None:
        args["salience"] = salience
    args.update(kwargs)
    return args


def make_edge_args(
    source_id: str,
    target_id: str,
    relation: str = "extends",
    namespace: str = "default",
    weight: float | None = 1.0,
    **kwargs,
) -> dict:
    """Return args dict for link(...) — does NOT call MCP."""
    args: dict = {
        "source_id": source_id,
        "target_id": target_id,
        "relation": relation,
        "namespace": namespace,
    }
    if weight is not None:
        args["weight"] = weight
    args.update(kwargs)
    return args
