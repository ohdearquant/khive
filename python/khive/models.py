"""Pydantic models mirroring khive's domain types.

These mirror the Rust domain types in `crates/khive-types` (Entity, Note,
Edge) and the paged result shape. They are deliberately tolerant on input
(`extra="allow"`): the daemon's JSON is the source of truth and this client
must keep reading result rows produced by a newer server without choking on
fields it does not know yet. Writes only ever send the fields spelled out
here.

`Page.total` is `Optional` on purpose: the server may skip the COUNT for
queries where computing it would scan the whole filtered set (count-free
pagination). `total=None` means "not counted", never "zero".
"""

from __future__ import annotations

from datetime import datetime
from enum import Enum
from typing import Any, Generic, TypeVar

from pydantic import BaseModel, ConfigDict, Field, field_validator

T = TypeVar("T")


class EdgeRelation(str, Enum):
    """The closed edge ontology (ADR-002 base 15 + ADR-055 epistemic 2)."""

    contains = "contains"
    part_of = "part_of"
    instance_of = "instance_of"
    extends = "extends"
    variant_of = "variant_of"
    introduced_by = "introduced_by"
    supersedes = "supersedes"
    derived_from = "derived_from"
    precedes = "precedes"
    depends_on = "depends_on"
    enables = "enables"
    implements = "implements"
    competes_with = "competes_with"
    composed_with = "composed_with"
    annotates = "annotates"
    supports = "supports"
    refutes = "refutes"


class _Record(BaseModel):
    model_config = ConfigDict(extra="allow")

    id: str | None = None
    created_at: datetime | None = None
    updated_at: datetime | None = None
    deleted_at: datetime | None = None
    properties: dict[str, Any] = Field(default_factory=dict)
    metadata: dict[str, Any] = Field(default_factory=dict)
    namespace: str | None = None
    tags: list[str] = Field(default_factory=list)
    kind: str

    # The server serializes an absent map/list as JSON null; read it as empty.
    @field_validator("properties", "metadata", mode="before", check_fields=False)
    @classmethod
    def _null_map(cls, v: Any) -> Any:
        return {} if v is None else v

    @field_validator("tags", mode="before", check_fields=False)
    @classmethod
    def _null_list(cls, v: Any) -> Any:
        return [] if v is None else v

class Entity(_Record):
    """A named node in the graph. `kind` validates server-side against the
    pack-declared entity vocabulary (concept, document, project, ...)."""

    name: str
    description: str | None = None

class Note(_Record):
    """Free-text annotation record. `kind` validates server-side against the
    pack-declared note vocabulary (observation, insight, task, memory, ...)."""

    kind: str = "observation"
    subject: str
    content: str

    @field_validator("kind", mode="before")
    def _validate_kind(cls, v: Any) -> str:
        if v is None:
            return "observation"
        if not v:
            raise ValueError("kind must be a non-empty string")
        if not isinstance(v, str):
            raise TypeError(f"kind must be a string, got {type(v)}")
        return v

class Incidence(BaseModel):
    """One node's participation in one edge (weighted incidence).

    Weight is a property of the (node, edge) pair, NOT of the edge: the same
    edge can matter differently to each node it touches (edge-dependent
    vertex weights). A binary edge has two incidences (roles source/target);
    a hyperedge has N; a symmetric relation uses role "member"."""

    model_config = ConfigDict(extra="allow")

    node_id: str
    role: str = "member"  # "source" / "target" for binary directed edges
    weight: float = 1.0
    properties: dict[str, Any] = Field(default_factory=dict)

    @field_validator("properties", mode="before")
    @classmethod
    def _null_map(cls, v: Any) -> Any:
        return {} if v is None else v


class Edge(_Record):
    """A typed relation among records. `kind` is the relation; membership —
    including per-node weight — lives on the incidences. Binary directed
    edges are the two-incidence special case; hyperedges are just more
    incidences."""

    kind: EdgeRelation
    members: list[Incidence] = Field(default_factory=list)

    # -- membership reads (roles are data, not schema) ---------------------

    @property
    def node_ids(self) -> list[str]:
        return [m.node_id for m in self.members]

    def with_role(self, role: str) -> list[Incidence]:
        return [m for m in self.members if m.role == role]

    def others(self, node_id: str) -> list[Incidence]:
        """Everyone else in this edge — the neighbor set as seen from one node."""
        return [m for m in self.members if m.node_id != node_id]

    # Sugar for the two-incidence case only; None on hyperedges.
    @property
    def source_id(self) -> str | None:
        ms = self.with_role("source")
        return ms[0].node_id if len(ms) == 1 else None

    @property
    def target_id(self) -> str | None:
        ms = self.with_role("target")
        return ms[0].node_id if len(ms) == 1 else None

    def weight_for(self, node_id: str) -> float:
        """This edge's weight relative to one participating node."""
        for m in self.members:
            if m.node_id == node_id:
                return m.weight
        raise KeyError(f"{node_id} is not a member of edge {self.id}")

class Embedding(BaseModel):
    """A client-supplied vector (the pgvector model: YOU embed, the db stores).

    `model` names the embedding space — vectors from different models are
    never comparable, so the server must index per model name. PENDING
    server support: today's daemon only embeds server-side (lattice native
    models; `embedding_content` overrides the text, not the vector)."""

    model: str
    vector: list[float]


class Attachment(BaseModel):
    """A blob attached to a record under a role (one blob per role)."""

    model_config = ConfigDict(extra="allow")

    content_ref: str  # BLAKE3 of the bytes, from blob.put
    role: str = "attachment"
    media_type: str | None = None
    size: int | None = None


class Page(BaseModel, Generic[T]):
    """One page of results. `total=None` means the server skipped the count."""

    model_config = ConfigDict(extra="allow")

    items: list[T] = Field(default_factory=list)
    total: int | None = None
    next_offset: int | None = None


class OpResult(BaseModel):
    """One op's outcome inside a request, exactly as the server reports it."""

    model_config = ConfigDict(extra="allow")

    ok: bool
    tool: str
    result: Any = None
    error: str | None = None
