"use client";

import AgentQueueTable from "./AgentQueueTable";
import AgentTaskList from "./AgentTaskList";
import AgentThroughputChart from "./AgentThroughputChart";
import { useAgentDrilldownContext } from "./AgentDrilldownContext";

interface AgentDrilldownViewProps {
  agentName: string;
}

export default function AgentDrilldownView({ agentName }: AgentDrilldownViewProps) {
  const { state } = useAgentDrilldownContext();

  const totalQueued = state.activeTasks.length + state.nextTasks.length;
  const statusLabel = state.activeTasks.length > 0 ? "Active" : "Idle";
  const statusBadge =
    state.activeTasks.length > 0
      ? "bg-green-100 text-green-700"
      : "bg-neutral-100 text-neutral-600";

  return (
    <div className="space-y-8 p-4 lg:p-8">
      {/* Agent header */}
      <header className="flex flex-wrap items-center gap-4">
        <h1 className="font-mono text-2xl font-bold text-neutral-900">{agentName}</h1>
        <span className={`rounded-full px-2.5 py-0.5 text-xs font-semibold ${statusBadge}`}>
          {statusLabel}
          {totalQueued > 0 && ` — ${totalQueued} task${totalQueued !== 1 ? "s" : ""}`}
        </span>
      </header>

      {/* Error banner */}
      {state.error && (
        <div
          role="alert"
          className="rounded-md border border-red-200 bg-red-50 px-4 py-3 text-sm text-red-700"
        >
          <strong>Error:</strong> {state.error}
        </div>
      )}

      {/* Throughput chart */}
      <section aria-labelledby="throughput-heading">
        <h2
          id="throughput-heading"
          className="mb-3 text-sm font-semibold uppercase tracking-wide text-neutral-500"
        >
          Throughput (last hour)
        </h2>
        <AgentThroughputChart buckets={state.throughputBuckets} height={160} />
      </section>

      {/* Active queue */}
      <section aria-labelledby="queue-heading">
        <h2
          id="queue-heading"
          className="mb-3 text-sm font-semibold uppercase tracking-wide text-neutral-500"
        >
          Active Queue
        </h2>
        <AgentQueueTable activeTasks={state.activeTasks} nextTasks={state.nextTasks} />
      </section>

      {/* Recent completions */}
      <section aria-labelledby="completions-heading">
        <h2
          id="completions-heading"
          className="mb-3 text-sm font-semibold uppercase tracking-wide text-neutral-500"
        >
          Recent Completions (last 20)
        </h2>
        <AgentTaskList tasks={state.recentCompletions} />
      </section>
    </div>
  );
}
