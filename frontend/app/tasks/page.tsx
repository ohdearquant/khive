"use client";

import { useState, useCallback } from "react";
import { useSearchParams, useRouter } from "next/navigation";
import { KanbanBoard } from "./components/KanbanBoard";
import { TaskDetailPanel } from "./components/TaskDetailPanel";
import type { Task } from "../../lib/types";

const PRIORITY_OPTIONS = [
  { value: "", label: "All priorities" },
  { value: "p0", label: "P0 — Critical" },
  { value: "p1", label: "P1 — High" },
  { value: "p2", label: "P2 — Normal" },
  { value: "p3", label: "P3 — Low" },
];

export default function GTDBoardPage() {
  const searchParams = useSearchParams();
  const router = useRouter();

  const initialTaskId = searchParams.get("task");
  const initialAssignee = searchParams.get("assignee") ?? "";
  const initialPriority = searchParams.get("priority") ?? "";

  const [assigneeFilter, setAssigneeFilter] = useState(initialAssignee);
  const [assigneeInput, setAssigneeInput] = useState(initialAssignee);
  const [priorityFilter, setPriorityFilter] = useState(initialPriority);
  const [selectedTaskId, setSelectedTaskId] = useState<string | null>(
    initialTaskId,
  );

  const handleSelectTask = useCallback(
    (task: Task) => {
      setSelectedTaskId(task.id);
      const params = new URLSearchParams(searchParams.toString());
      params.set("task", task.id);
      router.replace(`/tasks?${params.toString()}`, { scroll: false });
    },
    [searchParams, router],
  );

  const handleClosePanel = useCallback(() => {
    setSelectedTaskId(null);
    const params = new URLSearchParams(searchParams.toString());
    params.delete("task");
    router.replace(`/tasks?${params.toString()}`, { scroll: false });
  }, [searchParams, router]);

  const handleOpenTask = useCallback(
    (id: string) => {
      setSelectedTaskId(id);
      const params = new URLSearchParams(searchParams.toString());
      params.set("task", id);
      router.replace(`/tasks?${params.toString()}`, { scroll: false });
    },
    [searchParams, router],
  );

  function applyAssignee() {
    const val = assigneeInput.trim();
    setAssigneeFilter(val);
    const params = new URLSearchParams(searchParams.toString());
    if (val) params.set("assignee", val);
    else params.delete("assignee");
    router.replace(`/tasks?${params.toString()}`, { scroll: false });
  }

  function applyPriority(val: string) {
    setPriorityFilter(val);
    const params = new URLSearchParams(searchParams.toString());
    if (val) params.set("priority", val);
    else params.delete("priority");
    router.replace(`/tasks?${params.toString()}`, { scroll: false });
  }

  return (
    <div className="flex flex-col p-6">
      {/* Header */}
      <div className="mb-4 flex items-center justify-between">
        <div>
          <h1 className="text-xl font-bold text-white">GTD Board</h1>
          <p className="text-sm text-neutral-500">
            Task queue — read-only view (phase 1)
          </p>
        </div>
      </div>

      {/* Filters */}
      <div className="mb-4 flex flex-wrap items-center gap-3">
        {/* Assignee filter */}
        <div className="flex items-center gap-2">
          <label className="text-xs text-neutral-500">Assignee:</label>
          <input
            type="text"
            value={assigneeInput}
            onChange={(e) => setAssigneeInput(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && applyAssignee()}
            placeholder="all"
            className="w-40 rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-xs text-neutral-100 placeholder-neutral-600 focus:border-neutral-500 focus:outline-none"
          />
          <button
            onClick={applyAssignee}
            className="rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-xs text-neutral-400 hover:text-neutral-200"
          >
            Apply
          </button>
          {assigneeFilter && (
            <button
              onClick={() => {
                setAssigneeInput("");
                setAssigneeFilter("");
                const params = new URLSearchParams(searchParams.toString());
                params.delete("assignee");
                router.replace(`/tasks?${params.toString()}`, {
                  scroll: false,
                });
              }}
              className="text-xs text-neutral-600 hover:text-neutral-400"
            >
              ✕
            </button>
          )}
        </div>

        {/* Priority filter */}
        <div className="flex items-center gap-2">
          <label className="text-xs text-neutral-500">Priority:</label>
          <select
            value={priorityFilter}
            onChange={(e) => applyPriority(e.target.value)}
            className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-xs text-neutral-100 focus:border-neutral-500 focus:outline-none"
          >
            {PRIORITY_OPTIONS.map((opt) => (
              <option key={opt.value} value={opt.value}>
                {opt.label}
              </option>
            ))}
          </select>
        </div>

        {/* Active filter pills */}
        {(assigneeFilter || priorityFilter) && (
          <div className="flex gap-1">
            {assigneeFilter && (
              <span className="rounded bg-neutral-800 px-2 py-0.5 text-xs text-neutral-400">
                assignee: {assigneeFilter}
              </span>
            )}
            {priorityFilter && (
              <span className="rounded bg-neutral-800 px-2 py-0.5 text-xs text-neutral-400">
                priority: {priorityFilter}
              </span>
            )}
          </div>
        )}
      </div>

      {/* Board — horizontally scrollable on narrow viewports */}
      <div className="min-w-0 overflow-x-auto">
        <div className="min-w-[900px]">
          <KanbanBoard
            assignee={assigneeFilter || undefined}
            priority={priorityFilter || undefined}
            selectedTaskId={selectedTaskId}
            onSelectTask={handleSelectTask}
          />
        </div>
      </div>

      {/* Task detail drawer */}
      <TaskDetailPanel
        taskId={selectedTaskId}
        onClose={handleClosePanel}
        onOpenTask={handleOpenTask}
      />
    </div>
  );
}
