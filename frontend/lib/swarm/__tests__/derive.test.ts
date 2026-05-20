import { describe, expect, it } from "vitest";
import {
  buildHeatmapBuckets,
  computeAgentSummaries,
  deriveCycles,
  deriveDriftAlerts,
  deriveHandoffs,
  deriveHeatmap,
  deriveThroughputBuckets,
} from "../derive";
import type { AgentSummary, Task } from "../types";

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

const BASE_TASK: Omit<Task, "id" | "title" | "assignee" | "status"> = {
  fullId: "00000000-0000-0000-0000-000000000000",
  priority: "p2",
  tags: [],
  dependsOn: [],
  createdAt: Date.now() - 60_000,
  completedAt: null,
};

function makeTask(overrides: Partial<Task> & Pick<Task, "id" | "title">): Task {
  return { ...BASE_TASK, assignee: null, status: "active", ...overrides };
}

// ---------------------------------------------------------------------------
// computeAgentSummaries
// ---------------------------------------------------------------------------

describe("computeAgentSummaries", () => {
  it("returns empty array when no tasks", () => {
    expect(computeAgentSummaries([], [], [])).toEqual([]);
  });

  it("counts active and next tasks per agent", () => {
    const queue = [
      makeTask({ id: "t1", title: "T1", assignee: "gap-analyst", status: "active" }),
      makeTask({ id: "t2", title: "T2", assignee: "gap-analyst", status: "next" }),
      makeTask({ id: "t3", title: "T3", assignee: "expander", status: "active" }),
    ];
    const summaries = computeAgentSummaries(queue, [], []);

    const ga = summaries.find((s) => s.name === "gap-analyst")!;
    expect(ga.activeTasks).toBe(1);
    expect(ga.nextTasks).toBe(1);

    const ex = summaries.find((s) => s.name === "expander")!;
    expect(ex.activeTasks).toBe(1);
    expect(ex.nextTasks).toBe(0);
  });

  it("counts completedLast1h from recentCompleted", () => {
    const recent = [
      makeTask({
        id: "c1",
        title: "C1",
        assignee: "gap-analyst",
        status: "done",
        completedAt: Date.now() - 30_000,
      }),
      makeTask({
        id: "c2",
        title: "C2",
        assignee: "gap-analyst",
        status: "done",
        completedAt: Date.now() - 60_000,
      }),
    ];
    const summaries = computeAgentSummaries([], recent, []);
    const ga = summaries.find((s) => s.name === "gap-analyst")!;
    expect(ga.completedLast1h).toBe(2);
  });

  it("computes meanDurationMs from done tasks", () => {
    const now = Date.now();
    const done = [
      makeTask({
        id: "d1",
        title: "D1",
        assignee: "expander",
        status: "done",
        createdAt: now - 10_000,
        completedAt: now,
      }),
      makeTask({
        id: "d2",
        title: "D2",
        assignee: "expander",
        status: "done",
        createdAt: now - 20_000,
        completedAt: now,
      }),
    ];
    const summaries = computeAgentSummaries([], [], done);
    const ex = summaries.find((s) => s.name === "expander")!;
    expect(ex.meanDurationMs).toBe(15_000);
  });

  it("sorts agents by activeTasks descending", () => {
    const queue = [
      makeTask({ id: "a1", title: "A1", assignee: "expander", status: "active" }),
      makeTask({ id: "a2", title: "A2", assignee: "expander", status: "active" }),
      makeTask({ id: "b1", title: "B1", assignee: "gap-analyst", status: "active" }),
    ];
    const summaries = computeAgentSummaries(queue, [], []);
    expect(summaries[0].name).toBe("expander");
  });
});

// ---------------------------------------------------------------------------
// deriveHandoffs
// ---------------------------------------------------------------------------

describe("deriveHandoffs", () => {
  it("returns empty array when no tasks", () => {
    expect(deriveHandoffs([])).toEqual([]);
  });

  it("extracts from:X tags as handoff edges", () => {
    const tasks = [
      makeTask({
        id: "h1",
        title: "H1",
        assignee: "expander",
        tags: ["from:gap-analyst", "cycle:1"],
        status: "active",
      }),
      makeTask({
        id: "h2",
        title: "H2",
        assignee: "expander",
        tags: ["from:gap-analyst"],
        status: "active",
      }),
      makeTask({
        id: "h3",
        title: "H3",
        assignee: "polisher",
        tags: ["from:expander"],
        status: "active",
      }),
    ];

    const edges = deriveHandoffs(tasks);

    const gaToEx = edges.find((e) => e.fromAgent === "gap-analyst" && e.toAgent === "expander");
    expect(gaToEx).toBeDefined();
    expect(gaToEx!.taskCount).toBe(2);

    const exToPol = edges.find((e) => e.fromAgent === "expander" && e.toAgent === "polisher");
    expect(exToPol).toBeDefined();
    expect(exToPol!.taskCount).toBe(1);
  });

  it("ignores self-handoffs", () => {
    const tasks = [
      makeTask({
        id: "s1",
        title: "S1",
        assignee: "gap-analyst",
        tags: ["from:gap-analyst"],
        status: "active",
      }),
    ];
    expect(deriveHandoffs(tasks)).toEqual([]);
  });

  it("ignores tasks with no assignee", () => {
    const tasks = [
      makeTask({
        id: "n1",
        title: "N1",
        assignee: null,
        tags: ["from:gap-analyst"],
        status: "active",
      }),
    ];
    expect(deriveHandoffs(tasks)).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// deriveCycles
// ---------------------------------------------------------------------------

describe("deriveCycles", () => {
  it("returns empty array when no tasks", () => {
    expect(deriveCycles([])).toEqual([]);
  });

  it("groups tasks by cycle:N tag", () => {
    const tasks = [
      makeTask({ id: "c1", title: "C1", assignee: "a", tags: ["cycle:1"], status: "done" }),
      makeTask({ id: "c2", title: "C2", assignee: "a", tags: ["cycle:1"], status: "done" }),
      makeTask({ id: "c3", title: "C3", assignee: "b", tags: ["cycle:2"], status: "active" }),
    ];

    const cycles = deriveCycles(tasks);
    const c1 = cycles.find((b) => b.cycleLabel === "cycle:1")!;
    expect(c1.counts.find((c) => c.status === "done")!.count).toBe(2);

    const c2 = cycles.find((b) => b.cycleLabel === "cycle:2")!;
    expect(c2.counts.find((c) => c.status === "active")!.count).toBe(1);
  });

  it("puts untagged tasks in 'none' bucket", () => {
    const tasks = [makeTask({ id: "u1", title: "U1", assignee: "a", tags: [], status: "active" })];
    const cycles = deriveCycles(tasks);
    expect(cycles.find((b) => b.cycleLabel === "none")).toBeDefined();
  });

  it("sorts cycle:N numerically with none last", () => {
    const tasks = [
      makeTask({ id: "x1", title: "X1", assignee: "a", tags: ["cycle:3"], status: "done" }),
      makeTask({ id: "x2", title: "X2", assignee: "a", tags: ["cycle:1"], status: "done" }),
      makeTask({ id: "x3", title: "X3", assignee: "a", tags: [], status: "done" }),
      makeTask({ id: "x4", title: "X4", assignee: "a", tags: ["cycle:2"], status: "done" }),
    ];
    const cycles = deriveCycles(tasks);
    const labels = cycles.map((c) => c.cycleLabel);
    expect(labels).toEqual(["cycle:1", "cycle:2", "cycle:3", "none"]);
  });
});

// ---------------------------------------------------------------------------
// deriveDriftAlerts
// ---------------------------------------------------------------------------

describe("deriveDriftAlerts", () => {
  it("returns empty when no agent exceeds 5% threshold", () => {
    const summaries: AgentSummary[] = [
      {
        name: "gap-analyst",
        activeTasks: 0,
        nextTasks: 0,
        completedLast1h: 100,
        meanDurationMs: null,
        revertRate: 0.02,
        lastActiveAt: null,
      },
    ];
    expect(deriveDriftAlerts(summaries)).toEqual([]);
  });

  it("returns alerts for agents at or above 5%", () => {
    const summaries: AgentSummary[] = [
      {
        name: "polisher",
        activeTasks: 2,
        nextTasks: 0,
        completedLast1h: 10,
        meanDurationMs: null,
        revertRate: 0.12,
        lastActiveAt: null,
      },
    ];
    const alerts = deriveDriftAlerts(summaries);
    expect(alerts).toHaveLength(1);
    expect(alerts[0].agentName).toBe("polisher");
    expect(alerts[0].revertRate).toBe(0.12);
  });

  it("sorts by revertRate descending", () => {
    const summaries: AgentSummary[] = [
      {
        name: "a",
        activeTasks: 0,
        nextTasks: 0,
        completedLast1h: 10,
        meanDurationMs: null,
        revertRate: 0.08,
        lastActiveAt: null,
      },
      {
        name: "b",
        activeTasks: 0,
        nextTasks: 0,
        completedLast1h: 10,
        meanDurationMs: null,
        revertRate: 0.2,
        lastActiveAt: null,
      },
    ];
    const alerts = deriveDriftAlerts(summaries);
    expect(alerts[0].agentName).toBe("b");
  });
});

// ---------------------------------------------------------------------------
// deriveThroughputBuckets
// ---------------------------------------------------------------------------

describe("deriveThroughputBuckets", () => {
  it("returns 12 buckets by default", () => {
    const buckets = deriveThroughputBuckets([], "gap-analyst");
    expect(buckets).toHaveLength(12);
  });

  it("counts completions in correct 5-minute bucket", () => {
    const now = Date.now();
    const bucketMs = 5 * 60 * 1000;
    // Task completed 2 minutes ago → last bucket
    const task = makeTask({
      id: "t1",
      title: "T1",
      assignee: "gap-analyst",
      status: "done",
      completedAt: now - 2 * 60 * 1000,
    });

    const buckets = deriveThroughputBuckets([task], "gap-analyst", 12, bucketMs);
    const lastBucket = buckets[buckets.length - 1];
    expect(lastBucket.count).toBe(1);
  });

  it("ignores tasks from other agents", () => {
    const task = makeTask({
      id: "t2",
      title: "T2",
      assignee: "expander",
      status: "done",
      completedAt: Date.now() - 60_000,
    });
    const buckets = deriveThroughputBuckets([task], "gap-analyst");
    expect(buckets.reduce((s, b) => s + b.count, 0)).toBe(0);
  });
});

// ---------------------------------------------------------------------------
// buildHeatmapBuckets
// ---------------------------------------------------------------------------

describe("buildHeatmapBuckets", () => {
  it("returns 24 buckets by default", () => {
    expect(buildHeatmapBuckets()).toHaveLength(24);
  });

  it("returns ISO-8601 strings aligned to the hour", () => {
    const buckets = buildHeatmapBuckets(3);
    for (const bucket of buckets) {
      expect(bucket).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:00:00Z$/);
    }
  });

  it("buckets are in chronological order", () => {
    const buckets = buildHeatmapBuckets(5);
    for (let i = 1; i < buckets.length; i++) {
      expect(new Date(buckets[i]).getTime()).toBeGreaterThan(new Date(buckets[i - 1]).getTime());
    }
  });
});

// ---------------------------------------------------------------------------
// deriveHeatmap
// ---------------------------------------------------------------------------

describe("deriveHeatmap", () => {
  it("returns a matrix with agents as rows and buckets as columns", () => {
    const agents = ["a", "b"];
    const buckets = buildHeatmapBuckets(4);
    const matrix = deriveHeatmap([], agents, buckets);

    expect(matrix).toHaveLength(2);
    expect(matrix[0]).toHaveLength(4);
    expect(matrix[1]).toHaveLength(4);
  });

  it("increments cell count for tasks in the corresponding bucket", () => {
    const agents = ["gap-analyst"];
    const buckets = buildHeatmapBuckets(2);
    const [oldBucket] = buckets;

    const task = makeTask({
      id: "t1",
      title: "T1",
      assignee: "gap-analyst",
      status: "active",
      createdAt: new Date(oldBucket).getTime() + 1000,
    });

    const matrix = deriveHeatmap([task], agents, buckets);
    // Task created just after the first bucket start — should appear in both
    // buckets (still active, no completedAt)
    expect(matrix[0][0].queueDepth).toBeGreaterThan(0);
  });
});
