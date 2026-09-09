import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { OntologyLegend } from "@/components/ontology-mark";
import {
  EDGE_RELATIONS,
  ENTITY_KINDS,
  NOTE_KINDS,
} from "@/lib/ontology-legend";

describe("OntologyLegend", () => {
  it("renders the complete closed vocabulary by default", () => {
    const { container } = render(<OntologyLegend />);

    const entityMarks = ENTITY_KINDS.map((kind) =>
      container.querySelector(`.ontology-kind-mark[data-kind="${kind}"]`)
    );
    const noteMarks = NOTE_KINDS.map((kind) =>
      container.querySelector(`.ontology-kind-mark[data-kind="${kind}"]`)
    );
    const relationMarks = EDGE_RELATIONS.map((relation) =>
      container.querySelector(
        `.ontology-relation-mark[data-relation="${relation}"]`,
      )
    );

    // Derived from the vocabularies themselves: the claim is that every
    // member renders a mark, which a literal restates and then goes stale on
    // the next member. A missing mark still fails, because the query for it
    // returns null and drops out of the filter.
    expect(entityMarks.filter(Boolean)).toHaveLength(ENTITY_KINDS.length);
    expect(noteMarks.filter(Boolean)).toHaveLength(NOTE_KINDS.length);
    expect(relationMarks.filter(Boolean)).toHaveLength(EDGE_RELATIONS.length);
    expect(container.querySelectorAll(".ontology-derived-mark")).toHaveLength(
      1,
    );
    expect(screen.getByLabelText("Ontology legend")).toHaveTextContent(
      "Derived",
    );
  });

  it("dims identities absent from the current graph without removing them", () => {
    const { container } = render(
      <OntologyLegend
        presentEntityKinds={["concept"]}
        presentRelations={["contains"]}
      />,
    );

    expect(
      container.querySelector('.ontology-kind-mark[data-kind="concept"]'),
    ).not.toHaveClass("ontology-mark-dim");
    expect(
      container.querySelector('.ontology-kind-mark[data-kind="document"]'),
    ).toHaveClass("ontology-mark-dim");
    expect(
      container.querySelector(
        '.ontology-relation-mark[data-relation="contains"]',
      ),
    ).not.toHaveClass("ontology-mark-dim");
    expect(
      container.querySelector(
        '.ontology-relation-mark[data-relation="depends_on"]',
      ),
    ).toHaveClass("ontology-mark-dim");
    expect(
      container.querySelectorAll(".ontology-kind-mark[data-kind]"),
    ).toHaveLength(ENTITY_KINDS.length + NOTE_KINDS.length);
  });
});
