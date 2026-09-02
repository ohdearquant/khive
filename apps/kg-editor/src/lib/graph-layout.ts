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
// gutters. The widest node card (a max-degree package/repository card in the
// repo showcase) renders at ~138px on that stage, so settled centers need up
// to a 23% horizontal inset to keep the complete card inside the clipped
// stage.
const HORIZONTAL_PADDING = 23;
const VERTICAL_PADDING = 10;
const CENTER = 50;
const ITERATIONS = 160;
// The small-graph layout (the review page and the repository showcase) runs
// the full force simulation unconditionally; once a graph is large enough
// that the fixed iteration counts start to dominate the main thread, cap the
// synchronous all-pairs work instead. At the supported 200-node maximum
// these budgets allow 12 force passes and 5 separation passes.
const FULL_QUALITY_NODE_LIMIT = 64;
const FORCE_PAIR_EVALUATION_BUDGET = 250_000;
const SEPARATION_PAIR_EVALUATION_BUDGET = 100_000;
// Minimum center-to-center separation (in the 0-100 layout space) below
// which two settled nodes are considered overlapping and get pushed apart by
// the deterministic cleanup pass that runs after the force settle.
const MIN_SEPARATION = 7;
const SEPARATION_ITERATIONS = 200;
// Golden-angle spiral used both for the initial placement and for nudging a
// settled point off a coordinate an earlier point already occupies.
const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));
export const MAX_SPIRAL_ATTEMPTS = 64;

// The rounded-coordinate grid inside the padded stage — the same rounding
// `rounded()` applies to the values this module returns — has 54,001 x
// 80,001 distinct cells at the current padding, far more than the 200-node
// schema maximum. A deterministic row-major scan from the stage origin is
// therefore guaranteed to reach an unoccupied cell after checking at most
// `occupied.size + 1` candidates, since at most 199 cells can be occupied.
const GRID_PRECISION = 1_000;
const MIN_X_GRID_UNITS = Math.round(HORIZONTAL_PADDING * GRID_PRECISION);
const MAX_X_GRID_UNITS = Math.round((100 - HORIZONTAL_PADDING) * GRID_PRECISION);
const MIN_Y_GRID_UNITS = Math.round(VERTICAL_PADDING * GRID_PRECISION);
const MAX_Y_GRID_UNITS = Math.round((100 - VERTICAL_PADDING) * GRID_PRECISION);

function boundedPairIterations(
  nodeCount: number,
  maximum: number,
  pairEvaluationBudget: number,
): number {
  if (nodeCount <= FULL_QUALITY_NODE_LIMIT) return maximum;
  const pairCount = nodeCount * (nodeCount - 1) / 2;
  return Math.min(maximum, Math.max(1, Math.floor(pairEvaluationBudget / pairCount)));
}

function clampToStageX(value: number): number {
  return Math.min(100 - HORIZONTAL_PADDING, Math.max(HORIZONTAL_PADDING, value));
}

function clampToStageY(value: number): number {
  return Math.min(100 - VERTICAL_PADDING, Math.max(VERTICAL_PADDING, value));
}

function firstFreeGridCoordinate(
  occupied: ReadonlySet<string>,
): { x: number; y: number } {
  for (let yUnits = MIN_Y_GRID_UNITS; yUnits <= MAX_Y_GRID_UNITS; yUnits += 1) {
    for (let xUnits = MIN_X_GRID_UNITS; xUnits <= MAX_X_GRID_UNITS; xUnits += 1) {
      const x = xUnits / GRID_PRECISION;
      const y = yUnits / GRID_PRECISION;
      if (!occupied.has(`${x},${y}`)) return { x, y };
    }
  }
  // Unreachable: the grid has 54,001 x 80,001 = 4,320,134,001 cells, over
  // four billion more than the schema's 200-node maximum can ever occupy.
  return { x: MIN_X_GRID_UNITS / GRID_PRECISION, y: MIN_Y_GRID_UNITS / GRID_PRECISION };
}

type LayoutPoint = { x: number; y: number };

/**
 * The `attempt`-th golden-angle nudge of `point` for the node at `index`: the
 * candidate `deduplicateSettledPositions` tries before falling back to the
 * grid scan. Exported so the fallback test occupies exactly the candidates
 * the production walk visits, whatever the formula becomes.
 */
export function spiralCandidate(
  point: LayoutPoint,
  index: number,
  attempt: number,
): LayoutPoint {
  const angle = index * GOLDEN_ANGLE * 7 + attempt * GOLDEN_ANGLE;
  const radius = 0.25 * attempt;
  return {
    x: clampToStageX(point.x + Math.cos(angle) * radius),
    y: clampToStageY(point.y + Math.sin(angle) * radius),
  };
}

// Walks settled points in order and nudges any point whose rounded
// coordinate repeats an earlier one. The golden-angle spiral is tried first
// (it preserves the existing layouts, since it was already the only
// mechanism in use); if all `MAX_SPIRAL_ATTEMPTS` spiral nudges are also
// occupied — the stage-corner case described above, where a diagonal push
// clamps straight back onto itself — fall back to the grid scan, which is
// guaranteed to terminate. The returned positions are pairwise distinct by
// construction.
export function deduplicateSettledPositions<T extends LayoutPoint>(
  points: readonly T[],
): T[] {
  const occupiedCoordinates = new Set<string>();
  const deduped = points.map((point) => ({ ...point }));
  for (let index = 0; index < deduped.length; index += 1) {
    const point = deduped[index];
    let key = `${rounded(point.x)},${rounded(point.y)}`;
    let attempt = 0;
    while (occupiedCoordinates.has(key) && attempt < MAX_SPIRAL_ATTEMPTS) {
      attempt += 1;
      const candidate = spiralCandidate(point, index, attempt);
      point.x = candidate.x;
      point.y = candidate.y;
      key = `${rounded(point.x)},${rounded(point.y)}`;
    }
    if (occupiedCoordinates.has(key)) {
      const free = firstFreeGridCoordinate(occupiedCoordinates);
      point.x = free.x;
      point.y = free.y;
      key = `${rounded(point.x)},${rounded(point.y)}`;
    }
    occupiedCoordinates.add(key);
  }
  return deduped;
}

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
  // Scale the spiral radius per axis by the padded half-extent of the stage
  // (not a flat pixel radius) so outer-ring nodes land inside bounds instead
  // of needing an immediate clamp — a large node count otherwise lands many
  // points on the same clamped corner before the force pass ever runs.
  const maxRadiusX = CENTER - HORIZONTAL_PADDING;
  const maxRadiusY = CENTER - VERTICAL_PADDING;
  const points = orderedNodes.map((node, index) => {
    const ringFraction = Math.sqrt((index + 0.5) / orderedNodes.length);
    const radiusScale = 0.15 + 0.8 * ringFraction;
    const angle = phase + index * GOLDEN_ANGLE + (random() - 0.5) * 0.08;
    return {
      node,
      x: CENTER + Math.cos(angle) * radiusScale * maxRadiusX,
      y: CENTER + Math.sin(angle) * radiusScale * maxRadiusY,
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
  const forceIterations = boundedPairIterations(
    points.length,
    ITERATIONS,
    FORCE_PAIR_EVALUATION_BUDGET,
  );
  const separationIterations = boundedPairIterations(
    points.length,
    SEPARATION_ITERATIONS,
    SEPARATION_PAIR_EVALUATION_BUDGET,
  );

  for (let iteration = 0; iteration < forceIterations; iteration += 1) {
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

    const temperature = 0.9 - iteration / forceIterations * 0.72;
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

  // The force pass alone can leave dense graphs (e.g. the 51-node repo
  // showcase) with settled points stacked on top of each other, since the
  // boundary clamp above stops motion at the edge instead of resolving the
  // overlap. Run a deterministic, order-fixed cleanup: repeatedly push any
  // pair closer than MIN_SEPARATION apart along their connecting vector (or
  // a stable per-pair fallback direction when they coincide exactly), then
  // re-clamp to the stage bounds.
  for (let pass = 0; pass < separationIterations; pass += 1) {
    let movedAny = false;
    for (let left = 0; left < points.length; left += 1) {
      for (let right = left + 1; right < points.length; right += 1) {
        const deltaX = points[right].x - points[left].x;
        const deltaY = points[right].y - points[left].y;
        const distance = Math.hypot(deltaX, deltaY);
        if (distance >= MIN_SEPARATION) continue;
        movedAny = true;
        const fallbackAngle = (left * 928_371 + right * 574_639) % 360 /
          360 * Math.PI * 2;
        const unitX = distance > 1e-6 ? deltaX / distance : Math.cos(fallbackAngle);
        const unitY = distance > 1e-6 ? deltaY / distance : Math.sin(fallbackAngle);
        const push = (MIN_SEPARATION - distance) / 2;
        points[left].x -= unitX * push;
        points[left].y -= unitY * push;
        points[right].x += unitX * push;
        points[right].y += unitY * push;
      }
    }
    for (const point of points) {
      point.x = Math.min(100 - HORIZONTAL_PADDING, Math.max(HORIZONTAL_PADDING, point.x));
      point.y = Math.min(100 - VERTICAL_PADDING, Math.max(VERTICAL_PADDING, point.y));
    }
    if (!movedAny) break;
  }

  // The pass budget above is sized to the node count, so a dense graph at
  // the schema-maximum can run out of separation passes with a handful of
  // points still exactly coincident: a diagonal push at a stage corner can
  // be entirely absorbed by the boundary clamp on both axes, since any
  // offset that moves a point further into the corner clamps straight back
  // to it, and no amount of extra passes resolves that on its own.
  // deduplicateSettledPositions makes the returned positions pairwise
  // distinct unconditionally, including that corner case.
  const deduped = deduplicateSettledPositions(points);

  return deduped.map(({ node, x, y }) => ({
    ...node,
    x: rounded(x),
    y: rounded(y),
  }));
}
