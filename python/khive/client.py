"""The store-shaped client facade.

khive is treated here as a database: entities, notes, and edges are the
substrate; search, graph traversal, and GQL/SPARQL are the read planes;
`diagnostics()`/`metrics()` are the observability plane. There is no
pack/verb surface in this API — the wire carries substrate operations only.

Write semantics, stated once because every experiment depends on them:

- Every write goes through the daemon's single writer. This client never
  opens the database file; a second writer is how contention starts.
- A `batch()` is one request frame but NOT a transaction: ops execute
  individually and a failure does not roll back its siblings. The per-op
  results say exactly which ops landed. Order within one batch is not
  guaranteed — chain dependent writes as separate calls instead.
- Ids are minted server-side and returned; this client never invents ids.
"""

from __future__ import annotations

import json
from typing import Any

from .errors import BatchError, OperationError
from .models import Edge, EdgeRelation, Entity, Incidence, Note, OpResult, Page
from .ops import encode, op
from .transport import Session, SocketTransport, Transport


def _one(results: list[dict[str, Any]]) -> Any:
    r = OpResult.model_validate(results[0])
    if not r.ok:
        raise OperationError(r.tool, r.error or "unknown error")
    return r.result


# -- wire translation (TEMPORARY) ------------------------------------------
# The models are the target interface; today's daemon still speaks the old
# field names. These two adapters bridge until the Rust side follows
# (graph_edges relation->kind, weight column -> properties; notes +subject).
# Delete them when the daemon serializes the new shape.


def _edge_from_wire(row: dict[str, Any]) -> Edge:
    row = dict(row)
    if "kind" not in row and "relation" in row:
        row["kind"] = row.pop("relation")
    if "members" not in row:
        # Reconstruct incidences. Per-node weights round-trip through
        # metadata["incidences"]; absent that, the legacy edge-level weight
        # is all we have and both endpoints inherit it.
        stored = (row.get("metadata") or {}).get("incidences")
        if stored:
            row["members"] = stored
        else:
            w = row.pop("weight", 1.0)
            row["members"] = [
                {"node_id": row.pop("source_id"), "role": "source", "weight": w},
                {"node_id": row.pop("target_id"), "role": "target", "weight": w},
            ]
    return Edge.model_validate(row)


def _note_from_wire(row: dict[str, Any]) -> Note:
    row = dict(row)
    if "subject" not in row:
        row["subject"] = (row.get("properties") or {}).get("subject", "")
    return Note.model_validate(row)


def _page(raw: Any, parse: Any) -> Page:
    if isinstance(raw, dict):
        items = raw.get("items", raw.get("results", []))
        total = raw.get("total")
        next_offset = raw.get("next_offset")
    else:
        items, total, next_offset = raw or [], None, None
    return Page(items=[parse(x) for x in items], total=total, next_offset=next_offset)


class Khive:
    """Client for one khived daemon.

    >>> db = Khive()                          # ~/.khive/khived.sock
    >>> db = Khive(socket_path="/tmp/x.sock") # a scratch daemon
    """

    def __init__(
        self,
        *,
        socket_path: str | None = None,
        transport: Transport | None = None,
        namespace: str = "local",
        actor_id: str | None = None,
        timeout: float = 30.0,
    ) -> None:
        if transport is None:
            transport = SocketTransport(socket_path)
        self.session = Session(
            transport, namespace=namespace, actor_id=actor_id, timeout=timeout
        )
        self.entities = _Entities(self)
        self.notes = _Notes(self)
        self.graph = _Graph(self)

    # -- raw planes --------------------------------------------------------

    def raw(self, ops: list[dict[str, Any]]) -> list[OpResult]:
        """Send pre-built ops; per-op outcomes in request order, unraised."""
        return [OpResult.model_validate(r) for r in self.session.request(encode(ops))]

    def batch(self, ops: list[dict[str, Any]]) -> list[OpResult]:
        """Like `raw`, but raises `BatchError` if any op failed."""
        results = self.session.request(encode(ops))
        failures = [
            (i, r.get("tool", "?"), str(r.get("error")))
            for i, r in enumerate(results)
            if not r.get("ok")
        ]
        if failures:
            raise BatchError(results, failures)
        return [OpResult.model_validate(r) for r in results]

    # -- database-wide reads ----------------------------------------------

    def stats(self) -> dict[str, Any]:
        return _one(self.session.request(encode([op("stats")])))

    def diagnostics(self) -> dict[str, Any]:
        """Writer/WAL/checkpoint/contention diagnostics (db_diagnostics)."""
        return _one(self.session.request(encode([op("db_diagnostics")])))

    def metrics(self) -> dict[str, Any]:
        """Server-side gauge snapshot straight off the frame (no dispatch)."""
        return self.session.metrics()

    def whoami(self) -> dict[str, Any]:
        return _one(self.session.request(encode([op("whoami")])))

    def get(self, id: str) -> dict[str, Any]:
        """Fetch any record by id — entity, note, or edge, auto-detected."""
        return _one(self.session.request(encode([op("get", id=id)])))

    def query(self, text: str, *, page_size: int | None = None) -> Any:
        """GQL or SPARQL. Deterministically paged; continue with SKIP."""
        return _one(self.session.request(encode([op("query", query=text, page_size=page_size)])))

    def search(
        self,
        query: str,
        *,
        kind: str = "entity",
        limit: int | None = None,
    ) -> list[dict[str, Any]]:
        raw = _one(self.session.request(encode([op("search", kind=kind, query=query, limit=limit)])))
        if isinstance(raw, dict):
            return raw.get("items", raw.get("results", []))
        return raw or []


class _Entities:
    def __init__(self, db: Khive) -> None:
        self._db = db

    def create(self, entity: Entity | None = None, /, **fields: Any) -> Entity:
        e = entity or Entity(**fields)
        raw = _one(
            self._db.session.request(
                encode(
                    [
                        op(
                            "create",
                            kind=e.kind,
                            name=e.name,
                            description=e.description,
                            properties=e.properties or None,
                            tags=e.tags or None,
                        )
                    ]
                )
            )
        )
        return Entity.model_validate(raw)

    def get(self, id: str) -> Entity:
        return Entity.model_validate(_one(self._db.session.request(encode([op("get", id=id)]))))

    def list(
        self,
        *,
        kind: str = "entity",
        limit: int | None = None,
        offset: int | None = None,
        **filters: Any,
    ) -> Page:
        raw = _one(
            self._db.session.request(
                encode([op("list", kind=kind, limit=limit, offset=offset, **filters)])
            )
        )
        return _page(raw, Entity.model_validate)

    def update(self, id: str, **patch: Any) -> Entity:
        raw = _one(self._db.session.request(encode([op("update", id=id, **patch)])))
        return Entity.model_validate(raw)

    def delete(self, id: str, *, hard: bool = False) -> Any:
        return _one(self._db.session.request(encode([op("delete", id=id, hard=hard or None)])))

    def merge(self, into_id: str, from_id: str) -> Any:
        return _one(
            self._db.session.request(encode([op("merge", into_id=into_id, from_id=from_id)]))
        )


class _Notes:
    def __init__(self, db: Khive) -> None:
        self._db = db

    def create(self, note: Note | None = None, /, **fields: Any) -> Note:
        n = note or Note(**fields)
        # subject rides in properties until the daemon grows the column.
        props = {**n.properties, "subject": n.subject} if n.subject else (n.properties or None)
        raw = _one(
            self._db.session.request(
                encode(
                    [
                        op(
                            "create",
                            kind=n.kind,
                            content=n.content,
                            properties=props or None,
                            tags=n.tags or None,
                        )
                    ]
                )
            )
        )
        return _note_from_wire(raw)

    def get(self, id: str) -> Note:
        return _note_from_wire(_one(self._db.session.request(encode([op("get", id=id)]))))

    def list(self, *, kind: str = "note", limit: int | None = None, **filters: Any) -> Page:
        raw = _one(self._db.session.request(encode([op("list", kind=kind, limit=limit, **filters)])))
        return _page(raw, _note_from_wire)


class _Graph:
    def __init__(self, db: Khive) -> None:
        self._db = db

    def link(
        self,
        source_id: str,
        target_id: str,
        kind: EdgeRelation | str,
        *,
        source_weight: float = 1.0,
        target_weight: float = 1.0,
        metadata: dict[str, Any] | None = None,
    ) -> Edge:
        """Binary directed edge — the two-incidence special case."""
        return self.hyperlink(
            kind,
            members=[
                Incidence(node_id=source_id, role="source", weight=source_weight),
                Incidence(node_id=target_id, role="target", weight=target_weight),
            ],
            metadata=metadata,
        )

    def hyperlink(
        self,
        kind: EdgeRelation | str,
        *,
        members: list[Incidence],
        metadata: dict[str, Any] | None = None,
    ) -> Edge:
        """One edge over N weighted incidences.

        TEMPORARY wire mapping: the daemon stores flat binary edges, so the
        full incidence list rides in metadata["incidences"] and the flat
        columns carry source/target (first two members) with the source's
        weight. Deletes when graph_incidences lands server-side."""
        if len(members) < 2:
            raise ValueError("an edge needs at least two members")
        src, tgt = members[0], members[1]
        meta = dict(metadata or {})
        meta["incidences"] = [m.model_dump() for m in members]
        raw = _one(
            self._db.session.request(
                encode(
                    [
                        op(
                            "link",
                            source_id=src.node_id,
                            target_id=tgt.node_id,
                            relation=str(getattr(kind, "value", kind)),
                            weight=src.weight,
                            metadata=meta,
                        )
                    ]
                )
            )
        )
        return _edge_from_wire(raw)

    def neighbors(
        self,
        node_id: str,
        *,
        direction: str | None = None,
        relations: list[str] | None = None,
    ) -> Any:
        return _one(
            self._db.session.request(
                encode(
                    [op("neighbors", node_id=node_id, direction=direction, relations=relations)]
                )
            )
        )

    def traverse(
        self,
        roots: list[str],
        *,
        max_depth: int | None = None,
        direction: str | None = None,
        relations: list[str] | None = None,
        limit: int | None = None,
    ) -> Any:
        return _one(
            self._db.session.request(
                encode(
                    [
                        op(
                            "traverse",
                            roots=roots,
                            max_depth=max_depth,
                            direction=direction,
                            relations=relations,
                            limit=limit,
                        )
                    ]
                )
            )
        )

    def edges(self, *, limit: int | None = None, **filters: Any) -> Page:
        raw = _one(
            self._db.session.request(encode([op("list", kind="edge", limit=limit, **filters)]))
        )
        return _page(raw, _edge_from_wire)

    # -- incidence-aware reads (client-side PROTOTYPE) ---------------------
    # The target engine computes these as an incidence join server-side:
    #   neighbors(x) = incidences[node=x] JOIN incidences[same edge, node!=x]
    # Until that lands, these scan the edge list client-side. Correct on any
    # edge arity; O(edges) — fine for experiments, not for a big store.

    def incident(self, node_id: str, *, kind: str | None = None) -> list[Edge]:
        """Every edge this node participates in, whatever its arity."""
        page = self.edges(limit=1000)
        return [
            e
            for e in page.items
            if node_id in e.node_ids and (kind is None or e.kind.value == kind)
        ]

    def co_members(self, node_id: str) -> list[tuple[Edge, Incidence]]:
        """(edge, other-member) pairs — the hypergraph neighbor view,
        each neighbor carrying ITS OWN weight in the shared edge."""
        return [(e, m) for e in self.incident(node_id) for m in e.others(node_id)]


def _json_pretty(value: Any) -> str:
    return json.dumps(value, indent=2, default=str)
