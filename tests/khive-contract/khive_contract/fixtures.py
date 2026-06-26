"""Canonical constants and closed sets for khive contract tests.

These are facts derived from the ADRs — not generated at runtime.
Test modules import from here so there is one source of truth.
"""

from __future__ import annotations

# ---------------------------------------------------------------------------
# Entity kind taxonomy (ADR-001 + ADR-048)
# 8 base kinds + resource (ADR-048) = 9 total
# Source of truth: crates/khive-pack-kg/src/vocab.rs EntityKind::NAMES
# ---------------------------------------------------------------------------

ENTITY_KINDS: frozenset[str] = frozenset(
    {
        "concept",
        "document",
        "dataset",
        "project",
        "person",
        "org",
        "artifact",
        "service",
        "resource",
    }
)

# ---------------------------------------------------------------------------
# Note kind taxonomy (ADR-013)
# 5 canonical kinds — no aliases accepted
# Source of truth: crates/khive-pack-kg/src/vocab.rs NoteKind::NAMES
# ---------------------------------------------------------------------------

NOTE_KINDS: frozenset[str] = frozenset(
    {
        "observation",
        "insight",
        "question",
        "decision",
        "reference",
    }
)

# ---------------------------------------------------------------------------
# Edge relation ontology (ADR-002 base 15 + ADR-055 epistemic 2 = 17 total)
# Source of truth: crates/khive-types/src/edge.rs EdgeRelation::VALID_NAMES
# ---------------------------------------------------------------------------

EDGE_RELATIONS: frozenset[str] = frozenset(
    {
        # Structure
        "contains",
        "part_of",
        "instance_of",
        # Derivation
        "extends",
        "variant_of",
        "introduced_by",
        "supersedes",
        # Provenance
        "derived_from",
        # Temporal
        "precedes",
        # Dependency
        "depends_on",
        "enables",
        # Implementation
        "implements",
        # Lateral
        "competes_with",
        "composed_with",
        # Annotation
        "annotates",
        # Epistemic (ADR-055)
        "supports",
        "refutes",
    }
)

# annotates has source-must-be-note constraint (ADR-002 §annotates)
ANNOTATES_SOURCE_MUST_BE_NOTE = True

# ---------------------------------------------------------------------------
# Product verb manifest (ADR-023 / ADR-025 / ADR-027)
# KG pack ships 16 verbs; bare names (no pack prefix).
# Source of truth: crates/khive-pack-kg/src/handler_defs.rs KG_HANDLERS
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
        "stats",
        "propose",
        "review",
        "withdraw",
        "verbs",
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

# The play spec says "15 product verbs"; the baseline exposes 23
# (KG:16 + GTD:5 + memory:2). DISCOVERABLE_PRODUCT_VERBS (23) subsumes
# the stated minimum (15).
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
