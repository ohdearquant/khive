"use client";

import type { Task } from "@/lib/swarm/types";

// ---------------------------------------------------------------------------
// Relative time formatting
// ---------------------------------------------------------------------------

function relativeTime(ms: number): string {
  const diff = Date.now() - ms;
  const seconds = Math.floor(diff / 1000);
  const minutes = Math.floor(seconds / 60);
  const hours = Math.floor(minutes / 60);

  if (seconds < 60) return `${seconds}s ago`;
  if (minutes < 60) return `${minutes}m ago`;
  return `${hours}h ago`;
}

// ---------------------------------------------------------------------------
// Status badge
// ---------------------------------------------------------------------------

const STATUS_COLOR: Record<string, string> = {
  done: "bg-green-100 text-green-700",
  cancelled: "bg-neutral-100 text-neutral-500",
};

// ---------------------------------------------------------------------------
// AgentTaskList — scrollable list of recent completions
// ---------------------------------------------------------------------------

interface AgentTaskListProps {
  tasks: Task[];
}

export default function AgentTaskList({ tasks }: AgentTaskListProps) {
  if (tasks.length === 0) {
    return <p className="text-sm text-neutral-500">No completed tasks in the last hour.</p>;
  }

  return (
    <ul
      className="divide-y divide-neutral-100 rounded-lg border border-neutral-200 bg-white"
      aria-label="Recent task completions"
    >
      {tasks.map((task) => (
        <li key={task.id} className="flex items-start gap-3 px-4 py-3 text-sm">
          <code className="mt-0.5 shrink-0 font-mono text-xs text-neutral-400">#{task.id}</code>

          <div className="min-w-0 flex-1">
            <p className="truncate text-neutral-900">{task.title}</p>
          </div>

          <div className="flex shrink-0 items-center gap-2 text-xs text-neutral-500">
            <span
              className={`rounded px-1.5 py-0.5 font-semibold ${STATUS_COLOR[task.status] ?? ""}`}
            >
              {task.status}
            </span>
            {task.completedAt && <span>{relativeTime(task.completedAt)}</span>}
          </div>
        </li>
      ))}
    </ul>
  );
}
