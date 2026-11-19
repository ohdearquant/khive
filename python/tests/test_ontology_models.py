import pytest
from pydantic import ValidationError

from khive.models import Edge, EdgeRelation


def test_ownership_relation_round_trips_through_edge_model():
    assert EdgeRelation("owns") is EdgeRelation.owns
    edge = Edge.model_validate({"id": "holding", "kind": "owns", "members": []})
    assert edge.kind is EdgeRelation.owns
    assert edge.model_dump(mode="json")["kind"] == "owns"


def test_unknown_relation_is_still_rejected():
    with pytest.raises(ValidationError):
        Edge.model_validate({"id": "holding", "kind": "owned_by", "members": []})
