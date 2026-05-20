"use client";

import { useEffect, useRef } from "react";
import { useTask, useNeighbors, useEntity } from "../../../lib/api";
import type { Priority } from "../../../lib/types";

const PRIORITY_BADGE: Record<
  Priority,
  { label: string; className: string }
> = {
  p0: { label: "P0", className: "bg-red-900 text-red-300" },
  p1: { label: "P1", className: "bg-orange-900 text-orange-300" },
  p2: { label: "P2", className: "bg-blue-900 text-blue-300" },
  p3: { label: "P3", className: "bg-neutral-800 text-neutral-400" },
};

const STATUS_BADGE: Record<
  string,
  string
> = {
  inbox: "bg-neutral-800 text-neutral-400",
  next: "bg-blue-900 text-blue-300",
  active: "bg-emerald-900 text-emerald-300",
  waiting: "bg-yellow-900 text-yellow-300",
  done: "bg-teal-900 text-teal-300",
  cancelled: "bg-neutral-800 text-neutral-600",
};

function formatDate(dateStr: string): string {
  return new Date(dateStr).toLocaleDateString("en-US", {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

// Dependency item component — fetches entity/task details
function DependencyItem({
  entityId,
  onClick,
}: {
  entityId: string;
  onClick: (id: string) => void;
}) {
  const { data } = useEntity(entityId);
  if (!data) return null;
  return (
    <button
      onClick={() => onClick(entityId)}
      className="flex items-center gap-2 rounded bg-neutral-900 px-3 py-1.5 text-sm text-neutral-300 hover:bg-neutral-800"
    >
      <span className="text-neutral-500">▶</span>
      {data.name}
    </button>
  );
}

interface Props {
  taskId: string | null;
  onClose: () => void;
  onOpenTask?: (id: string) => void;
}

export function TaskDetailPanel({ taskId, onClose, onOpenTask }: Props) {
  const panelRef = useRef<HTMLDivElement>(null);
  const { data: task, isLoading, isError, error } = useTask(taskId);
  const { data: neighbors } = useNeighbors(taskId);

  // Filter neighbors by relation type
  const depends = (neighbors ?? []).filter(
    (n) => n.relation === "depends_on" && n.direction === "outbound",
  );

  // Close on Escape
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  const isOpen = taskId != null;

  return (
    <>
      {isOpen && (
        <div
          className="fixed inset-0 z-40 bg-black/30"
          aria-hidden="true"
          onClick={onClose}
        />
      )}
      <div
        ref={panelRef}
        className={[
          "fixed right-0 top-0 z-50 flex h-full w-[420px] flex-col border-l border-neutral-800 bg-neutral-950 shadow-2xl",
          "transition-transform duration-200",
          isOpen ? "translate-x-0" : "translate-x-full",
        ].join(" ")}
        aria-label="Task detail panel"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="flex items-start justify-between border-b border-neutral-800 px-4 py-3">
          <div className="flex-1 pr-4">
            {task ? (
              <h2 className="text-base font-semibold text-white">
                {task.title}
              </h2>
            ) : (
              <div className="h-5 w-3/4 animate-pulse rounded bg-neutral-800" />
            )}
            {task && (
              <div className="mt-1 flex items-center gap-2">
                <span
                  className={`rounded px-2 py-0.5 text-xs font-medium ${STATUS_BADGE[task.status] ?? "bg-neutral-800 text-neutral-400"}`}
                >
                  {task.status}
                </span>
                <span
                  className={`rounded px-2 py-0.5 text-xs font-bold ${PRIORITY_BADGE[task.priority].className}`}
                >
                  {PRIORITY_BADGE[task.priority].label}
                </span>
              </div>
            )}
          </div>
          <button
            onClick={onClose}
            className="rounded p-1 text-neutral-400 hover:bg-neutral-800 hover:text-white"
            aria-label="Close"
          >
            ✕
          </button>
        </div>

        {/* Body */}
        <div className="flex-1 overflow-y-auto p-4 text-sm">
          {isLoading && (
            <div className="space-y-3">
              {[...Array(6)].map((_, i) => (
                <div
                  key={i}
                  className="h-4 animate-pulse rounded bg-neutral-800"
                />
              ))}
            </div>
          )}

          {isError && (
            <div className="rounded border border-red-800 bg-red-950 p-3 text-red-300">
              Error: {error instanceof Error ? error.message : String(error)}
            </div>
          )}

          {task && (
            <div className="space-y-4">
              {/* Metadata grid */}
              <table className="w-full text-xs">
                <tbody>
                  {[
                    ["Assignee", task.assignee ?? "—"],
                    [
                      "Due",
                      task.due ? formatDate(task.due) : "—",
                    ],
                    ["Created", formatDate(task.created_at)],
                    ["Updated", formatDate(task.updated_at)],
                  ].map(([key, val]) => (
                    <tr key={key} className="border-t border-neutral-800 first:border-0">
                      <td className="py-1 pr-4 font-medium text-neutral-500">
                        {key}
                      </td>
                      <td className="py-1 text-neutral-300">{val}</td>
                    </tr>
                  ))}
                </tbody>
              </table>

              {/* Description */}
              {(task.description ?? task.properties?.description) && (
                <div>
                  <h3 className="mb-1 text-xs font-medium uppercase tracking-wider text-neutral-500">
                    Description
                  </h3>
                  <p className="text-neutral-300">
                    {task.description ?? task.properties?.description}
                  </p>
                </div>
              )}

              {/* Tags */}
              {task.tags && task.tags.length > 0 && (
                <div>
                  <h3 className="mb-1 text-xs font-medium uppercase tracking-wider text-neutral-500">
                    Tags
                  </h3>
                  <div className="flex flex-wrap gap-1">
                    {task.tags.map((tag) => (
                      <span
                        key={tag}
                        className="rounded bg-neutral-800 px-2 py-0.5 text-xs text-neutral-300"
                      >
                        {tag}
                      </span>
                    ))}
                  </div>
                </div>
              )}

              {/* Dependencies */}
              {depends.length > 0 && (
                <div>
                  <h3 className="mb-1 text-xs font-medium uppercase tracking-wider text-neutral-500">
                    Depends on ({depends.length})
                  </h3>
                  <div className="space-y-1">
                    {depends.map((dep) => (
                      <DependencyItem
                        key={dep.entity_id}
                        entityId={dep.entity_id}
                        onClick={onOpenTask ?? (() => {})}
                      />
                    ))}
                  </div>
                </div>
              )}

              {/* Timeline — ADR-047 §2.2 notes: falls back to created/updated when ADR-038 not shipped */}
              <div>
                <h3 className="mb-1 text-xs font-medium uppercase tracking-wider text-neutral-500">
                  Timeline
                </h3>
                <div className="space-y-1 text-xs text-neutral-400">
                  <div className="flex gap-2">
                    <span className="text-neutral-600">
                      {formatDate(task.created_at)}
                    </span>
                    <span>Created ({task.status})</span>
                  </div>
                  {task.created_at !== task.updated_at && (
                    <div className="flex gap-2">
                      <span className="text-neutral-600">
                        {formatDate(task.updated_at)}
                      </span>
                      <span>Last updated</span>
                    </div>
                  )}
                </div>
              </div>
            </div>
          )}
        </div>
      </div>
    </>
  );
}
