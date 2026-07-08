"""Note kind taxonomy contract tests.

ADR: ADR-013 (file named adr_019 per play specification; ADR drift documented in README)
section: Base taxonomy; Default kind; Kind is a string validated; Search and discrimination;
         Supersession via edge
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveMcpSession
from khive_contract.fixtures import NOTE_KINDS

VERBS_UNDER_TEST = {"create", "list", "get", "search", "link"}

# Runtime-confirmed note kinds (5 base kinds) — canonical set lives in
# khive_contract.fixtures.NOTE_KINDS; sorted here for a deterministic
# parametrize order.
RUNTIME_NOTE_KINDS = tuple(sorted(NOTE_KINDS))


@pytest.mark.adr_013
@pytest.mark.slow
@pytest.mark.parametrize("note_kind", RUNTIME_NOTE_KINDS)
def test_create_list_get_each_base_note_kind(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_note,
    note_kind: str,
) -> None:
    """Create, list-filtered, and get each of the 5 base note kinds.

    ADR: ADR-013
    section: Base taxonomy

    Each create returns a kind; list filtered by note_kind contains the id;
    get returns kind=="note" wrapper with matching content.
    """
    args = sample_note(
        note_kind=note_kind,
        content=f"content for {note_kind} note",
        salience=0.6,
    )
    result = khive_session.verb("create", args)
    assert result is not None, f"create(note_kind={note_kind}) returned None"
    note_id = result.get("id")
    assert note_id, f"create note missing 'id': {result}"
    # Runtime response uses 'kind' field for note_kind value
    assert result.get("kind") == note_kind, (
        f"kind mismatch: got {result.get('kind')!r}, expected {note_kind!r}"
    )

    # list filtered by note_kind must include the new id
    listed = khive_session.verb("list", {
        "kind": "note",
        "note_kind": note_kind,
        "namespace": temp_namespace,
    })
    assert isinstance(listed, list), f"list returned non-list: {listed!r}"
    ids = [n.get("id") for n in listed]
    assert note_id in ids, (
        f"list(note_kind={note_kind}) omitted id={note_id}; got {ids}"
    )

    # Per P-H2 (ADR-045): get returns flat object with granular kind at top —
    # no {data: ...} wrapper, same shape as create/list.
    fetched = khive_session.verb("get", {"id": note_id, "namespace": temp_namespace})
    assert fetched is not None
    assert "data" not in fetched, (
        f"get must NOT wrap in {{data: ...}} (P-H2); got: {fetched}"
    )
    assert fetched.get("kind") == note_kind, (
        f"get kind should be granular {note_kind!r}, got {fetched.get('kind')!r}"
    )
    assert fetched.get("content") == f"content for {note_kind} note", (
        f"get content mismatch: {fetched}"
    )


@pytest.mark.adr_013
@pytest.mark.slow
def test_invalid_note_kind_reports_registered_set(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Invalid note_kind returns per-op error that names the offending kind and lists valid set.

    ADR: ADR-013
    section: Kind is a string validated

    Ports test_closed_taxonomy_errors note_kind check.
    """
    envelope = khive_session.request_batch([
        {"tool": "create", "args": {
            "kind": "note",
            "note_kind": "scribble",
            "content": "some content",
            "namespace": temp_namespace,
        }}
    ])
    results = envelope.get("results", [])
    assert results, "Expected results in envelope"
    first = results[0]
    assert not first.get("ok", False), "Expected per-op error for invalid note_kind"
    err = first.get("error", "")
    assert err, "Error message must be non-empty"
    assert "scribble" in err, f"Error must name offending note_kind 'scribble': {err!r}"

    # All 5 base note kinds must be listed
    for nk in RUNTIME_NOTE_KINDS:
        assert nk in err, (
            f"Valid note_kind '{nk}' missing from error message: {err!r}"
        )


@pytest.mark.adr_013
@pytest.mark.slow
def test_note_supersession_search_excludes_old_but_get_keeps_both(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_note,
) -> None:
    """Superseded note excluded from search results but still gettable via get().

    ADR: ADR-013
    section: Supersession via edge

    Ports test_note_supersession from contract_test.py.
    """
    old_note = khive_session.verb("create", sample_note(
        note_kind="observation",
        content="SupersededContent unique_token_abc_ns",
        salience=0.8,
    ))
    old_id = old_note["id"]

    new_note = khive_session.verb("create", sample_note(
        note_kind="insight",
        content="NewerContent unique_token_abc_ns",
        salience=0.9,
    ))
    new_id = new_note["id"]

    # Wire supersedes edge: new → old
    khive_session.verb("link", {
        "source_id": new_id,
        "target_id": old_id,
        "relation": "supersedes",
        "weight": 1.0,
        "namespace": temp_namespace,
    })

    # search must exclude superseded old note and include new note
    hits = khive_session.verb("search", {
        "kind": "note",
        "query": "unique_token_abc_ns",
        "limit": 20,
        "namespace": temp_namespace,
    })
    hit_ids = [h.get("id", "") for h in hits]

    assert old_id not in hit_ids, (
        f"Superseded note (old_id={old_id}) should be excluded from search, "
        f"but appeared in hits: {hit_ids}"
    )
    assert new_id in hit_ids, (
        f"New note (new_id={new_id}) must appear in search results; hits: {hit_ids}"
    )

    # Per P-H2 (ADR-045): get returns flat object — no {data: ...} wrapper.
    # get(old_id) must still succeed — superseded is not deleted
    fetched_old = khive_session.verb("get", {"id": old_id, "namespace": temp_namespace})
    assert fetched_old.get("kind") == "observation", (
        f"Superseded note must still be gettable via get(), got: {fetched_old}"
    )
    assert fetched_old.get("content") == "SupersededContent unique_token_abc_ns"

    # get(new_id) must also succeed
    fetched_new = khive_session.verb("get", {"id": new_id, "namespace": temp_namespace})
    assert fetched_new.get("kind") == "insight"


@pytest.mark.adr_013
@pytest.mark.slow
def test_note_salience_stored_and_retrievable(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_note,
) -> None:
    """Note salience is stored and returned by get.

    ADR: ADR-013
    section: Base taxonomy
    """
    args = sample_note(note_kind="observation", salience=0.75)
    result = khive_session.verb("create", args)
    note_id = result["id"]

    # Per P-H2 (ADR-045): get returns flat object — no {data: ...} wrapper.
    fetched = khive_session.verb("get", {"id": note_id, "namespace": temp_namespace})
    salience = fetched.get("salience")
    assert salience is not None, f"salience not stored: {fetched}"
    assert abs(salience - 0.75) < 0.01, f"salience value mismatch: {salience}"
