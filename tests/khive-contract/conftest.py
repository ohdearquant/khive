"""Shared pytest fixtures for the khive-contract test suite.

All fixtures here are deterministic except the unique namespace suffix.
Tests must pass namespace=temp_namespace to all verbs that accept it.
"""

from __future__ import annotations

import re
import secrets
import uuid
from pathlib import Path
from typing import Any, Callable, Iterator, Mapping, Sequence

import pytest

from khive_contract.client import KhiveMcpSession


# ---------------------------------------------------------------------------
# Session fixtures — one MCP process per test session, shared across tests.
# Tests MUST use temp_namespace to avoid cross-test contamination.
# ---------------------------------------------------------------------------


@pytest.fixture(scope="session")
def khive_session() -> Iterator[KhiveMcpSession]:
    """KG-only MCP session.

    ADR: ADR-027 (single-tool MCP surface)
    Spawn config: packs=("kg",), db=":memory:", no_embed=True, log="error".
    """
    with KhiveMcpSession(packs=("kg",), db=":memory:", no_embed=True, log="error") as session:
        yield session


@pytest.fixture(scope="session")
def khive_gtd_session() -> Iterator[KhiveMcpSession]:
    """KG + GTD MCP session.

    ADR: ADR-019 (GTD pack)
    Spawn config: packs=("kg", "gtd"), db=":memory:", no_embed=True, log="error".
    """
    with KhiveMcpSession(
        packs=("kg", "gtd"), db=":memory:", no_embed=True, log="error"
    ) as session:
        yield session


@pytest.fixture(scope="session")
def khive_memory_session() -> Iterator[KhiveMcpSession]:
    """KG + memory MCP session.

    ADR: ADR-021 (memory pack)
    Spawn config: packs=("kg", "memory"), db=":memory:", no_embed=True, log="error".
    """
    with KhiveMcpSession(
        packs=("kg", "memory"), db=":memory:", no_embed=True, log="error"
    ) as session:
        yield session


@pytest.fixture(scope="session")
def khive_formal_session() -> Iterator[KhiveMcpSession]:
    """KG + formal-math ontology MCP session.

    ADR: ADR-017 (pack standard, edge endpoint rules)
    Spawn config: packs=("kg", "formal"), db=":memory:", no_embed=True, log="error".

    The formal pack (crates/khive-pack-formal) registers 21 EntityOfType edge
    endpoint rules for six concept subtypes (theorem, definition, structure,
    instance, axiom, goal) — no verbs, pure ontology extension.
    """
    with KhiveMcpSession(
        packs=("kg", "formal"), db=":memory:", no_embed=True, log="error"
    ) as session:
        yield session


# ---------------------------------------------------------------------------
# Function fixtures — unique per test, never shared.
# ---------------------------------------------------------------------------


@pytest.fixture
def temp_namespace(request: pytest.FixtureRequest) -> str:
    """Unique namespace per test function.

    Format: "pyct_<sanitized-node-name>_<12-hex-random>".
    Contains only lowercase letters, digits, and underscores.
    """
    raw_name = request.node.name
    sanitized = re.sub(r"[^a-z0-9]", "_", raw_name.lower())[:32]
    suffix = secrets.token_hex(6)
    return f"pyct_{sanitized}_{suffix}"


@pytest.fixture
def sample_entity(temp_namespace: str) -> Callable[..., dict[str, Any]]:
    """Factory for create(kind="entity", ...) args.

    Returns args dict only — does NOT call the MCP session.
    """

    def factory(
        entity_kind: str = "concept",
        name: str | None = None,
        *,
        entity_type: str | None = None,
        description: str | None = None,
        properties: Mapping[str, Any] | None = None,
        tags: Sequence[str] | None = None,
        namespace: str | None = None,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {
            "kind": "entity",
            "entity_kind": entity_kind,
            "name": name or f"{entity_kind}_{uuid.uuid4().hex[:8]}",
            "namespace": namespace or temp_namespace,
        }
        if entity_type is not None:
            args["entity_type"] = entity_type
        if description is not None:
            args["description"] = description
        if properties is not None:
            args["properties"] = dict(properties)
        if tags is not None:
            args["tags"] = list(tags)
        return args

    return factory


@pytest.fixture
def sample_note(temp_namespace: str) -> Callable[..., dict[str, Any]]:
    """Factory for create(kind="note", ...) args.

    Returns args dict only — does NOT call the MCP session.
    """

    def factory(
        note_kind: str = "observation",
        content: str | None = None,
        *,
        salience: float | None = 0.5,
        decay_factor: float | None = None,
        properties: Mapping[str, Any] | None = None,
        tags: Sequence[str] | None = None,
        namespace: str | None = None,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {
            "kind": "note",
            "note_kind": note_kind,
            "content": content or f"note {note_kind} {uuid.uuid4().hex[:8]}",
            "namespace": namespace or temp_namespace,
        }
        if salience is not None:
            args["salience"] = salience
        if decay_factor is not None:
            args["decay_factor"] = decay_factor
        if properties is not None:
            args["properties"] = dict(properties)
        if tags is not None:
            args["tags"] = list(tags)
        return args

    return factory


@pytest.fixture
def sample_edge(temp_namespace: str) -> Callable[..., dict[str, Any]]:
    """Factory for link(...) args.

    Returns args dict only — does NOT call the MCP session.
    """

    def factory(
        source_id: str,
        target_id: str,
        relation: str = "extends",
        *,
        weight: float | None = 1.0,
        properties: Mapping[str, Any] | None = None,
        metadata: Mapping[str, Any] | None = None,
        namespace: str | None = None,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {
            "source_id": source_id,
            "target_id": target_id,
            "relation": relation,
            "namespace": namespace or temp_namespace,
        }
        if weight is not None:
            args["weight"] = weight
        if properties is not None:
            args["properties"] = dict(properties)
        if metadata is not None:
            args["metadata"] = dict(metadata)
        return args

    return factory


# ---------------------------------------------------------------------------
# Path helpers (optional)
# ---------------------------------------------------------------------------

_PKG_ROOT = Path(__file__).parent


@pytest.fixture
def golden_dir() -> Path:
    """Path to the golden/ directory inside the package root."""
    return _PKG_ROOT / "golden"


@pytest.fixture
def baseline_path() -> Path:
    """Path to baselines/latency.json inside the package root."""
    return _PKG_ROOT / "baselines" / "latency.json"


# ---------------------------------------------------------------------------
# CLI option: --update-golden
# ---------------------------------------------------------------------------


def pytest_addoption(parser: pytest.Parser) -> None:
    parser.addoption(
        "--update-golden",
        action="store_true",
        default=False,
        help="Regenerate golden snapshot files instead of comparing them.",
    )


@pytest.fixture
def update_golden(request: pytest.FixtureRequest) -> bool:
    return bool(request.config.getoption("--update-golden", default=False))
