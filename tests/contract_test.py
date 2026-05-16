#!/usr/bin/env python3
"""Behavioral-contract tests for the khive-mcp binary (GitHub issue #21).

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

How to run
----------
    python3 tests/contract_test.py

Each test function is named `test_<contract>` and prints [pass] / [FAIL].
Exit code is 0 if every test passes, 1 if any fail.

The KHIVE_MCP_BINARY env var overrides the default binary path.
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
    "KHIVE_MCP_BINARY",
    os.path.join(
        os.path.dirname(__file__), "..", "crates", "target", "release", "khive-mcp"
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


def _tool_raw(proc: subprocess.Popen, name: str, args: dict) -> dict:
    """Call a tool and return the raw response dict.

    Returns {"_rpc_error": {...}} if the server replied with a JSON-RPC error
    (code -32xxx) rather than a tool result.  Callers that want to inspect RPC
    errors without raising can check for "_rpc_error" in the return value.
    """
    _send(proc, "tools/call", {"name": name, "arguments": args})
    resp = _recv(proc)
    if "error" in resp:
        # Surface as a sentinel dict so callers can inspect the message.
        return {"_rpc_error": resp["error"]}
    return resp.get("result", {})


def _tool(proc: subprocess.Popen, name: str, args: dict) -> Any:
    """Call a tool; raise on any error; parse and return JSON payload."""
    result = _tool_raw(proc, name, args)
    if "_rpc_error" in result:
        raise RuntimeError(f"MCP-level error calling {name}: {result['_rpc_error']}")
    if result.get("isError"):
        content = result.get("content", [])
        text = content[0]["text"] if content else "(no text)"
        raise RuntimeError(f"Tool '{name}' returned error: {text}")
    content = result.get("content", [])
    text = content[0]["text"] if content else ""
    return json.loads(text) if text else None


def _expect_rpc_error(proc: subprocess.Popen, name: str, args: dict) -> str:
    """Call a tool; assert it returns a JSON-RPC-level error (code -32xxx).

    khive-mcp returns McpError::invalid_params (or invalid_request) uniformly
    for all validation failures — invalid kinds, unknown properties, malformed
    IDs, and not-found lookups all surface as RPC-level errors, never as tool-
    level isError responses.  This helper enforces that contract precisely so
    that a future regression (e.g. switching to isError) is caught immediately.

    Returns the RPC error message string so callers can assert on its content.
    """
    result = _tool_raw(proc, name, args)

    if "_rpc_error" in result:
        err = result["_rpc_error"]
        return err.get("message", str(err))

    # If we got a tool-level isError, that is a contract deviation — fail hard.
    if result.get("isError"):
        content = result.get("content", [])
        text = content[0]["text"] if content else ""
        raise AssertionError(
            f"Tool '{name}' returned tool-level isError instead of the expected "
            f"RPC-level error (McpError::invalid_params).  This is a contract "
            f"deviation — khive-mcp should surface validation errors as JSON-RPC "
            f"errors, not as isError tool results.  Text: {text!r}"
        )

    # Success — also a failure
    content = result.get("content", [])
    text = content[0]["text"] if content else ""
    raise AssertionError(
        f"Expected tool '{name}' to return an RPC-level error but got success:\n{text}"
    )


def _tool_expect_error(proc: subprocess.Popen, name: str, args: dict) -> str:
    """Alias retained for call-sites that need channel-agnostic behaviour.

    For new call-sites, prefer _expect_rpc_error() which asserts the specific
    channel (RPC-level) that khive-mcp uses for all validation failures.
    """
    return _expect_rpc_error(proc, name, args)


# ---------------------------------------------------------------------------
# Server lifecycle
# ---------------------------------------------------------------------------

def _start_server(db_path: str) -> subprocess.Popen:
    """Spawn a fresh khive-mcp process backed by a temp SQLite file."""
    proc = subprocess.Popen(
        [BINARY, "--db", db_path, "--no-embed", "--log", "error"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
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
    """Entity in namespace A must be invisible via get/search/list from namespace B."""
    # Create entity in namespace "ns-alpha"
    entity = _tool(proc, "create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "AlphaEntity",
        "description": "Only visible in ns-alpha",
        "namespace": "ns-alpha",
    })
    full_id = entity["id"]
    short_prefix = full_id[:8]

    # ---- get from ns-beta must not find it (RPC-level error) ----
    err_text = _expect_rpc_error(proc, "get", {"id": full_id, "namespace": "ns-beta"})
    assert "not found" in err_text.lower(), (
        f"Expected 'not found' error from ns-beta get, got: {err_text!r}"
    )

    # ---- list from ns-beta must return empty ----
    entities_beta = _tool(proc, "list", {
        "kind": "entity",
        "entity_kind": "concept",
        "namespace": "ns-beta",
    })
    ids_beta = [e["id"] for e in entities_beta]
    assert full_id not in ids_beta, (
        f"AlphaEntity full_id appeared in ns-beta list: {ids_beta}"
    )

    # ---- search from ns-beta must not contain it ----
    hits_beta = _tool(proc, "search", {
        "kind": "entity",
        "query": "AlphaEntity",
        "namespace": "ns-beta",
    })
    hit_ids_beta = [h.get("entity_id", "") for h in hits_beta]
    assert full_id not in hit_ids_beta, (
        f"AlphaEntity appeared in ns-beta search hits: {hit_ids_beta}"
    )

    # ---- get from ns-alpha must succeed ----
    fetched = _tool(proc, "get", {"id": full_id, "namespace": "ns-alpha"})
    assert fetched["kind"] == "entity", f"Expected kind=entity, got {fetched['kind']}"
    assert fetched["data"]["name"] == "AlphaEntity"

    # ---- short prefix from ns-beta must not resolve to the entity (RPC-level error) ----
    err_prefix = _expect_rpc_error(proc, "get", {"id": short_prefix, "namespace": "ns-beta"})
    # The error should say "no record matches" rather than returning the entity from ns-alpha
    assert ("no record" in err_prefix.lower() or "not found" in err_prefix.lower()), (
        f"Expected prefix-not-found from ns-beta, got: {err_prefix!r}"
    )


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
    fetched = _tool(proc, "get", {"id": prefix8})
    assert fetched["kind"] == "entity"
    assert fetched["data"]["name"] == "PrefixTarget", (
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
    rows = _tool(proc, "query", {
        "query": "MATCH (a:concept)-[e:extends]->(b:concept) RETURN a.name, b.name LIMIT 10",
    })
    assert isinstance(rows, list) and len(rows) >= 1, (
        f"Expected >=1 rows for valid projection, got: {rows}"
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
    assert "Valid: id, name, kind, namespace, description, properties, created_at, updated_at" in err_text, (
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
    e2 = _tool(proc, "link", {
        "source_id": spoke2["id"], "target_id": hub["id"], "relation": "depends_on",
    })
    e1_id = e1["id"]
    e2_id = e2["id"]

    # Verify edges exist before delete
    edges_before = _tool(proc, "list", {"kind": "edge", "source_id": hub["id"]})
    assert any(e["id"] == e1_id for e in edges_before), (
        "outbound edge from hub not listed before hard-delete"
    )

    # Hard-delete the hub
    del_result = _tool(proc, "delete", {"id": hub["id"], "hard": True})
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

    del_soft = _tool(proc, "delete", {"id": hub_soft["id"]})  # hard=False by default
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
    hit_note_ids = [h.get("note_id", "") for h in hits]

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
    fetched_old = _tool(proc, "get", {"id": old_id})
    assert fetched_old["kind"] == "note", (
        f"Superseded note must still be gettable via get(), got: {fetched_old}"
    )
    assert fetched_old["data"]["content"] == "SupersededContent unique_token_abc", (
        f"Superseded note content incorrect: {fetched_old}"
    )

    # ---- get(new_id) must also succeed (recoverability check) ----
    fetched_new = _tool(proc, "get", {"id": new_id})
    assert fetched_new["kind"] == "note", (
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
    # The error should mention at least some valid kinds so the agent can self-correct.
    # The message may come from the runtime layer as "invalid input: ..." so we
    # check the overall text (case-insensitive) for at least the unknown kind and
    # evidence of valid values.
    assert "galaxy" in err_ek.lower() or "galaxy" in err_ek, (
        f"Error should name the offending kind 'galaxy': {err_ek!r}"
    )
    valid_kind_mentioned = any(v in err_ek for v in ("concept", "document", "project", "dataset", "person", "org"))
    assert valid_kind_mentioned, (
        f"At least one valid entity_kind must be listed in error: {err_ek!r}"
    )

    # ---- Invalid edge relation ----
    # Need two entities first
    src = _tool(proc, "create", {"kind": "entity", "entity_kind": "concept", "name": "TaxSrc"})
    tgt = _tool(proc, "create", {"kind": "entity", "entity_kind": "concept", "name": "TaxTgt"})
    err_rel = _expect_rpc_error(proc, "link", {
        "source_id": src["id"],
        "target_id": tgt["id"],
        "relation": "invented_by",  # not in the 13-relation closed set
    })
    assert err_rel, "Expected non-empty error for invalid edge relation"
    assert "invented_by" in err_rel, (
        f"Error should name the offending relation 'invented_by': {err_rel!r}"
    )
    valid_rel_mentioned = any(v in err_rel for v in ("extends", "variant_of", "annotates", "introduced_by"))
    assert valid_rel_mentioned, (
        f"At least one valid edge relation must be listed in error: {err_rel!r}"
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
    valid_nk_mentioned = any(v in err_nk for v in ("observation", "insight", "decision", "question", "reference"))
    assert valid_nk_mentioned, (
        f"At least one valid note_kind must be listed in error: {err_nk!r}"
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
    valid_top_mentioned = any(v in err_kind for v in ("entity", "note"))
    assert valid_top_mentioned, (
        f"Valid top-level kinds (entity, note) must appear in error: {err_kind!r}"
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
    e_inbound = _tool(proc, "link", {
        "source_id": third["id"],
        "target_id": gone["id"],
        "relation": "depends_on",
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
    rewired_edge = _tool(proc, "get", {"id": e_inbound_id})
    assert rewired_edge["kind"] == "edge", f"rewired edge not found: {rewired_edge}"
    edge_data = rewired_edge["data"]
    assert edge_data["target_id"] == kept["id"], (
        f"Inbound edge target should be rewired to kept_id={kept['id']}, "
        f"got target_id={edge_data['target_id']}"
    )
    assert edge_data["source_id"] == third["id"], (
        f"Inbound edge source should still be third={third['id']}, "
        f"got {edge_data['source_id']}"
    )

    # ---- Tags are unioned on the kept entity ----
    kept_after = _tool(proc, "get", {"id": kept["id"]})
    assert kept_after["kind"] == "entity"
    tags_after = set(kept_after["data"].get("tags", []))
    assert "alpha" in tags_after, f"Tag 'alpha' missing after merge: {tags_after}"
    assert "beta" in tags_after, f"Tag 'beta' missing after merge: {tags_after}"
    assert "gamma" in tags_after, f"Tag 'gamma' missing after merge: {tags_after}"

    # ---- Self-loop (gone→kept, both become kept) must be dropped ----
    # The edge between kept and kept-as-from would be a self-loop and should be dropped.
    # Verify that kept entity has no self-loop outbound edge (the only outbound edge from
    # kept→itself via e_self_loop should have been dropped during merge).
    kept_outbound = _tool(proc, "list", {
        "kind": "edge",
        "source_id": kept["id"],
    })
    self_loop_ids = [e["id"] for e in kept_outbound if e["source_id"] == e["target_id"]]
    assert len(self_loop_ids) == 0, (
        f"Self-loop edges should be dropped after merge, found: {self_loop_ids}"
    )


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    assert os.path.exists(BINARY), (
        f"Binary not found at {BINARY!r}.\n"
        f"Build with: cd crates && cargo build --release -p khive-mcp"
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
