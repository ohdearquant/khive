"use client";

import type { Task, Priority } from "../../../lib/types";

const PRIORITY_BADGE: Record<
  Priority,
  { label: string; className: string }
> = {
  p0: { label: "P0", className: "bg-red-900 text-red-300 border-red-800" },
  p1: {
    label: "P1",
    className: "bg-orange-900 text-orange-300 border-orange-800",
  },
  p2: { label: "P2", className: "bg-blue-900 text-blue-300 border-blue-800" },
  p3: { label: "P3", className: "bg-neutral-800 text-neutral-400 border-neutral-700" },
};

function formatRelativeDate(dateStr?: string): string | null {
  if (!dateStr) return null;
  const date = new Date(dateStr);
  const now = new Date();
  const diffMs = date.getTime() - now.getTime();
  const diffDays = Math.round(diffMs / (1000 * 60 * 60 * 24));
  if (diffDays === 0) return "today";
  if (diffDays === 1) return "tomorrow";
  if (diffDays === -1) return "yesterday";
  if (diffDays > 0) return `in ${diffDays}d`;
  return `overdue ${Math.abs(diffDays)}d`;
}

function isOverdue(dateStr?: string): boolean {
  if (!dateStr) return false;
  return new Date(dateStr) < new Date();
}

interface Props {
  task: Task;
  onClick: (task: Task) => void;
}

const MAX_TAGS = 3;

export function TaskCard({ task, onClick }: Props) {
  const badge = PRIORITY_BADGE[task.priority];
  const overdue = isOverdue(task.due);
  const relDate = formatRelativeDate(task.due);
  const tags = task.tags ?? [];
  const visibleTags = tags.slice(0, MAX_TAGS);
  const extraTags = tags.length - MAX_TAGS;

  return (
    <button
      onClick={() => onClick(task)}
      className={[
        "w-full rounded border bg-neutral-900 p-3 text-left transition-colors hover:bg-neutral-800",
        overdue ? "border-red-800" : "border-neutral-800",
      ].join(" ")}
    >
      {/* Priority + title */}
      <div className="mb-1 flex items-start gap-2">
        <span
          className={`mt-0.5 shrink-0 rounded border px-1 py-0.5 text-xs font-bold ${badge.className}`}
        >
          {badge.label}
        </span>
        <span className="line-clamp-2 text-sm font-medium text-neutral-100">
          {task.title}
        </span>
      </div>

      {/* Assignee */}
      {task.assignee && (
        <div className="mb-1.5">
          <span className="rounded bg-neutral-800 px-1.5 py-0.5 text-xs text-neutral-400">
            {task.assignee}
          </span>
        </div>
      )}

      {/* Tags */}
      {visibleTags.length > 0 && (
        <div className="mb-1.5 flex flex-wrap gap-1">
          {visibleTags.map((tag) => (
            <span
              key={tag}
              className="rounded bg-neutral-800 px-1.5 py-0.5 text-xs text-neutral-500"
            >
              {tag}
            </span>
          ))}
          {extraTags > 0 && (
            <span className="rounded bg-neutral-800 px-1.5 py-0.5 text-xs text-neutral-600">
              +{extraTags} more
            </span>
          )}
        </div>
      )}

      {/* Due date */}
      {relDate && (
        <div className={`text-xs ${overdue ? "text-red-400" : "text-neutral-500"}`}>
          {overdue ? "⚠ " : ""}due {relDate}
        </div>
      )}
    </button>
  );
}
