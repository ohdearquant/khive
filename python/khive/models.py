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
from typing import Any, Generic, Literal, TypeVar

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


class Page(BaseModel, Generic[T]):
    """One page; missing counts/continuations are not inferred from its length.

    Cursor readers follow `next_after`. An empty or short page with a cursor
    or `scan_incomplete=True` does not establish the end of the filtered scan.
    """

    model_config = ConfigDict(extra="allow")

    items: list[T] = Field(default_factory=list)
    total: int | None = None
    next_offset: int | None = None
    next_after: str | None = None
    scan_incomplete: bool | None = None
    requested_limit: int | None = None
    effective_limit: int | None = None
    limit_clamped: bool | None = None


class OpError(BaseModel):
    """A structured per-op error, including the server's unchanged write finality."""

    model_config = ConfigDict(extra="allow", strict=True)

    message: str
    kind: str | None = None
    code: str | None = None
    stage: str | None = None
    retryable: bool | None = None
    request_state: str | None = None
    task_terminated: bool | None = None
    timeout_ms: int | None = None
    capability: str | None = None
    operation: str | None = None
    scope: str | None = None
    retry_after_ms: int | None = None
    details: dict[str, str] | None = None
    domain_disposition: Literal["committed", "not_committed", "unknown"] | None = None
    domain_result: Any = None

    @field_validator("domain_disposition", mode="before")
    @classmethod
    def _non_null_disposition(cls, value: Any) -> Any:
        if value is None:
            raise ValueError("domain_disposition must name a domain outcome when present")
        return value

    def __str__(self) -> str:
        return f"{self.code}: {self.message}" if self.code else self.message


class OpResult(BaseModel):
    """One op's outcome inside a request, exactly as the server reports it."""

    model_config = ConfigDict(extra="allow")

    ok: bool
    tool: str
    result: Any = None
    error: OpError | str | None = None
    domain_disposition: Literal["committed", "not_committed", "unknown"] | None = None

    @field_validator("domain_disposition", mode="before")
    @classmethod
    def _non_null_disposition(cls, value: Any) -> Any:
        if value is None:
            raise ValueError("domain_disposition must name a domain outcome when present")
        return value


class RecallHit(BaseModel):
    """One `memory.recall` row, as the server stamps it.

    `degraded` is the server's per-row marker (`"ann_unavailable"`) on a
    non-empty degraded response; `truncated` is the per-row marker on a
    non-empty budget-capped response. Both stay optional so a clean row
    decodes to a different value from a stamped one.
    """

    model_config = ConfigDict(extra="allow")

    id: str
    score: float
    rank_score: float | None = None
    raw_score: float | None = None
    content: str | None = None
    salience: float | None = None
    decay_factor: float | None = None
    memory_type: str | None = None
    created_at: datetime | None = None
    source_id: str | None = None
    degraded: str | None = None
    degraded_reason: str | None = None
    truncated: bool | None = None
    served_by_profile_id: str | None = None
    serve_attribution: Any = None
    breakdown: Any = None

    @property
    def is_degraded(self) -> bool:
        return self.degraded is not None

    @property
    def is_truncated(self) -> bool:
        return self.truncated is True


class RecallOutcome(BaseModel):
    """The typed outcome of one `memory.recall`, preserving its envelope class.

    The server answers a non-empty recall with a bare array of rows and
    stamps degradation and truncation on each row. It changes shape only
    when the response is empty for a reason a bare `[]` could not carry:
    `{"results": [], "degraded": true, "degraded_reason": ...}`,
    `{"results": [], "truncated": true}`, or both. `from_result` reads either
    shape into one value: `hits` are the typed rows, `degraded` and
    `truncated` are true when either the envelope or any row says so, and
    `enveloped` records whether the server used the object shape. A clean
    empty recall is `hits=[]`, both flags false, `enveloped=False`.
    """

    hits: list[RecallHit] = Field(default_factory=list)
    degraded: bool = False
    truncated: bool = False
    degraded_reason: str | None = None
    enveloped: bool = False

    @property
    def envelope_class(self) -> str:
        """One of the response classes the server distinguishes, as a label."""
        flags = []
        if self.degraded:
            flags.append("degraded")
        if self.truncated:
            flags.append("truncated")
        base = "empty" if not self.hits else "rows"
        return "_".join([*flags, base]) if flags else f"clean_{base}"

    @classmethod
    def from_result(cls, result: Any) -> RecallOutcome:
        """Build from an `OpResult.result` of `memory.recall` without dropping its shape."""
        if isinstance(result, list):
            rows = result
            enveloped = False
            envelope: dict[str, Any] = {}
        elif isinstance(result, dict) and isinstance(result.get("results"), list):
            rows = result["results"]
            enveloped = True
            envelope = result
        else:
            raise TypeError(
                "memory.recall result is neither a row array nor a results envelope: "
                f"{str(result)[:200]}"
            )
        for key in ("degraded", "truncated"):
            if key in envelope and type(envelope[key]) is not bool:
                raise ValueError(f"memory.recall envelope field {key!r} must be a boolean")
        hits = [RecallHit.model_validate(row) for row in rows]
        degraded = envelope.get("degraded") is True or any(hit.is_degraded for hit in hits)
        truncated = envelope.get("truncated") is True or any(hit.is_truncated for hit in hits)
        reason = envelope.get("degraded_reason")
        if reason is None:
            reason = next((hit.degraded_reason for hit in hits if hit.degraded_reason), None)
        if reason is not None and not isinstance(reason, str):
            raise ValueError("memory.recall degraded_reason must be a string")
        return cls(
            hits=hits,
            degraded=degraded,
            truncated=truncated,
            degraded_reason=reason,
            enveloped=enveloped,
        )
