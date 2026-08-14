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
          "x": 77,
          "y": 86.327,
        },
        {
          "id": "a1f00000-0000-4000-8000-000000000001",
          "x": 64.679,
          "y": 46.634,
        },
        {
          "id": "a1f00000-0000-4000-8000-000000000002",
          "x": 77,
          "y": 10,
        },
        {
          "id": "c2d00000-0000-4000-8000-000000000030",
          "x": 23,
          "y": 79.157,
        },
        {
          "id": "d0b00000-0000-4000-8000-000000000021",
          "x": 23,
          "y": 38.517,
        },
      ]
    `);
  });

  it("keeps mobile node footprints inside the clipped stage", () => {
    const stageWidth = 300;
    const nodeHalfWidth = 60;
    const settled = settleGraphLayout(
      Array.from({ length: 20 }, (_, index) => ({ id: `node-${index}` })),
      [],
    );

    for (const node of settled) {
      const center = node.x / 100 * stageWidth;
      expect(center - nodeHalfWidth).toBeGreaterThanOrEqual(0);
      expect(center + nodeHalfWidth).toBeLessThanOrEqual(stageWidth);
    }
  });
});
