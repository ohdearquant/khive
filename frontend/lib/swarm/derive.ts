import type {
  AgentSummary,
  CycleBucket,
  DriftAlert,
  HandoffEdge,
  HeatmapCell,
  Task,
  ThroughputBucket,
} from "./types";

// ---------------------------------------------------------------------------
// computeAgentSummaries
//
// Input: all active/next tasks + completed tasks in the last 1h + done tasks
// Output: one AgentSummary per unique assignee seen in any task
// ---------------------------------------------------------------------------

export function computeAgentSummaries(
  queueTasks: Task[],
  recentCompleted: Task[],
  doneTasks: Task[],
): AgentSummary[] {
  const agentNames = new Set<string>();

  for (const t of queueTasks) {
    if (t.assignee) agentNames.add(t.assignee);
  }
  for (const t of recentCompleted) {
    if (t.assignee) agentNames.add(t.assignee);
  }
  for (const t of doneTasks) {
    if (t.assignee) agentNames.add(t.assignee);
  }

  const summaries: AgentSummary[] = [];

  for (const name of agentNames) {
    const activeTasks = queueTasks.filter(
      (t) => t.assignee === name && t.status === "active",
    ).length;

    const nextTasks = queueTasks.filter((t) => t.assignee === name && t.status === "next").length;

    const completedLast1h = recentCompleted.filter((t) => t.assignee === name).length;

    // Mean duration from all done tasks that have both timestamps
    const doneWithDuration = doneTasks.filter(
      (t) => t.assignee === name && t.completedAt !== null && t.completedAt !== undefined,
    );

    let meanDurationMs: number | null = null;
    if (doneWithDuration.length > 0) {
      const totalMs = doneWithDuration.reduce((sum, t) => {
        const dur = (t.completedAt ?? t.createdAt) - t.createdAt;
        return sum + Math.max(0, dur);
      }, 0);
      meanDurationMs = Math.round(totalMs / doneWithDuration.length);
    }

    // Revert rate: tasks that were cancelled / total handled
    // Phase 1 approximation — phase 2 will use actual transition events
    const allHandled = doneTasks.filter((t) => t.assignee === name).length;
    const cancelled = queueTasks.filter(
      (t) => t.assignee === name && t.status === "cancelled",
    ).length;
    const revertRate = allHandled + cancelled > 0 ? cancelled / (allHandled + cancelled) : 0;

    // Last active timestamp
    const allAgentTasks = [
      ...queueTasks.filter((t) => t.assignee === name),
      ...recentCompleted.filter((t) => t.assignee === name),
    ];
    const lastActiveAt =
      allAgentTasks.length > 0
        ? Math.max(...allAgentTasks.map((t) => t.completedAt ?? t.createdAt))
        : null;

    summaries.push({
      name,
      activeTasks,
      nextTasks,
      completedLast1h,
      meanDurationMs,
      revertRate,
      lastActiveAt,
    });
  }

  // Sort by active task count descending for stable card ordering
  return summaries.sort((a, b) => b.activeTasks - a.activeTasks);
}

// ---------------------------------------------------------------------------
// deriveHandoffs
//
// Handoffs are implicit: a task assigned to agent Y with a tag "from:X"
// means X handed off to Y.
// ---------------------------------------------------------------------------

export function deriveHandoffs(tasks: Task[]): HandoffEdge[] {
  // Map: `${fromAgent}:${toAgent}` -> { count, lastTs }
  const edgeMap = new Map<string, { count: number; lastTs: number | null }>();

  for (const task of tasks) {
    if (!task.assignee) continue;

    for (const tag of task.tags ?? []) {
      if (tag.startsWith("from:")) {
        const fromAgent = tag.slice(5);
        const toAgent = task.assignee;

        if (!fromAgent || fromAgent === toAgent) continue;

        const key = `${fromAgent}:${toAgent}`;
        const existing = edgeMap.get(key);
        const ts = task.completedAt ?? task.createdAt;

        if (existing) {
          existing.count += 1;
          if (ts !== null && (existing.lastTs === null || ts > existing.lastTs)) {
            existing.lastTs = ts;
          }
        } else {
          edgeMap.set(key, { count: 1, lastTs: ts });
        }
      }
    }
  }

  const edges: HandoffEdge[] = [];
  for (const [key, { count, lastTs }] of edgeMap) {
    const colonIdx = key.indexOf(":");
    edges.push({
      fromAgent: key.slice(0, colonIdx),
      toAgent: key.slice(colonIdx + 1),
      taskCount: count,
      lastHandoffAt: lastTs,
    });
  }

  return edges.sort((a, b) => b.taskCount - a.taskCount);
}

// ---------------------------------------------------------------------------
// deriveCycles
//
// Tasks carry tags like "cycle:1", "cycle:2". Group by cycle label and status.
// Tasks without a cycle tag go into "none".
// ---------------------------------------------------------------------------

export function deriveCycles(tasks: Task[]): CycleBucket[] {
  const cycleMap = new Map<string, Map<string, number>>();

  for (const task of tasks) {
    let cycleLabel = "none";

    for (const tag of task.tags ?? []) {
      if (tag.startsWith("cycle:")) {
        cycleLabel = tag;
        break;
      }
    }

    let statusMap = cycleMap.get(cycleLabel);
    if (!statusMap) {
      statusMap = new Map<string, number>();
      cycleMap.set(cycleLabel, statusMap);
    }

    statusMap.set(task.status, (statusMap.get(task.status) ?? 0) + 1);
  }

  const buckets: CycleBucket[] = [];
  for (const [cycleLabel, statusMap] of cycleMap) {
    const counts = Array.from(statusMap.entries()).map(([status, count]) => ({
      status,
      count,
    }));
    buckets.push({ cycleLabel, counts });
  }

  // Sort cycles: numeric order for "cycle:N", "none" last
  return buckets.sort((a, b) => {
    if (a.cycleLabel === "none") return 1;
    if (b.cycleLabel === "none") return -1;
    const na = parseInt(a.cycleLabel.replace("cycle:", ""), 10);
    const nb = parseInt(b.cycleLabel.replace("cycle:", ""), 10);
    return na - nb;
  });
}

// ---------------------------------------------------------------------------
// deriveHeatmap
//
// Builds a HeatmapCell matrix: rows = agents, columns = time buckets.
// Each cell = queue depth (active + next task count) at the start of that bucket.
//
// Strategy: for each task in active/next, find which 1-hour bucket its
// createdAt falls in, and increment that cell. This approximates queue depth
// at each hour (tasks that were created and still in queue during that bucket).
// ---------------------------------------------------------------------------

export function deriveHeatmap(tasks: Task[], agents: string[], buckets: string[]): HeatmapCell[][] {
  // Build a lookup: agentName -> bucketIso -> count
  const countMap = new Map<string, Map<string, number>>();

  for (const agent of agents) {
    countMap.set(agent, new Map<string, number>());
  }

  // Each time bucket is a 1-hour ISO string. Parse to get the start timestamp.
  const bucketStartMs = buckets.map((b) => new Date(b).getTime());

  const queueTasks = tasks.filter((t) => t.status === "active" || t.status === "next");

  for (const task of queueTasks) {
    if (!task.assignee || !agents.includes(task.assignee)) continue;

    const agentMap = countMap.get(task.assignee)!;

    // Find the bucket this task belongs to (created within or before this bucket)
    for (let i = 0; i < buckets.length; i++) {
      const bucketStart = bucketStartMs[i];
      const bucketEnd =
        i + 1 < bucketStartMs.length ? bucketStartMs[i + 1] : bucketStart + 3_600_000;

      // Task was queued during this bucket if it was created before bucket end
      // and not completed before bucket start
      const createdBefore = task.createdAt < bucketEnd;
      const completedBefore =
        task.completedAt !== null &&
        task.completedAt !== undefined &&
        task.completedAt < bucketStart;

      if (createdBefore && !completedBefore) {
        agentMap.set(buckets[i], (agentMap.get(buckets[i]) ?? 0) + 1);
      }
    }
  }

  // Build the 2D matrix: outer = agents, inner = buckets
  return agents.map((agent) => {
    const agentMap = countMap.get(agent) ?? new Map<string, number>();
    return buckets.map((bucket) => ({
      agentName: agent,
      timeBucket: bucket,
      queueDepth: agentMap.get(bucket) ?? 0,
    }));
  });
}

// ---------------------------------------------------------------------------
// deriveDriftAlerts
//
// Returns agents whose revert rate >= 5% (ADR-045 threshold).
// Sorted by revertRate descending.
// ---------------------------------------------------------------------------

export function deriveDriftAlerts(summaries: AgentSummary[]): DriftAlert[] {
  return summaries
    .filter((s) => s.revertRate >= 0.05)
    .map((s) => ({
      agentName: s.name,
      revertCount: Math.round(s.revertRate * (s.completedLast1h || 1)),
      windowHours: 1,
      revertRate: s.revertRate,
    }))
    .sort((a, b) => b.revertRate - a.revertRate);
}

// ---------------------------------------------------------------------------
// deriveThroughputBuckets
//
// Splits completed tasks into 5-minute buckets (last hour = 12 buckets).
// Returns newest-last array of { ts, count }.
// ---------------------------------------------------------------------------

export function deriveThroughputBuckets(
  completedTasks: Task[],
  agentName: string,
  bucketCount = 12,
  bucketMs = 5 * 60 * 1000,
): ThroughputBucket[] {
  const now = Date.now();
  const buckets: ThroughputBucket[] = [];

  for (let i = bucketCount - 1; i >= 0; i--) {
    const bucketStart = now - (i + 1) * bucketMs;
    const bucketEnd = now - i * bucketMs;

    const count = completedTasks.filter(
      (t) =>
        t.assignee === agentName &&
        t.completedAt !== null &&
        t.completedAt !== undefined &&
        t.completedAt >= bucketStart &&
        t.completedAt < bucketEnd,
    ).length;

    buckets.push({ ts: bucketStart, count });
  }

  return buckets;
}

// ---------------------------------------------------------------------------
// buildHeatmapBuckets
//
// Generates the last 24 one-hour bucket labels as ISO-8601 strings.
// ---------------------------------------------------------------------------

export function buildHeatmapBuckets(hourCount = 24): string[] {
  const now = Date.now();
  const buckets: string[] = [];

  for (let i = hourCount - 1; i >= 0; i--) {
    const ts = now - i * 3_600_000;
    // Truncate to the hour
    const d = new Date(ts);
    d.setMinutes(0, 0, 0);
    buckets.push(d.toISOString().replace(".000Z", "Z"));
  }

  return buckets;
}
