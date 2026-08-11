import { describe, expect, it } from "vitest";

import { settleGraphLayout } from "@/lib/graph-layout";
import { atlasReviewFixture } from "@/lib/fixtures/atlas-review";

describe("settleGraphLayout", () => {
  it("settles the golden review graph independently of input order", () => {
    const nodes = atlasReviewFixture.graph.nodes.items;
    const edges = atlasReviewFixture.graph.edges.items;
    const settled = settleGraphLayout(nodes, edges).map(({ id, x, y }) => ({
      id,
      x,
      y,
    }));

    expect(
      settleGraphLayout([...nodes].reverse(), [...edges].reverse()).map(
        ({ id, x, y }) => ({ id, x, y }),
      ),
    )
      .toEqual(settled);
    expect(settled).toMatchInlineSnapshot(`
      [
        {
          "id": "7a66357c-8bd4-4fb8-b822-8ded7232ab31",
          "x": 80.826,
          "y": 81.519,
        },
        {
          "id": "a1f00000-0000-4000-8000-000000000001",
          "x": 62.062,
          "y": 43.805,
        },
        {
          "id": "a1f00000-0000-4000-8000-000000000002",
          "x": 86.438,
          "y": 11.261,
        },
        {
          "id": "c2d00000-0000-4000-8000-000000000030",
          "x": 10,
          "y": 75.31,
        },
        {
          "id": "d0b00000-0000-4000-8000-000000000021",
          "x": 20.265,
          "y": 36.226,
        },
      ]
    `);
  });
});
