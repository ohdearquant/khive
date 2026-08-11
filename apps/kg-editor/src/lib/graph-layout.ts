import { GRAPH_LAYOUT_SEED } from "@/lib/ontology-legend";

type GraphLayoutNode = Readonly<{ id: string }>;
type GraphLayoutEdge = Readonly<{
  id: string;
  source: string;
  target: string;
}>;

export type SettledGraphNode<T extends GraphLayoutNode> =
  & T
  & Readonly<{
    x: number;
    y: number;
  }>;

// The narrowest supported graph stage is 300 px after mobile workspace
// gutters. Mobile node cards can be 120 px wide, so their centers need a
// 20% horizontal inset to keep the complete card inside the clipped stage.
const HORIZONTAL_PADDING = 20;
const VERTICAL_PADDING = 10;
const CENTER = 50;
const ITERATIONS = 160;

function compareText(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

function compareEdges(left: GraphLayoutEdge, right: GraphLayoutEdge): number {
  return compareText(left.id, right.id) ||
    compareText(left.source, right.source) ||
    compareText(left.target, right.target);
}

function seededRandom(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state += 0x6d2b_79f5;
    let value = state;
    value = Math.imul(value ^ (value >>> 15), value | 1);
    value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
    return ((value ^ (value >>> 14)) >>> 0) / 4_294_967_296;
  };
}

function rounded(value: number): number {
  return Math.round(value * 1_000) / 1_000;
}

export function settleGraphLayout<T extends GraphLayoutNode>(
  nodes: readonly T[],
  edges: readonly GraphLayoutEdge[],
): SettledGraphNode<T>[] {
  const orderedNodes = [...nodes].sort((left, right) =>
    compareText(left.id, right.id)
  );
  if (orderedNodes.length === 0) return [];
  if (orderedNodes.length === 1) {
    return [{ ...orderedNodes[0], x: CENTER, y: CENTER }];
  }

  const random = seededRandom(GRAPH_LAYOUT_SEED);
  const phase = random() * Math.PI * 2;
  const goldenAngle = Math.PI * (3 - Math.sqrt(5));
  const points = orderedNodes.map((node, index) => {
    const radius = 16 + 25 * Math.sqrt((index + 0.5) / orderedNodes.length);
    const angle = phase + index * goldenAngle + (random() - 0.5) * 0.08;
    return {
      node,
      x: CENTER + Math.cos(angle) * radius,
      y: CENTER + Math.sin(angle) * radius,
      velocityX: 0,
      velocityY: 0,
    };
  });
  const indexById = new Map(
    points.map((point, index) => [point.node.id, index]),
  );
  const orderedEdges = [...edges].sort(compareEdges).flatMap((edge) => {
    const source = indexById.get(edge.source);
    const target = indexById.get(edge.target);
    return source === undefined || target === undefined || source === target
      ? []
      : [{ source, target }];
  });

  for (let iteration = 0; iteration < ITERATIONS; iteration += 1) {
    const forceX = Array.from({ length: points.length }, () => 0);
    const forceY = Array.from({ length: points.length }, () => 0);

    for (let left = 0; left < points.length; left += 1) {
      for (let right = left + 1; right < points.length; right += 1) {
        const deltaX = points[left].x - points[right].x;
        const deltaY = points[left].y - points[right].y;
        const distanceSquared = Math.max(deltaX * deltaX + deltaY * deltaY, 1);
        const distance = Math.sqrt(distanceSquared);
        const repulsion = 720 / distanceSquared;
        const x = deltaX / distance * repulsion;
        const y = deltaY / distance * repulsion;
        forceX[left] += x;
        forceY[left] += y;
        forceX[right] -= x;
        forceY[right] -= y;
      }
    }

    for (const edge of orderedEdges) {
      const deltaX = points[edge.target].x - points[edge.source].x;
      const deltaY = points[edge.target].y - points[edge.source].y;
      const distance = Math.max(Math.hypot(deltaX, deltaY), 1);
      const attraction = (distance - 29) * 0.035;
      const x = deltaX / distance * attraction;
      const y = deltaY / distance * attraction;
      forceX[edge.source] += x;
      forceY[edge.source] += y;
      forceX[edge.target] -= x;
      forceY[edge.target] -= y;
    }

    const temperature = 0.9 - iteration / ITERATIONS * 0.72;
    for (let index = 0; index < points.length; index += 1) {
      forceX[index] += (CENTER - points[index].x) * 0.006;
      forceY[index] += (CENTER - points[index].y) * 0.006;
      points[index].velocityX = (points[index].velocityX + forceX[index]) *
        0.68;
      points[index].velocityY = (points[index].velocityY + forceY[index]) *
        0.68;
      points[index].x = Math.min(
        100 - HORIZONTAL_PADDING,
        Math.max(
          HORIZONTAL_PADDING,
          points[index].x + points[index].velocityX * temperature,
        ),
      );
      points[index].y = Math.min(
        100 - VERTICAL_PADDING,
        Math.max(
          VERTICAL_PADDING,
          points[index].y + points[index].velocityY * temperature,
        ),
      );
    }
  }

  return points.map(({ node, x, y }) => ({
    ...node,
    x: rounded(x),
    y: rounded(y),
  }));
}
