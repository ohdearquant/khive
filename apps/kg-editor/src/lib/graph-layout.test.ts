import { describe, expect, it } from "vitest";

import { createHash } from "node:crypto";

import {
  deduplicateSettledPositions,
  settleGraphLayout,
} from "@/lib/graph-layout";
import { demoReviewFixture } from "@/lib/fixtures/demo-review";

describe("settleGraphLayout", () => {
  it("settles the golden review graph independently of input order", () => {
    const nodes = demoReviewFixture.graph.nodes.items;
    const edges = demoReviewFixture.graph.edges.items;
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

  it("bounds pair-distance work for a schema-maximum graph", () => {
    const originalSqrt = Math.sqrt;
    const originalHypot = Math.hypot;
    let distanceEvaluations = 0;
    Math.sqrt = (value) => {
      distanceEvaluations += 1;
      return originalSqrt(value);
    };
    Math.hypot = (...values) => {
      distanceEvaluations += 1;
      return originalHypot(...values);
    };

    try {
      settleGraphLayout(
        Array.from({ length: 200 }, (_, index) => ({ id: `node-${index}` })),
        [],
      );
    } finally {
      Math.sqrt = originalSqrt;
      Math.hypot = originalHypot;
    }

    expect(distanceEvaluations).toBeLessThanOrEqual(400_000);
  }, 15_000);

  it("never settles two nodes onto the same point at the schema maximum", () => {
    const settled = settleGraphLayout(
      Array.from({ length: 200 }, (_, index) => ({ id: `node-${index}` })),
      [],
    );

    const occupied = new Set<string>();
    for (const { x, y } of settled) {
      const key = `${x},${y}`;
      expect(occupied.has(key)).toBe(false);
      occupied.add(key);
    }

    // None of these 200 points ever exhausts the golden-angle spiral, so a
    // digest of the exact output pins the common-case layout: any change to
    // the spiral placement, the force settle or the de-duplication order
    // moves it, while the fallback path stays covered by the test below.
    const digest = createHash("sha256")
      .update(JSON.stringify(settled.map(({ id, x, y }) => [id, x, y])))
      .digest("hex");
    expect(digest).toBe(
      "541b45033bdf2eddbd44fc704c69270eeb7614a19b35297279dbeb9e3cc143aa",
    );
  });

  it("falls back to a free stage coordinate when all spiral nudges are already occupied", () => {
    const targetIndex = 65;
    const goldenAngle = Math.PI * (3 - Math.sqrt(5));
    const horizontalPadding = 23;
    const verticalPadding = 10;
    const clampX = (value: number) =>
      Math.min(100 - horizontalPadding, Math.max(horizontalPadding, value));
    const clampY = (value: number) =>
      Math.min(100 - verticalPadding, Math.max(verticalPadding, value));

    // Replay the production spiral formula to build a walk of 64 cumulative
    // nudges starting from the same coordinate the target point will start
    // from, so every candidate the target tries during its own de-duplication
    // pass is already occupied by an earlier point.
    const spiralWalk: { x: number; y: number }[] = [{ x: 50, y: 50 }];
    for (let attempt = 1; attempt <= 64; attempt += 1) {
      const previous = spiralWalk[spiralWalk.length - 1];
      const angle = targetIndex * goldenAngle * 7 + attempt * goldenAngle;
      const radius = 0.25 * attempt;
      spiralWalk.push({
        x: clampX(previous.x + Math.cos(angle) * radius),
        y: clampY(previous.y + Math.sin(angle) * radius),
      });
    }

    const seed = spiralWalk[0];
    const points = [
      seed, // index 0: occupies the coordinate the target starts from
      ...spiralWalk.slice(1), // indices 1-64: occupy every spiral candidate
      seed, // index 65 === targetIndex: starts on an already-occupied point
    ];
    expect(points.length - 1).toBe(targetIndex);

    const deduped = deduplicateSettledPositions(points);

    const keys = deduped.map(({ x, y }) => `${x},${y}`);
    expect(new Set(keys).size).toBe(keys.length);
  });
});
