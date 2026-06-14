#!/usr/bin/env python3
"""Behavioral-contract tests for the MCP surface, served by `kkernel mcp` (GitHub issue #21).

CONTRACT vs SMOKE
-----------------
Smoke tests (tests/smoke_test.py) verify that the 11 verbs exist and return
non-error responses for valid inputs.  Contract tests go deeper: they assert
*behavioural invariants* — properties that must hold regardless of
implementation detail.  A contract test that passes gives evidence that the
system respects a specific design commitment documented in an ADR.

Concretely, these tests cover:
  1. Namespace isolation — entities from ns-A are invisible from ns-B.
  2. Short-UUID prefix resolution — 8-hex resolves; <8 or non-hex errors.
  3. GQL property projection — only valid column names compile; invalid ones
     return a compile error listing the valid set.
  4. Edge cascade on hard delete — hard-delete removes incident edges; soft-
     delete leaves them intact.
  5. Note supersession — a note targeted by a `supersedes` edge is excluded
     from search results but still gettable via `get()`.
  6. Closed taxonomy — invalid entity_kind / edge relation / note_kind each
     error with the valid values listed in the error message.
  7. Merge semantics — `merge(into_id, from_id)` rewires edges to the kept
     entity, unions tags, and the from_id is gone afterward.
  8. annotates source-must-be-note — entity-as-source is rejected with a
     clear error; note-as-source succeeds; hard-deleting the target cascades
     the edge (ADR-002 §annotates endpoint validation).
  9. Epistemic edge endpoint enforcement — supports/refutes legal pairs are
     accepted; illegal pairs (wrong target kind, cross-substrate) are rejected
     with clear error messages (ADR-055 + ADR-002 §"Epistemic relations").

How to run
----------
    python3 tests/contract_test.py

Each test function is named `test_<contract>` and prints [pass] / [FAIL].
Exit code is 0 if every test passes, 1 if any fail.

The KKERNEL_BINARY env var overrides the default binary path. The server is the
`mcp` subcommand of the unified kkernel binary.
"""

import json
import os
import subprocess
import sys
import tempfile
import traceback
from typing import Any

# ---------------------------------------------------------------------------
# Binary location
# ---------------------------------------------------------------------------

BINARY = os.environ.get(
    "KKERNEL_BINARY",
    os.path.join(
        os.path.dirname(__file__), "..", "crates", "target", "release", "kkernel"
    ),
)

# ---------------------------------------------------------------------------
# Low-level JSON-RPC + MCP helpers
# ---------------------------------------------------------------------------

_request_id = 0


def _next_id() -> int:
    global _request_id
    _request_id += 1
    return _request_id


def _send(proc: subprocess.Popen, method: str, params: Any = None) -> None:
    msg: dict = {"jsonrpc": "2.0", "id": _next_id(), "method": method}
    if params is not None:
        msg["params"] = params
    line = json.dumps(msg) + "\n"
    proc.stdin.write(line.encode())
    proc.stdin.flush()


def _recv(proc: subprocess.Popen) -> dict:
    line = proc.stdout.readline()
    if not line:
        raise RuntimeError("MCP server closed stdout unexpectedly")
    return json.loads(line)


def _request_raw(proc: subprocess.Popen, ops_string: str) -> dict:
    """Call the single `request` MCP tool and return the parsed response body.

    Uses ``presentation: "verbose"`` so test assertions receive full canonical
    UUIDs and timestamps (ADR-045 — scripted/CI callers default to Verbose).

    Returns {"_rpc_error": {...}} if the server replied with a JSON-RPC error
    (i.e. the DSL itself was rejected — malformed input).
    """
    _send(
        proc,
        "tools/call",
        {"name": "request", "arguments": {"ops": ops_string, "presentation": "verbose"}},
    )
    resp = _recv(proc)
    if "error" in resp:
        return {"_rpc_error": resp["error"]}
    result = resp.get("result", {})
    if result.get("isError"):
        content = result.get("content", [])
        text = content[0]["text"] if content else ""
        return {"_rpc_error": {"message": text, "code": -32603}}
    content = result.get("content", [])
    text = content[0]["text"] if content else ""
    return json.loads(text) if text else {}


def _tool_raw(proc: subprocess.Popen, name: str, args: dict) -> dict:
    """Call a verb through `request`. Return a sentinel-keyed dict.

    The MCP server exposes a single tool (`request`) per ADR-027; tests still
    talk in verbs, so this helper packs the verb into a one-op JSON-form batch,
    dispatches it, and returns:

      - On success         : {"_ok": <verb's result object>}
      - On per-op failure  : {"_op_error": "<message>"}
      - On RPC-level fail  : {"_rpc_error": {...}}

    Validation errors that v0.1 surfaced as RPC-level McpError::invalid_params
    are now per-op errors inside the batch response — see ADR-020 §dispatch
    (`{ok, error}` per op).  Callers asserting failure should use
    `_expect_rpc_error()` which accepts either channel.
    """
    ops = json.dumps([{"tool": name, "args": args}])
    body = _request_raw(proc, ops)
    if "_rpc_error" in body:
        return body
    results = body.get("results") or []
    if not results:
        return {"_rpc_error": {"message": f"empty results for verb {name}: {body}"}}
    first = results[0]
    if not first.get("ok", False):
        return {"_op_error": first.get("error", "<no error string>")}
    return {"_ok": first.get("result")}


def _tool(proc: subprocess.Popen, name: str, args: dict) -> Any:
    """Call a verb through `request`; raise on any error; return its result."""
    result = _tool_raw(proc, name, args)
    if "_rpc_error" in result:
        raise RuntimeError(f"MCP-level error calling {name}: {result['_rpc_error']}")
    if "_op_error" in result:
        raise RuntimeError(f"Verb '{name}' returned error: {result['_op_error']}")
    return result.get("_ok")


def _expect_rpc_error(proc: subprocess.Popen, name: str, args: dict) -> str:
    """Assert the verb call fails. Return the error message string.

    Pre-ADR-027 (v0.1) khive-mcp surfaced every validation failure as an
    RPC-level `McpError::invalid_params`.  Post-ADR-027 the single `request`
    tool returns per-op `{ok: false, error: "..."}` for verb-level failures
    and reserves RPC-level errors for DSL/parse failures.  Both are
    "this call did not succeed" from the caller's point of view, so this
    helper accepts either channel and returns the error message string.
    """
    result = _tool_raw(proc, name, args)

    if "_rpc_error" in result:
        err = result["_rpc_error"]
        return err.get("message", str(err))

    if "_op_error" in result:
        return result["_op_error"]

    raise AssertionError(
        f"Expected verb '{name}' to fail but got success:\n{result.get('_ok')!r}"
    )


def _tool_expect_error(proc: subprocess.Popen, name: str, args: dict) -> str:
    """Alias retained for call-sites — prefer `_expect_rpc_error()`."""
    return _expect_rpc_error(proc, name, args)


# ---------------------------------------------------------------------------
# Server lifecycle
# ---------------------------------------------------------------------------

def _start_server(db_path: str) -> subprocess.Popen:
    """Spawn a fresh `kkernel mcp` process backed by a temp SQLite file."""
    env = {**os.environ, "KHIVE_NO_DAEMON": "1"}
    proc = subprocess.Popen(
        [BINARY, "mcp", "--db", db_path, "--no-embed", "--log", "error"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    # MCP handshake
    _send(proc, "initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "contract-test", "version": "0.1.0"},
    })
    init = _recv(proc)
    assert init["result"]["serverInfo"]["name"] == "khive-mcp", f"bad init: {init}"
    notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
    proc.stdin.write((json.dumps(notify) + "\n").encode())
    proc.stdin.flush()
    return proc


def _stop_server(proc: subprocess.Popen) -> None:
    try:
        proc.stdin.close()
        proc.wait(timeout=5)
    except Exception:
        proc.kill()


# ---------------------------------------------------------------------------
# Test runner
# ---------------------------------------------------------------------------

_results: list[tuple[str, bool, str]] = []


def _run_test(name: str, fn) -> None:
    """Execute a test function; record pass/fail."""
    with tempfile.NamedTemporaryFile(suffix=".db", delete=False) as f:
        db_path = f.name
    proc = _start_server(db_path)
    try:
        fn(proc)
        _results.append((name, True, ""))
        print(f"  [pass] {name}")
    except Exception as exc:
        detail = traceback.format_exc()
        _results.append((name, False, detail))
        print(f"  [FAIL] {name}")
        print(f"         {exc}")
    finally:
        _stop_server(proc)
        try:
            os.unlink(db_path)
        except OSError:
            pass


# ---------------------------------------------------------------------------
# Contract 1 — Namespace isolation
# ---------------------------------------------------------------------------

def test_namespace_isolation(proc: subprocess.Popen) -> None:
    """All substrates (entities, notes) are namespace-isolated per ADR-050."""
    # ADR-050 supersedes ADR-007 §"Namespace-by-Layer Rule": KG entity and edge
    # verbs now honor the NamespaceToken from VerbRegistry::dispatch.  Entities,
    # edges, and notes are all scoped to the caller's namespace.

    # ---- entities: namespace-isolated ----
    entity = _tool(proc, "create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "AlphaEntity",
        "description": "Only visible in ns-alpha",
        "namespace": "ns-alpha",
    })
    full_id = entity["id"]

    # get from ns-alpha MUST succeed (own namespace)
    fetched_alpha = _tool(proc, "get", {"id": full_id, "namespace": "ns-alpha"})
    assert fetched_alpha["kind"] == "concept", f"Expected kind=concept, got {fetched_alpha['kind']}"
    assert fetched_alpha["name"] == "AlphaEntity"

    # get from ns-beta MUST fail (cross-namespace, ADR-050)
    err_text = _expect_rpc_error(proc, "get", {"id": full_id, "namespace": "ns-beta"})
    assert "not found" in err_text.lower(), (
        f"Expected entity not found from ns-beta (ADR-050), got: {err_text!r}"
    )

    # list from ns-beta MUST NOT find the entity
    entities_beta = _tool(proc, "list", {
        "kind": "entity",
        "entity_kind": "concept",
        "namespace": "ns-beta",
    })
    ids_beta = [e["id"] for e in entities_beta]
    assert full_id not in ids_beta, (
        f"AlphaEntity must NOT be visible in ns-beta list (ADR-050): {ids_beta}"
    )

    # same-namespace link MUST succeed
    alpha_entity2 = _tool(proc, "create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "AlphaEntity2",
        "namespace": "ns-alpha",
    })
    link_result = _tool(proc, "link", {
        "source_id": alpha_entity2["id"],
        "target_id": full_id,
        "relation": "extends",
        "namespace": "ns-alpha",
    })
    assert link_result.get("ok", link_result.get("id")) is not None, (
        f"Same-namespace link must succeed: {link_result}"
    )

    # ---- notes: namespace-isolated (unchanged from ADR-007) ----
    note = _tool(proc, "create", {
        "kind": "note",
        "note_kind": "observation",
        "content": "Alpha-only observation",
        "namespace": "ns-alpha",
    })
    note_id = note["id"]

    # get note from ns-beta must NOT find it
    err_text = _expect_rpc_error(proc, "get", {"id": note_id, "namespace": "ns-beta"})
    assert "not found" in err_text.lower(), (
        f"Expected note not found from ns-beta, got: {err_text!r}"
    )

    # get note from ns-alpha MUST succeed
    fetched_note = _tool(proc, "get", {"id": note_id, "namespace": "ns-alpha"})
    assert fetched_note["kind"] == "observation"


# ---------------------------------------------------------------------------
# Contract 2 — Short-UUID prefix resolution
# ---------------------------------------------------------------------------

def test_short_uuid_prefix_resolution(proc: subprocess.Popen) -> None:
    """8-char hex prefix resolves; <8 or non-hex returns an explicit error."""
    entity = _tool(proc, "create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "PrefixTarget",
    })
    full_id: str = entity["id"]
    prefix8 = full_id[:8]   # exactly 8 hex chars
    prefix7 = full_id[:7]   # too short
    prefix_bad = "ZZZZZZZZ"  # non-hex (alphabetically valid length, but contains non-hex)

    # ---- 8-char prefix resolves correctly ----
    # Post-W2 #454: flat shape, granular kind at top level.
    fetched = _tool(proc, "get", {"id": prefix8})
    assert fetched["kind"] == "concept"
    assert fetched["name"] == "PrefixTarget", (
        f"8-char prefix did not resolve to PrefixTarget: {fetched}"
    )

    # ---- 7-char prefix returns an RPC-level error ----
    err_7 = _expect_rpc_error(proc, "get", {"id": prefix7})
    assert err_7, f"Expected an error for 7-char prefix, got empty string"
    # Should not return a wrong record — confirmed by RPC error above.

    # ---- non-hex 8-char string returns an RPC-level error ----
    err_bad = _expect_rpc_error(proc, "get", {"id": prefix_bad})
    assert err_bad, f"Expected an error for non-hex prefix, got empty string"


# ---------------------------------------------------------------------------
# Contract 3 — GQL property projection
# ---------------------------------------------------------------------------

def test_gql_property_projection(proc: subprocess.Popen) -> None:
    """RETURN a.name returns only the name column; RETURN a.bogus returns a compile error."""
    # Seed two entities with an edge
    a = _tool(proc, "create", {
        "kind": "entity", "entity_kind": "concept", "name": "GQL_A",
    })
    b = _tool(proc, "create", {
        "kind": "entity", "entity_kind": "concept", "name": "GQL_B",
    })
    _tool(proc, "link", {
        "source_id": a["id"], "target_id": b["id"],
        "relation": "extends", "weight": 1.0,
    })

    # ---- RETURN a.name, b.name — must succeed and contain only those columns ----
    result = _tool(proc, "query", {
        "query": "MATCH (a:concept)-[e:extends]->(b:concept) RETURN a.name, b.name LIMIT 10",
    })
    rows = result.get("rows", result) if isinstance(result, dict) else result
    assert isinstance(rows, list) and len(rows) >= 1, (
        f"Expected >=1 rows for valid projection, got: {result}"
    )
    row = rows[0]
    # The runtime serialises SqlRow as {"columns": [{"name": col_name, "value": {...}}]}.
    # Flatten it into a name→value dict for assertions.
    if "columns" in row:
        flat_row = {col["name"]: col["value"] for col in row["columns"]}
    else:
        flat_row = row  # already flat (future-proofing)

    # Columns should include a_name and b_name (the result serialisation maps
    # "var.prop" → "var_prop" key).
    assert "a_name" in flat_row, f"a_name key missing from projected row: {flat_row}"
    assert "b_name" in flat_row, f"b_name key missing from projected row: {flat_row}"
    # Should NOT contain full entity blobs (i.e. a_properties should not be present)
    assert "a_properties" not in flat_row, (
        f"Property projection leaked a_properties into result: {flat_row}"
    )

    # ---- RETURN a.bogus — must return a compile error listing valid columns ----
    # khive-query surfaces this as a RPC-level error (McpError::invalid_request,
    # code -32603) with the message format:
    #   "query: compile error: unknown node property '<token>' in RETURN projection.
    #    Valid: id, name, kind, namespace, description, properties, created_at, updated_at"
    err_text = _expect_rpc_error(proc, "query", {
        "query": "MATCH (a:concept)-[e:extends]->(b:concept) RETURN a.bogus LIMIT 5",
    })
    # Error must quote the offending token in single-quotes exactly as the compiler emits.
    assert "'bogus'" in err_text, (
        f"Error text should name the offending property as \"'bogus'\": {err_text!r}"
    )
    # Error must contain the compiler's fixed-format valid-column list.  If the
    # columns change, this assertion will catch the drift.
    assert "Valid: id, name, kind, entity_type, namespace, description, properties, created_at, updated_at" in err_text, (
        f"Error text must contain the full valid-column list emitted by the compiler: {err_text!r}"
    )


# ---------------------------------------------------------------------------
# Contract 4 — Edge cascade on hard delete
# ---------------------------------------------------------------------------

def test_edge_cascade_hard_delete(proc: subprocess.Popen) -> None:
    """Hard-delete removes incident edges; soft-delete leaves edges in place."""
    # ---- Hard delete: edges must be removed ----
    hub = _tool(proc, "create", {
        "kind": "entity", "entity_kind": "concept", "name": "HubHard",
    })
    spoke1 = _tool(proc, "create", {
        "kind": "entity", "entity_kind": "concept", "name": "Spoke1Hard",
    })
    spoke2 = _tool(proc, "create", {
        "kind": "entity", "entity_kind": "concept", "name": "Spoke2Hard",
    })
    e1 = _tool(proc, "link", {
        "source_id": hub["id"], "target_id": spoke1["id"], "relation": "extends",
    })
    # ADR-002: depends_on is restricted to Project/Service/Artifact endpoints, not Concept.
    # Use `enables` (valid concept-to-concept) for this contract.
    e2 = _tool(proc, "link", {
        "source_id": spoke2["id"], "target_id": hub["id"], "relation": "enables",
    })
    e1_id = e1["id"]
    e2_id = e2["id"]

    # Verify edges exist before delete
    edges_before = _tool(proc, "list", {"kind": "edge", "source_id": hub["id"]})
    assert any(e["id"] == e1_id for e in edges_before), (
        "outbound edge from hub not listed before hard-delete"
    )

    # Hard-delete the hub
    del_result = _tool(proc, "delete", {"id": hub["id"], "kind": "entity", "hard": True})
    assert del_result["deleted"] is True, f"Hard delete should return deleted=true: {del_result}"

    # Both incident edges must be gone — assert via get() AND via list() so the
    # proof is symmetric with the pre-delete list assertion above.
    err_e1 = _expect_rpc_error(proc, "get", {"id": e1_id})
    assert "not found" in err_e1.lower(), (
        f"Outbound edge should be deleted after hard-delete of source: {err_e1!r}"
    )
    err_e2 = _expect_rpc_error(proc, "get", {"id": e2_id})
    assert "not found" in err_e2.lower(), (
        f"Inbound edge should be deleted after hard-delete of target: {err_e2!r}"
    )

    # Symmetric list proof: list by source_id and target_id must both return empty
    # (mirrors the pre-delete list assertion — completes the cascade proof).
    edges_after_src = _tool(proc, "list", {"kind": "edge", "source_id": hub["id"]})
    assert edges_after_src == [] or not any(
        e["id"] in (e1_id, e2_id) for e in edges_after_src
    ), (
        f"list(source_id=hub) should return no incident edges after hard-delete, "
        f"got: {[e['id'] for e in edges_after_src]}"
    )
    edges_after_tgt = _tool(proc, "list", {"kind": "edge", "target_id": hub["id"]})
    assert edges_after_tgt == [] or not any(
        e["id"] in (e1_id, e2_id) for e in edges_after_tgt
    ), (
        f"list(target_id=hub) should return no incident edges after hard-delete, "
        f"got: {[e['id'] for e in edges_after_tgt]}"
    )

    # ---- Soft delete: edges must remain ----
    hub_soft = _tool(proc, "create", {
        "kind": "entity", "entity_kind": "concept", "name": "HubSoft",
    })
    spoke_soft = _tool(proc, "create", {
        "kind": "entity", "entity_kind": "concept", "name": "SpokeSoft",
    })
    e_soft = _tool(proc, "link", {
        "source_id": hub_soft["id"], "target_id": spoke_soft["id"], "relation": "extends",
    })
    e_soft_id = e_soft["id"]

    del_soft = _tool(proc, "delete", {"id": hub_soft["id"], "kind": "entity"})  # hard=False by default
    assert del_soft["deleted"] is True

    # Edge should still be retrievable after soft delete
    fetched_edge = _tool(proc, "get", {"id": e_soft_id})
    assert fetched_edge["kind"] == "edge", (
        f"Edge should survive soft-delete of incident entity: {fetched_edge}"
    )


# ---------------------------------------------------------------------------
# Contract 5 — Note supersession
# ---------------------------------------------------------------------------

def test_note_supersession(proc: subprocess.Popen) -> None:
    """A note targeted by a `supersedes` edge is excluded from search but still gettable."""
    # Create old note (to be superseded)
    old_note = _tool(proc, "create", {
        "kind": "note",
        "note_kind": "observation",
        "content": "SupersededContent unique_token_abc",
        "salience": 0.8,
    })
    old_id = old_note["id"]

    # Create new note that supersedes the old one
    new_note = _tool(proc, "create", {
        "kind": "note",
        "note_kind": "insight",
        "content": "NewerContent unique_token_abc",
        "salience": 0.9,
    })
    new_id = new_note["id"]

    # Wire the supersedes edge: new → old (new supersedes old)
    _tool(proc, "link", {
        "source_id": new_id,
        "target_id": old_id,
        "relation": "supersedes",
        "weight": 1.0,
    })

    # ---- search(kind=note) must exclude the superseded old note AND include the new note ----
    # Both notes share the token "unique_token_abc".  A bug that dropped ALL notes
    # sharing the token would make old_id absent (correct) but also new_id absent
    # (false green).  Asserting new_id IS present closes that gap.
    hits = _tool(proc, "search", {
        "kind": "note",
        "query": "unique_token_abc",
        "limit": 20,
    })
    hit_note_ids = [h.get("id", "") for h in hits]

    assert old_id not in hit_note_ids, (
        f"Superseded note (old_id={old_id}) should be excluded from search, "
        f"but appeared in hits: {hit_note_ids}"
    )
    assert new_id in hit_note_ids, (
        f"New note (new_id={new_id}) MUST appear in search results — the unique "
        f"token 'unique_token_abc' is shared by both notes, so a correct "
        f"implementation must return the non-superseded note.  hits: {hit_note_ids}"
    )

    # ---- get(old_id) must still succeed — superseded is not deleted ----
    # Post-W2 #454: flat shape with granular kind ("observation").
    fetched_old = _tool(proc, "get", {"id": old_id})
    assert fetched_old["kind"] == "observation", (
        f"Superseded note must still be gettable via get(), got: {fetched_old}"
    )
    assert fetched_old["content"] == "SupersededContent unique_token_abc", (
        f"Superseded note content incorrect: {fetched_old}"
    )

    # ---- get(new_id) must also succeed (recoverability check) ----
    fetched_new = _tool(proc, "get", {"id": new_id})
    assert fetched_new["kind"] == "insight", (
        f"New note must be gettable via get(): {fetched_new}"
    )


# ---------------------------------------------------------------------------
# Contract 6 — Closed taxonomy errors
# ---------------------------------------------------------------------------

def test_closed_taxonomy_errors(proc: subprocess.Popen) -> None:
    """Invalid entity_kind / edge relation / note_kind error with valid values listed."""
    # ---- Invalid entity_kind ----
    err_ek = _expect_rpc_error(proc, "create", {
        "kind": "entity",
        "entity_kind": "galaxy",   # not in the 6-element closed set
        "name": "StarSystem",
    })
    assert err_ek, "Expected non-empty error for invalid entity_kind"
    assert "galaxy" in err_ek.lower() or "galaxy" in err_ek, (
        f"Error should name the offending kind 'galaxy': {err_ek!r}"
    )
    # The full closed set must be listed so agents can self-correct.
    for kind in ("concept", "document", "project", "dataset", "person", "org"):
        assert kind in err_ek, (
            f"Valid entity_kind '{kind}' missing from error message: {err_ek!r}"
        )

    # ---- Invalid edge relation ----
    # Need two entities first
    src = _tool(proc, "create", {"kind": "entity", "entity_kind": "concept", "name": "TaxSrc"})
    tgt = _tool(proc, "create", {"kind": "entity", "entity_kind": "concept", "name": "TaxTgt"})
    err_rel = _expect_rpc_error(proc, "link", {
        "source_id": src["id"],
        "target_id": tgt["id"],
        "relation": "invented_by",  # not in the 17-relation closed set
    })
    assert err_rel, "Expected non-empty error for invalid edge relation"
    assert "invented_by" in err_rel, (
        f"Error should name the offending relation 'invented_by': {err_rel!r}"
    )
    # All 17 canonical relations must be listed (ADR-002 amended by ADR-055: 15→17).
    for rel in ("contains", "part_of", "instance_of", "extends", "variant_of",
                "introduced_by", "supersedes", "derived_from", "precedes",
                "depends_on", "enables", "implements", "competes_with",
                "composed_with", "annotates", "supports", "refutes"):
        assert rel in err_rel, (
            f"Valid edge relation '{rel}' missing from error message: {err_rel!r}"
        )

    # ---- Invalid note_kind ----
    err_nk = _expect_rpc_error(proc, "create", {
        "kind": "note",
        "note_kind": "scribble",   # not in the 5-element closed set
        "content": "some content",
    })
    assert err_nk, "Expected non-empty error for invalid note_kind"
    assert "scribble" in err_nk, (
        f"Error should name the offending note_kind 'scribble': {err_nk!r}"
    )
    # All 5 note kinds must be listed.
    for nk in ("observation", "insight", "decision", "question", "reference"):
        assert nk in err_nk, (
            f"Valid note_kind '{nk}' missing from error message: {err_nk!r}"
        )

    # ---- Invalid top-level kind ----
    err_kind = _expect_rpc_error(proc, "create", {
        "kind": "blob",
        "name": "Whatever",
    })
    assert err_kind, "Expected non-empty error for invalid kind"
    assert "blob" in err_kind, (
        f"Error should name the offending kind 'blob': {err_kind!r}"
    )
    # Both valid top-level kinds must appear.
    for tk in ("entity", "note"):
        assert tk in err_kind, (
            f"Valid top-level kind '{tk}' missing from error message: {err_kind!r}"
        )


# ---------------------------------------------------------------------------
# Contract 7 — Merge semantics
# ---------------------------------------------------------------------------

def test_merge_semantics(proc: subprocess.Popen) -> None:
    """merge(into, from) rewires edges to kept entity, unions tags, from is gone after."""
    # Create entities
    kept = _tool(proc, "create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "KeptEntity",
        "tags": ["alpha", "beta"],
    })
    gone = _tool(proc, "create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "GoneEntity",
        "tags": ["beta", "gamma"],
    })
    third = _tool(proc, "create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "ThirdEntity",
    })

    # Create edges incident on "gone":
    #   third → gone (inbound edge to gone)
    #   gone → kept  (outbound edge from gone, which would become a self-loop after merge — should be dropped)
    # ADR-002: depends_on is restricted to Project/Service/Artifact endpoints, not Concept.
    # Use `enables` (valid concept-to-concept) for this contract.
    e_inbound = _tool(proc, "link", {
        "source_id": third["id"],
        "target_id": gone["id"],
        "relation": "enables",
        "weight": 0.7,
    })
    e_self_loop = _tool(proc, "link", {
        "source_id": gone["id"],
        "target_id": kept["id"],
        "relation": "extends",
        "weight": 0.5,
    })
    e_inbound_id = e_inbound["id"]
    e_self_loop_id = e_self_loop["id"]

    # ---- Execute merge ----
    summary = _tool(proc, "merge", {
        "into_id": kept["id"],
        "from_id": gone["id"],
        "strategy": "prefer_into",
    })

    # Summary must report the kept and removed IDs
    assert summary["kept_id"] == kept["id"], (
        f"kept_id mismatch: expected {kept['id']}, got {summary['kept_id']}"
    )
    assert summary["removed_id"] == gone["id"], (
        f"removed_id mismatch: expected {gone['id']}, got {summary['removed_id']}"
    )

    # ---- from_id is gone (RPC-level not-found) ----
    err_gone = _expect_rpc_error(proc, "get", {"id": gone["id"]})
    assert ("not found" in err_gone.lower()), (
        f"Merged-away entity should not be gettable: {err_gone!r}"
    )

    # ---- Inbound edge now points to kept_id (rewired) ----
    # Post-W2 #454: flat shape — edge fields at top level, kind="edge".
    rewired_edge = _tool(proc, "get", {"id": e_inbound_id})
    assert rewired_edge["kind"] == "edge", f"rewired edge not found: {rewired_edge}"
    assert rewired_edge["target_id"] == kept["id"], (
        f"Inbound edge target should be rewired to kept_id={kept['id']}, "
        f"got target_id={rewired_edge['target_id']}"
    )
    assert rewired_edge["source_id"] == third["id"], (
        f"Inbound edge source should still be third={third['id']}, "
        f"got {rewired_edge['source_id']}"
    )

    # ---- Tags are unioned on the kept entity ----
    # Post-W2 #454: granular kind ("concept") at top level.
    kept_after = _tool(proc, "get", {"id": kept["id"]})
    assert kept_after["kind"] == "concept"
    tags_after = set(kept_after.get("tags", []))
    assert "alpha" in tags_after, f"Tag 'alpha' missing after merge: {tags_after}"
    assert "beta" in tags_after, f"Tag 'beta' missing after merge: {tags_after}"
    assert "gamma" in tags_after, f"Tag 'gamma' missing after merge: {tags_after}"

    # ---- Self-loop (gone→kept, both become kept) must be dropped ----
    # The edge between kept and kept-as-from would be a self-loop and should be dropped.
    # Assert via direct get that the specific edge is gone (not just absent from list).
    err_self_loop = _expect_rpc_error(proc, "get", {"id": e_self_loop_id})
    assert "not found" in err_self_loop.lower(), (
        f"Self-loop edge should be deleted after merge, got: {err_self_loop!r}"
    )

    # Also verify no edges remain referencing the removed entity at all.
    gone_outbound = _tool(proc, "list", {"kind": "edge", "source_id": gone["id"]})
    assert gone_outbound == [], (
        f"No edges should have source_id=gone after merge, got: {gone_outbound}"
    )
    gone_inbound = _tool(proc, "list", {"kind": "edge", "target_id": gone["id"]})
    assert gone_inbound == [], (
        f"No edges should have target_id=gone after merge, got: {gone_inbound}"
    )


# ---------------------------------------------------------------------------
# Contract 8 — annotates source-must-be-note constraint (ADR-002)
# ---------------------------------------------------------------------------

def test_annotates_source_must_be_note(proc: subprocess.Popen) -> None:
    """link(source=entity, relation=annotates) must fail; link(source=note, ...) must succeed.

    ADR-002 §annotates: the source of an annotates edge must be a note.
    Attempting to use an entity as source must return a clear error message
    containing 'annotates' and 'note', and must not create any edge.

    Also verifies that deleting the annotates target (hard) removes the edge
    (cascade-delete contract from ADR-002 §annotates endpoint validation).
    """
    concept = _tool(proc, "create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "AnnotatesTarget",
    })
    another = _tool(proc, "create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "AnnotatesWrongSource",
    })

    # ---- entity → entity annotates must fail ----
    err = _expect_rpc_error(proc, "link", {
        "source_id": another["id"],
        "target_id": concept["id"],
        "relation": "annotates",
    })
    assert "note" in err.lower(), (
        f"Error must mention 'note' (ADR-002 constraint); got: {err!r}"
    )
    assert "annotates" in err.lower(), (
        f"Error must mention 'annotates' relation; got: {err!r}"
    )

    # ---- No edge must have been created ----
    edges_after = _tool(proc, "list", {"kind": "edge", "source_id": another["id"]})
    assert edges_after == [], (
        f"No edge should exist after rejected annotates link, got: {edges_after}"
    )

    # ---- note → entity annotates must succeed ----
    note = _tool(proc, "create", {
        "kind": "note",
        "note_kind": "observation",
        "content": "Observation about AnnotatesTarget",
        "salience": 0.7,
    })
    edge = _tool(proc, "link", {
        "source_id": note["id"],
        "target_id": concept["id"],
        "relation": "annotates",
        "weight": 1.0,
    })
    assert edge["relation"] == "annotates", f"Expected annotates edge, got: {edge}"
    edge_id = edge["id"]

    # Confirm via neighbors that the note appears as inbound annotates neighbor
    nbrs = _tool(proc, "neighbors", {
        "node_id": concept["id"],
        "direction": "in",
        "relations": ["annotates"],
    })
    # #148: response uses canonical "id"; "node_id" accepted as alias on input only.
    neighbor_ids = [n.get("id", "") for n in nbrs]
    assert note["id"] in neighbor_ids, (
        f"Note should appear as annotates neighbor of concept; neighbors: {neighbor_ids}"
    )

    # ---- Hard-delete the target entity cascades the annotates edge ----
    del_result = _tool(proc, "delete", {"id": concept["id"], "kind": "entity", "hard": True})
    assert del_result["deleted"] is True

    err_edge = _expect_rpc_error(proc, "get", {"id": edge_id})
    assert "not found" in err_edge.lower(), (
        f"annotates edge must be cascade-deleted when target is hard-deleted; "
        f"get returned: {err_edge!r}"
    )


# ---------------------------------------------------------------------------
# Contract 9 — Epistemic edge endpoint enforcement (ADR-055 + ADR-002)
# ---------------------------------------------------------------------------

def test_epistemic_edge_endpoints(proc: subprocess.Popen) -> None:
    """supports/refutes legal endpoint pairs are accepted; illegal ones are rejected.

    ADR-055 §"Secondary rail: Entity→Entity" (kind-restricted):
      Legal   : concept|document|dataset|artifact → concept
      Illegal : document → document  (target is not concept)
      Illegal : entity  → note       (cross-substrate, same-substrate rule)
      Illegal : note    → entity     (cross-substrate, same-substrate rule)

    ADR-055 §"Primary rail: Note→Note":
      Legal   : any note kind → any note kind (substrate-level enforcement,
                operations.rs:702)

    Source citations:
      Entity allowlist  — operations.rs:211-219, pack-kg/handlers/common.rs:431-438
      Same-substrate    — operations.rs:652-654, 702, 713-724
      Error hint text   — operations.rs:689-693
    """
    # ---- Create fixtures ----
    claim = _tool(proc, "create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "ContractClaim",
        "description": "A hypothesis under test",
    })
    doc_evidence = _tool(proc, "create", {
        "kind": "entity",
        "entity_kind": "document",
        "name": "ContractEvidencePaper",
    })
    doc_other = _tool(proc, "create", {
        "kind": "entity",
        "entity_kind": "document",
        "name": "ContractDocTarget",
    })
    finding_note = _tool(proc, "create", {
        "kind": "note",
        "note_kind": "observation",
        "content": "Experiment confirms the claim with high confidence",
    })
    hypothesis_note = _tool(proc, "create", {
        "kind": "note",
        "note_kind": "insight",
        "content": "Claim might be true",
    })

    # ---- LEGAL: document -[supports]-> concept (ADR-055 entity-form) ----
    sup_edge = _tool(proc, "link", {
        "source_id": doc_evidence["id"],
        "target_id": claim["id"],
        "relation": "supports",
        "weight": 0.85,
    })
    assert sup_edge["relation"] == "supports", (
        f"document -[supports]-> concept must succeed; got: {sup_edge}"
    )

    # Verify via neighbors(direction=in, relations=[supports]) on the claim
    nbrs = _tool(proc, "neighbors", {
        "node_id": claim["id"],
        "direction": "in",
        "relations": ["supports"],
    })
    nbr_ids = [n.get("id", "") for n in nbrs]
    assert doc_evidence["id"] in nbr_ids, (
        f"doc_evidence must appear as inbound supports neighbor of claim; "
        f"nbr_ids={nbr_ids}"
    )

    # ---- LEGAL: document -[refutes]-> concept (ADR-055 entity-form) ----
    ref_edge = _tool(proc, "link", {
        "source_id": doc_other["id"],
        "target_id": claim["id"],
        "relation": "refutes",
        "weight": 0.6,
    })
    assert ref_edge["relation"] == "refutes", (
        f"document -[refutes]-> concept must succeed; got: {ref_edge}"
    )

    # ---- LEGAL: observation -[supports]-> insight (Note→Note rail, ADR-055 primary) ----
    note_edge = _tool(proc, "link", {
        "source_id": finding_note["id"],
        "target_id": hypothesis_note["id"],
        "relation": "supports",
        "weight": 0.9,
    })
    assert note_edge["relation"] == "supports", (
        f"observation -[supports]-> insight (Note→Note) must succeed; got: {note_edge}"
    )

    # ---- ILLEGAL: document -[supports]-> document (target not concept) ----
    # Error from operations.rs:695-699: target kind "document" is not in the
    # allowlist for supports; only "concept" is a valid entity-form target.
    err_doc_doc = _expect_rpc_error(proc, "link", {
        "source_id": doc_evidence["id"],
        "target_id": doc_other["id"],
        "relation": "supports",
    })
    assert err_doc_doc, "document -[supports]-> document must be rejected"
    assert "allowlist" in err_doc_doc or "concept" in err_doc_doc, (
        f"rejection error must mention 'allowlist' or 'concept'; got: {err_doc_doc!r}"
    )

    # ---- ILLEGAL: entity -[supports]-> note (cross-substrate, ADR-055 same-substrate rule) ----
    # operations.rs:713-716: entity→note cross-substrate is explicitly rejected.
    err_cross_en = _expect_rpc_error(proc, "link", {
        "source_id": doc_evidence["id"],
        "target_id": finding_note["id"],
        "relation": "supports",
    })
    assert err_cross_en, "entity -[supports]-> note (cross-substrate) must be rejected"
    assert "substrate" in err_cross_en or "note" in err_cross_en or "entity" in err_cross_en, (
        f"rejection error must mention substrate mismatch; got: {err_cross_en!r}"
    )

    # ---- ILLEGAL: note -[refutes]-> entity (cross-substrate, ADR-055 same-substrate rule) ----
    # operations.rs:719-723: note→entity cross-substrate is explicitly rejected.
    err_cross_ne = _expect_rpc_error(proc, "link", {
        "source_id": finding_note["id"],
        "target_id": claim["id"],
        "relation": "refutes",
    })
    assert err_cross_ne, "note -[refutes]-> entity (cross-substrate) must be rejected"
    assert "substrate" in err_cross_ne or "note" in err_cross_ne or "entity" in err_cross_ne, (
        f"rejection error must mention substrate mismatch; got: {err_cross_ne!r}"
    )


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    assert os.path.exists(BINARY), (
        f"Binary not found at {BINARY!r}.\n"
        f"Build with: cd crates && cargo build --release -p kkernel"
    )
    print(f"Binary: {BINARY}")
    print()

    tests = [
        ("namespace_isolation", test_namespace_isolation),
        ("short_uuid_prefix_resolution", test_short_uuid_prefix_resolution),
        ("gql_property_projection", test_gql_property_projection),
        ("edge_cascade_hard_delete", test_edge_cascade_hard_delete),
        ("note_supersession", test_note_supersession),
        ("closed_taxonomy_errors", test_closed_taxonomy_errors),
        ("merge_semantics", test_merge_semantics),
        ("annotates_source_must_be_note", test_annotates_source_must_be_note),
        ("epistemic_edge_endpoints", test_epistemic_edge_endpoints),
    ]

    for name, fn in tests:
        _run_test(name, fn)

    print()
    passes = sum(1 for _, ok, _ in _results if ok)
    fails = len(_results) - passes
    print(f"Results: {passes}/{len(_results)} passed")

    if fails:
        print("\nFailure details:")
        for name, ok, detail in _results:
            if not ok:
                print(f"\n{'='*60}")
                print(f"FAILED: {name}")
                print(detail)

    return 0 if fails == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
