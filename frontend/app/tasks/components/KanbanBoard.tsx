"use client";

import { useTasks } from "../../../lib/api";
import type { Task, TaskStatus } from "../../../lib/types";
import { BOARD_STATUSES } from "../../../lib/types";
import { TaskCard } from "./TaskCard";

const COLUMN_LABEL: Record<TaskStatus, string> = {
  inbox: "Inbox",
  next: "Next",
  active: "Active",
  waiting: "Waiting",
  done: "Done",
  cancelled: "Cancelled",
};

// Done and cancelled are collapsed by default — show only first 5 cards
const COLLAPSED_BY_DEFAULT: Set<TaskStatus> = new Set(["done", "cancelled"]);
const COLLAPSED_PREVIEW = 5;

interface ColumnProps {
  status: TaskStatus;
  assignee?: string;
  priority?: string;
  selectedId: string | null;
  onSelect: (task: Task) => void;
}

function KanbanColumn({
  status,
  assignee,
  priority,
  selectedId: _selectedId,
  onSelect,
}: ColumnProps) {
  const { data, isLoading, isError, refetch } = useTasks({
    status,
    assignee: assignee || undefined,
    priority: priority || undefined,
    limit: 50,
  });

  const items = data?.items ?? [];
  const isCollapsed = COLLAPSED_BY_DEFAULT.has(status);
  const visible = isCollapsed ? items.slice(0, COLLAPSED_PREVIEW) : items;
  const hiddenCount = isCollapsed ? items.length - COLLAPSED_PREVIEW : 0;

  return (
    <div className="flex min-w-0 flex-1 flex-col">
      {/* Column header */}
      <div className="mb-2 flex items-center justify-between rounded border border-neutral-800 bg-neutral-900 px-3 py-2">
        <span className="font-medium text-neutral-300">
          {COLUMN_LABEL[status]}
        </span>
        <span className="rounded bg-neutral-800 px-1.5 py-0.5 text-xs font-mono text-neutral-500">
          {isLoading ? "…" : (data?.total ?? items.length)}
        </span>
      </div>

      {/* Cards */}
      <div className="flex flex-col gap-2">
        {isLoading && (
          <>
            {[...Array(3)].map((_, i) => (
              <div
                key={i}
                className="h-20 animate-pulse rounded border border-neutral-800 bg-neutral-900"
              />
            ))}
          </>
        )}

        {isError && (
          <div className="rounded border border-red-900 bg-red-950 p-2 text-xs text-red-400">
            Load error.{" "}
            <button onClick={() => refetch()} className="underline">
              Retry
            </button>
          </div>
        )}

        {!isLoading && items.length === 0 && (
          <div className="rounded border border-dashed border-neutral-800 p-3 text-center text-xs text-neutral-700">
            empty
          </div>
        )}

        {visible.map((task) => (
          <TaskCard
            key={task.id}
            task={task}
            onClick={onSelect}
          />
        ))}

        {isCollapsed && hiddenCount > 0 && (
          <div className="text-center text-xs text-neutral-600">
            +{hiddenCount} more
          </div>
        )}
      </div>
    </div>
  );
}

interface Props {
  assignee?: string;
  priority?: string;
  selectedTaskId: string | null;
  onSelectTask: (task: Task) => void;
}

export function KanbanBoard({
  assignee,
  priority,
  selectedTaskId,
  onSelectTask,
}: Props) {
  return (
    <div className="grid grid-cols-6 gap-3">
      {BOARD_STATUSES.map((status) => (
        <KanbanColumn
          key={status}
          status={status}
          assignee={assignee}
          priority={priority}
          selectedId={selectedTaskId}
          onSelect={onSelectTask}
        />
      ))}
    </div>
  );
}
