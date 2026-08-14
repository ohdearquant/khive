import { describe, expect, it } from "vitest";

import { edgeDirectionMark } from "@/components/ontology-mark";
import { edgeLegendFor } from "@/lib/ontology-legend";

describe("ontology marks", () => {
  it("keeps direction cues in the visible edge span", () => {
    const mark = edgeDirectionMark(
      edgeLegendFor("supports"),
      { x: 0, y: 0 },
      { x: 100, y: 50 },
    );

    expect(mark).toMatchObject({ x: 68, y: 34 });
  });
});
