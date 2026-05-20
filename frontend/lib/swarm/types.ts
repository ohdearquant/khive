import { z } from "zod";

// ---------------------------------------------------------------------------
// Raw GTD substrate types (ADR-026)
// ---------------------------------------------------------------------------

export const TaskSchema = z.object({
  id: z.string(),
  fullId: z.string().optional().default(""),
  title: z.string(),
  status: z.enum(["inbox", "next", "active", "waiting", "someday", "done", "cancelled"]),
  priority: z.enum(["p0", "p1", "p2", "p3"]),
  assignee: z.string().nullable().optional().default(null),
  tags: z.array(z.string()).optional().default([]),
  dependsOn: z.array(z.string()).optional().default([]),
  createdAt: z.number(),
  completedAt: z.number().nullable().optional().default(null),
});

export type Task = z.infer<typeof TaskSchema>;

// ---------------------------------------------------------------------------
// Lifecycle transition events (ADR-038 phase 2; used in phase 1 polling too)
// ---------------------------------------------------------------------------

export const TransitionEventSchema = z.object({
  id: z.string(),
  verb: z.string(),
  actor: z.string(),
  namespace: z.string(),
  createdAt: z.number(),
  data: z.object({
    taskId: z.string(),
    assignee: z.string(),
    fromStatus: z.string(),
    toStatus: z.string(),
  }),
});

export type TransitionEvent = z.infer<typeof TransitionEventSchema>;

// ---------------------------------------------------------------------------
// Computed aggregates (derived client-side, never stored)
// ---------------------------------------------------------------------------

export const AgentSummarySchema = z.object({
  name: z.string(),
  activeTasks: z.number(),
  nextTasks: z.number(),
  completedLast1h: z.number(),
  meanDurationMs: z.number().nullable(),
  revertRate: z.number(),
  lastActiveAt: z.number().nullable(),
});

export type AgentSummary = z.infer<typeof AgentSummarySchema>;

export const HandoffEdgeSchema = z.object({
  fromAgent: z.string(),
  toAgent: z.string(),
  taskCount: z.number(),
  lastHandoffAt: z.number().nullable(),
});

export type HandoffEdge = z.infer<typeof HandoffEdgeSchema>;

export const CycleBucketSchema = z.object({
  cycleLabel: z.string(),
  counts: z.array(
    z.object({
      status: z.string(),
      count: z.number(),
    }),
  ),
});

export type CycleBucket = z.infer<typeof CycleBucketSchema>;

export const HeatmapCellSchema = z.object({
  agentName: z.string(),
  timeBucket: z.string(),
  queueDepth: z.number(),
});

export type HeatmapCell = z.infer<typeof HeatmapCellSchema>;

export const DriftAlertSchema = z.object({
  agentName: z.string(),
  revertCount: z.number(),
  windowHours: z.number(),
  revertRate: z.number(),
});

export type DriftAlert = z.infer<typeof DriftAlertSchema>;

// ---------------------------------------------------------------------------
// Throughput bucket for sparklines (5-minute buckets)
// ---------------------------------------------------------------------------

export interface ThroughputBucket {
  ts: number; // Unix ms, start of bucket
  count: number;
}

// ---------------------------------------------------------------------------
// Raw API response shape from the khive gateway
// ---------------------------------------------------------------------------

export const TasksResponseSchema = z.object({
  ok: z.boolean(),
  tool: z.string().optional(),
  result: z.array(z.unknown()).optional().default([]),
  error: z.string().optional(),
});

export type TasksResponse = z.infer<typeof TasksResponseSchema>;
