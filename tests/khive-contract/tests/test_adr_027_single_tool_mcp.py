"""Single-tool MCP surface contract tests.

ADR: ADR-027
section: MCP wire format unchanged; One MCP tool; Pack selection; Dynamic verb catalog
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveMcpSession, KhiveRpcError
from khive_contract.fixtures import KG_VERBS as _KG_VERBS

VERBS_UNDER_TEST = {"create"}

# KG verbs imported from fixtures.py — single source of truth (16 verbs).
KG_VERBS = tuple(sorted(_KG_VERBS))
GTD_VERBS = ("gtd.assign", "gtd.next", "gtd.complete", "gtd.tasks", "gtd.transition")
MEMORY_VERBS = ("memory.remember", "memory.recall")


@pytest.mark.adr_027
@pytest.mark.slow
def test_tools_list_exposes_exactly_request(
    khive_session: KhiveMcpSession,
) -> None:
    """tools/list returns exactly one tool named 'request'.

    ADR: ADR-027
    section: One MCP tool; MCP wire format unchanged

    Ports smoke single-tool assertion.
    """
    tools = khive_session.tools_list()
    tool_names = [t.get("name") for t in tools]
    assert tool_names == ["request"], (
        f"Expected exactly [request], got {tool_names}"
    )


@pytest.mark.adr_027
@pytest.mark.slow
def test_request_description_lists_kg_verbs(
    khive_session: KhiveMcpSession,
) -> None:
    """The 'request' tool description lists all KG verb names.

    ADR: ADR-027
    section: Dynamic verb catalog; ADR-016 One MCP tool

    Ports smoke verb-in-description assertion.
    """
    tools = khive_session.tools_list()
    assert tools, "tools/list returned empty"
    request_tool = next((t for t in tools if t.get("name") == "request"), None)
    assert request_tool is not None, "No 'request' tool in tools/list"

    description = request_tool.get("description") or ""
    for verb in KG_VERBS:
        assert verb in description, (
            f"KG verb '{verb}' missing from request description; got:\n{description!r}"
        )


@pytest.mark.adr_027
@pytest.mark.slow
def test_gtd_verbs_absent_from_kg_only_description(
    khive_session: KhiveMcpSession,
) -> None:
    """KG-only session description does not include GTD or memory verbs.

    ADR: ADR-027
    section: Pack selection; Dynamic verb catalog
    """
    tools = khive_session.tools_list()
    description = tools[0].get("description") or ""
    # GTD verbs must not appear in KG-only description
    for verb in GTD_VERBS:
        assert verb not in description, (
            f"GTD verb '{verb}' should not appear in KG-only description; "
            f"got:\n{description!r}"
        )


@pytest.mark.adr_027
@pytest.mark.slow
def test_gtd_session_description_includes_gtd_verbs(
    khive_gtd_session: KhiveMcpSession,
) -> None:
    """KG+GTD session description includes GTD verb names.

    ADR: ADR-027
    section: Pack selection; Dynamic verb catalog

    Ports pack smoke startup.
    """
    tools = khive_gtd_session.tools_list()
    assert tools, "tools/list returned empty for GTD session"
    description = tools[0].get("description") or ""
    for verb in GTD_VERBS:
        assert verb in description, (
            f"GTD verb '{verb}' missing from GTD session description; got:\n{description!r}"
        )


@pytest.mark.adr_027
@pytest.mark.slow
def test_memory_session_description_includes_memory_verbs(
    khive_memory_session: KhiveMcpSession,
) -> None:
    """KG+memory session description includes remember and recall.

    ADR: ADR-027
    section: Pack selection; Dynamic verb catalog
    """
    tools = khive_memory_session.tools_list()
    assert tools, "tools/list returned empty for memory session"
    description = tools[0].get("description") or ""
    for verb in MEMORY_VERBS:
        assert verb in description, (
            f"Memory verb '{verb}' missing from memory session description; "
            f"got:\n{description!r}"
        )


@pytest.mark.adr_027
@pytest.mark.slow
def test_kg_session_rejects_gtd_verb(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """KG-only session returns per-op error for GTD verbs.

    ADR: ADR-027
    section: Pack selection

    GTD verbs must not be callable when gtd pack is not loaded.
    """
    envelope = khive_session.request_batch([
        {"tool": "gtd.assign", "args": {
            "title": "test task",
            "namespace": temp_namespace,
        }}
    ])
    results = envelope.get("results", [])
    assert results, "Expected results in envelope"
    first = results[0]
    assert not first.get("ok", False), (
        "KG-only session should not allow GTD 'gtd.assign' verb"
    )


@pytest.mark.adr_027
@pytest.mark.slow
def test_unknown_pack_fails_startup() -> None:
    """Spawning with an unknown pack name fails with a clear error.

    ADR: ADR-027
    section: Dependency ordering; Boot errors

    The process must fail to initialize with a useful error message.
    """
    import subprocess
    from khive_contract.client import _resolve_binary

    binary = _resolve_binary(None)
    # The MCP server is the `mcp` subcommand of the unified kkernel binary. An
    # unknown pack must fail initialization with a non-empty error, not hang.
    proc = subprocess.Popen(
        [str(binary), "mcp", "--db", ":memory:", "--no-embed", "--log", "error",
         "--pack", "kg", "--pack", "does_not_exist"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        _, stderr = proc.communicate(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        pytest.fail("kkernel mcp with an unknown pack should fail fast, not hang")
    assert proc.returncode != 0, "unknown pack must cause a nonzero exit"
    assert stderr.strip(), "startup failure must produce a non-empty error message"
