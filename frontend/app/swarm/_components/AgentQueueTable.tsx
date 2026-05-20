"use client";

import type { Task } from "@/lib/swarm/types";

// ---------------------------------------------------------------------------
// Priority badge
// ---------------------------------------------------------------------------

const PRIORITY_COLOR: Record<string, string> = {
  p0: "bg-red-100 text-red-700",
  p1: "bg-orange-100 text-orange-700",
  p2: "bg-blue-100 text-blue-700",
  p3: "bg-neutral-100 text-neutral-600",
};

// ---------------------------------------------------------------------------
// Status badge
// ---------------------------------------------------------------------------

const STATUS_COLOR: Record<string, string> = {
  active: "bg-green-100 text-green-700",
  next: "bg-blue-100 text-blue-700",
  waiting: "bg-amber-100 text-amber-700",
};

// ---------------------------------------------------------------------------
// AgentQueueTable — shows active + next tasks for the agent
// ---------------------------------------------------------------------------

interface AgentQueueTableProps {
  activeTasks: Task[];
  nextTasks: Task[];
}

export default function AgentQueueTable({ activeTasks, nextTasks }: AgentQueueTableProps) {
  const combined = [
    ...activeTasks.map((t) => ({ ...t, status: "active" as const })),
    ...nextTasks.map((t) => ({ ...t, status: "next" as const })),
  ];

  if (combined.length === 0) {
    return <p className="text-sm text-neutral-500">No tasks in queue.</p>;
  }

  const shown = combined.slice(0, 10);
  const overflow = combined.length - shown.length;

  return (
    <div>
      <ul
        className="divide-y divide-neutral-100 rounded-lg border border-neutral-200 bg-white"
        aria-label="Agent queue"
      >
        {shown.map((task) => (
          <li key={task.id} className="flex items-start gap-3 px-4 py-3 text-sm">
            <code className="mt-0.5 shrink-0 font-mono text-xs text-neutral-400">#{task.id}</code>

            <div className="min-w-0 flex-1">
              <p className="truncate text-neutral-900">{task.title}</p>
              {task.tags && task.tags.length > 0 && (
                <p className="mt-0.5 truncate text-xs text-neutral-400">
                  {task.tags.slice(0, 4).join(" · ")}
                </p>
              )}
            </div>

            <div className="flex shrink-0 items-center gap-1.5">
              <span
                className={`rounded px-1.5 py-0.5 text-xs font-semibold ${PRIORITY_COLOR[task.priority] ?? PRIORITY_COLOR.p3}`}
              >
                {task.priority}
              </span>
              <span
                className={`rounded px-1.5 py-0.5 text-xs font-semibold ${STATUS_COLOR[task.status] ?? ""}`}
              >
                {task.status}
              </span>
            </div>
          </li>
        ))}
      </ul>

      {overflow > 0 && <p className="mt-2 text-xs text-neutral-400">{overflow} more…</p>}
    </div>
  );
}
