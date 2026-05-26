"""Regression tests for memories_corpus_v2.json eval corpus integrity.

ADR: ADR-021
section: Recall evaluation corpus; salience weighting; query-type coverage

These tests verify:
  1. Schema correctness — every query.expected_top_k references a real memory ID.
  2. Query type coverage — each type appears at least N times.
  3. Distractor count — documented and within acceptable range.
  4. Domain coverage — at least 5 distinct domains are present.
  5. Memory ID uniqueness — no duplicate corpus IDs.
  6. expected_excluded integrity — all excluded IDs also reference real memories.

These are pure data tests; they require no MCP server or binary.
"""

from __future__ import annotations

import json
from collections import Counter
from pathlib import Path

import pytest

# Manifest contract — declares which verbs the corpus is calibrated against.
# The corpus drives memory.recall scoring evaluation; these are the verbs that
# read the salience signal this corpus tests.
VERBS_UNDER_TEST = {"memory.recall", "memory.remember"}

_HERE = Path(__file__).parent
_FIXTURES = _HERE.parent / "fixtures"
_V2_CORPUS = _FIXTURES / "memories_corpus_v2.json"

# Minimum occurrences required for each query_type in the corpus
_MIN_QUERY_TYPE_COUNT = {
    "synonym": 5,
    "partial": 5,
    "importance_trap": 4,
}

# Acceptable distractor ratio range [min_fraction, max_fraction]
_DISTRACTOR_RATIO_MIN = 0.20
_DISTRACTOR_RATIO_MAX = 0.50

# Minimum number of distinct non-distractor domains
_MIN_DOMAINS = 5

# Minimum total memories and queries
_MIN_MEMORIES = 150
_MIN_QUERIES = 40


@pytest.fixture(scope="module")
def corpus_data() -> dict:
    if not _V2_CORPUS.exists():
        pytest.skip(f"v2 corpus not found at {_V2_CORPUS}")
    return json.loads(_V2_CORPUS.read_text())


@pytest.fixture(scope="module")
def memory_ids(corpus_data: dict) -> set[str]:
    return {m["id"] for m in corpus_data["memories"]}


@pytest.fixture(scope="module")
def memories(corpus_data: dict) -> list[dict]:
    return corpus_data["memories"]


@pytest.fixture(scope="module")
def eval_queries(corpus_data: dict) -> list[dict]:
    return corpus_data["eval_queries"]


# ---------------------------------------------------------------------------
# Scale tests
# ---------------------------------------------------------------------------


def test_memory_count(memories: list[dict]) -> None:
    """Corpus must have at least MIN_MEMORIES memories."""
    assert len(memories) >= _MIN_MEMORIES, (
        f"Expected at least {_MIN_MEMORIES} memories, got {len(memories)}"
    )


def test_query_count(eval_queries: list[dict]) -> None:
    """Corpus must have at least MIN_QUERIES eval queries."""
    assert len(eval_queries) >= _MIN_QUERIES, (
        f"Expected at least {_MIN_QUERIES} eval queries, got {len(eval_queries)}"
    )


# ---------------------------------------------------------------------------
# Memory ID integrity
# ---------------------------------------------------------------------------


def test_memory_ids_unique(memories: list[dict]) -> None:
    """All memory IDs must be unique."""
    ids = [m["id"] for m in memories]
    duplicates = [mid for mid, count in Counter(ids).items() if count > 1]
    assert not duplicates, f"Duplicate memory IDs found: {duplicates}"


def test_memory_ids_have_id_field(memories: list[dict]) -> None:
    """Every memory must have an 'id' field."""
    missing = [i for i, m in enumerate(memories) if "id" not in m]
    assert not missing, f"Memories at indices {missing} are missing 'id' field"


def test_memory_required_fields(memories: list[dict]) -> None:
    """Every memory must have the required fields."""
    required = {"id", "content", "salience", "decay_factor", "memory_type"}
    for m in memories:
        missing = required - m.keys()
        assert not missing, f"Memory {m.get('id', '?')} missing fields: {missing}"


# ---------------------------------------------------------------------------
# Query schema tests
# ---------------------------------------------------------------------------


def test_query_ids_unique(eval_queries: list[dict]) -> None:
    """All query IDs must be unique."""
    ids = [q["id"] for q in eval_queries]
    duplicates = [qid for qid, count in Counter(ids).items() if count > 1]
    assert not duplicates, f"Duplicate query IDs: {duplicates}"


def test_queries_have_expected_top_k(eval_queries: list[dict]) -> None:
    """Every eval query must have expected_top_k (v2 schema marker)."""
    missing = [q.get("id", i) for i, q in enumerate(eval_queries) if "expected_top_k" not in q]
    assert not missing, f"Queries missing expected_top_k: {missing}"


def test_queries_have_query_type(eval_queries: list[dict]) -> None:
    """Every eval query must have a query_type field."""
    missing = [q.get("id", i) for i, q in enumerate(eval_queries) if "query_type" not in q]
    assert not missing, f"Queries missing query_type: {missing}"


# ---------------------------------------------------------------------------
# Cross-reference integrity
# ---------------------------------------------------------------------------


def test_expected_top_k_references_real_ids(
    eval_queries: list[dict], memory_ids: set[str]
) -> None:
    """Every ID in expected_top_k must correspond to a real memory."""
    errors: list[str] = []
    for q in eval_queries:
        for mid in q.get("expected_top_k", []):
            if mid not in memory_ids:
                errors.append(f"Query {q['id']}: expected_top_k references unknown memory {mid!r}")
    assert not errors, "\n".join(errors)


def test_expected_excluded_references_real_ids(
    eval_queries: list[dict], memory_ids: set[str]
) -> None:
    """Every ID in expected_excluded must correspond to a real memory."""
    errors: list[str] = []
    for q in eval_queries:
        for mid in q.get("expected_excluded", []):
            if mid not in memory_ids:
                errors.append(
                    f"Query {q['id']}: expected_excluded references unknown memory {mid!r}"
                )
    assert not errors, "\n".join(errors)


def test_no_overlap_between_top_k_and_excluded(eval_queries: list[dict]) -> None:
    """A memory cannot appear in both expected_top_k and expected_excluded."""
    errors: list[str] = []
    for q in eval_queries:
        top_k = set(q.get("expected_top_k", []))
        excluded = set(q.get("expected_excluded", []))
        overlap = top_k & excluded
        if overlap:
            errors.append(f"Query {q['id']}: IDs in both top_k and excluded: {overlap}")
    assert not errors, "\n".join(errors)


# ---------------------------------------------------------------------------
# Query type coverage
# ---------------------------------------------------------------------------


def test_query_type_minimum_counts(eval_queries: list[dict]) -> None:
    """Each required query_type must appear at least MIN_QUERY_TYPE_COUNT times."""
    type_counts = Counter(q.get("query_type", "unknown") for q in eval_queries)
    errors: list[str] = []
    for qtype, min_count in _MIN_QUERY_TYPE_COUNT.items():
        actual = type_counts.get(qtype, 0)
        if actual < min_count:
            errors.append(
                f"query_type={qtype!r}: expected >= {min_count} queries, got {actual}"
            )
    assert not errors, "\n".join(errors)


def test_query_type_distribution(eval_queries: list[dict]) -> None:
    """Report query type distribution (informational — never fails)."""
    type_counts = Counter(q.get("query_type", "unknown") for q in eval_queries)
    # Just verify types are a non-empty set — actual values tested above
    assert len(type_counts) >= 1


# ---------------------------------------------------------------------------
# Distractor ratio
# ---------------------------------------------------------------------------


def test_distractor_ratio(memories: list[dict]) -> None:
    """Distractor memories should comprise between MIN and MAX of the corpus."""
    distractors = [m for m in memories if m.get("domain") == "distractor"]
    ratio = len(distractors) / len(memories)
    assert _DISTRACTOR_RATIO_MIN <= ratio <= _DISTRACTOR_RATIO_MAX, (
        f"Distractor ratio {ratio:.2%} outside expected range "
        f"[{_DISTRACTOR_RATIO_MIN:.0%}, {_DISTRACTOR_RATIO_MAX:.0%}]. "
        f"Got {len(distractors)} distractors out of {len(memories)} memories."
    )


# ---------------------------------------------------------------------------
# Domain coverage
# ---------------------------------------------------------------------------


def test_domain_coverage(memories: list[dict]) -> None:
    """At least MIN_DOMAINS distinct non-distractor domains must be present."""
    non_distractor_domains = {
        m.get("domain", "unknown")
        for m in memories
        if m.get("domain") != "distractor"
    }
    assert len(non_distractor_domains) >= _MIN_DOMAINS, (
        f"Expected at least {_MIN_DOMAINS} distinct domains, "
        f"found {len(non_distractor_domains)}: {sorted(non_distractor_domains)}"
    )


def test_domain_field_present(memories: list[dict]) -> None:
    """Every memory should have a 'domain' field (informational integrity check)."""
    missing = [m["id"] for m in memories if "domain" not in m]
    assert not missing, f"Memories missing 'domain' field: {missing}"


# ---------------------------------------------------------------------------
# expected_top_k non-empty
# ---------------------------------------------------------------------------


def test_expected_top_k_non_empty(eval_queries: list[dict]) -> None:
    """Every query must have at least one expected_top_k entry."""
    empty = [q["id"] for q in eval_queries if not q.get("expected_top_k")]
    assert not empty, f"Queries with empty expected_top_k: {empty}"
