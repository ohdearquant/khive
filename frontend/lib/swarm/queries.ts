import { Task, TaskSchema } from "./types";

// ---------------------------------------------------------------------------
// Gateway base — matches the next.config.ts rewrite rule
// ---------------------------------------------------------------------------

const GATEWAY = "/api/server";

// ---------------------------------------------------------------------------
// khive request DSL helper (ADR-020 + ADR-027)
// Sends a parallel or single request DSL string to the Deno gateway.
// ---------------------------------------------------------------------------

interface OpResult {
  ok: boolean;
  tool?: string;
  result?: unknown;
  error?: string;
}

async function khiveRequest(ops: string): Promise<OpResult[]> {
  const resp = await fetch(`${GATEWAY}/request`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ops }),
  });

  if (!resp.ok) {
    throw new Error(`Gateway error ${resp.status}: ${await resp.text()}`);
  }

  const json = await resp.json();

  // The gateway may return a single result or an array
  if (Array.isArray(json)) {
    return json as OpResult[];
  }
  return [json as OpResult];
}

// ---------------------------------------------------------------------------
// Parse a raw task result from the gateway into a typed Task
// Returns null if the shape is unexpected (defensive — skip bad records).
// ---------------------------------------------------------------------------

function parseTask(raw: unknown): Task | null {
  try {
    return TaskSchema.parse(raw);
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Fetch all active + next tasks across all assignees
// ---------------------------------------------------------------------------

export async function fetchAgentQueues(namespace: string): Promise<Task[]> {
  const resp = await khiveRequest(
    `[tasks(namespace="${namespace}", status="active", limit=200), tasks(namespace="${namespace}", status="next", limit=200)]`,
  );

  const active = (resp[0]?.result as unknown[]) ?? [];
  const next = (resp[1]?.result as unknown[]) ?? [];

  return [...active, ...next].map(parseTask).filter((t): t is Task => t !== null);
}

// ---------------------------------------------------------------------------
// Fetch recently completed tasks (last 1 hour) for throughput calculation.
// The `tasks` verb has no `since` parameter in v0.1 so we filter client-side.
// ---------------------------------------------------------------------------

export async function fetchRecentCompleted(namespace: string): Promise<Task[]> {
  const since = Date.now() - 3_600_000; // 1 hour ago

  const resp = await khiveRequest(`[tasks(namespace="${namespace}", status="done", limit=500)]`);

  const raw = (resp[0]?.result as unknown[]) ?? [];
  return raw
    .map(parseTask)
    .filter((t): t is Task => t !== null)
    .filter((t) => t.completedAt !== null && t.completedAt > since);
}

// ---------------------------------------------------------------------------
// Fetch all done tasks for mean-duration calculation
// (broader than 1h so we get a meaningful average)
// ---------------------------------------------------------------------------

export async function fetchDoneTasks(namespace: string): Promise<Task[]> {
  const resp = await khiveRequest(`[tasks(namespace="${namespace}", status="done", limit=500)]`);

  const raw = (resp[0]?.result as unknown[]) ?? [];
  return raw.map(parseTask).filter((t): t is Task => t !== null);
}

// ---------------------------------------------------------------------------
// Fetch lifecycle transition events for revert-rate calculation.
// Phase 1: we fetch recent tasks and infer reverts from status patterns.
// Phase 2 (ADR-038): replace with list(kind="event", verb="transition").
// ---------------------------------------------------------------------------

export async function fetchRevertEvents(namespace: string): Promise<Task[]> {
  // In phase 1, we approximate revert signals by looking at cancelled tasks —
  // a task cancelled and reassigned back is the closest observable signal
  // available without the full event stream.
  // The actual revert-rate derivation in derive.ts uses this combined list.
  const resp = await khiveRequest(
    `[tasks(namespace="${namespace}", status="cancelled", limit=200)]`,
  );

  const raw = (resp[0]?.result as unknown[]) ?? [];
  return raw.map(parseTask).filter((t): t is Task => t !== null);
}

// ---------------------------------------------------------------------------
// Fetch all tasks (active + next + done + cancelled) in one parallel batch
// for the heatmap derivation which needs a 24-hour window.
// ---------------------------------------------------------------------------

export async function fetchAllTasksForHeatmap(namespace: string): Promise<Task[]> {
  const resp = await khiveRequest(
    `[tasks(namespace="${namespace}", status="active", limit=200), tasks(namespace="${namespace}", status="next", limit=200), tasks(namespace="${namespace}", status="done", limit=500)]`,
  );

  const active = (resp[0]?.result as unknown[]) ?? [];
  const next = (resp[1]?.result as unknown[]) ?? [];
  const done = (resp[2]?.result as unknown[]) ?? [];

  return [...active, ...next, ...done].map(parseTask).filter((t): t is Task => t !== null);
}
