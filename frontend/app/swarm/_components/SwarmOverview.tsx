"use client";

import { useState } from "react";
import AgentCard from "./AgentCard";
import BottleneckHeatmap from "./BottleneckHeatmap";
import CycleTimeline from "./CycleTimeline";
import DriftAlerts from "./DriftAlerts";
import HandoffDag from "./HandoffDag";
import { useSwarmContext } from "./SwarmContext";

// ---------------------------------------------------------------------------
// Timestamp formatting
// ---------------------------------------------------------------------------

function fmtTimestamp(ms: number): string {
  return new Date(ms).toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

// ---------------------------------------------------------------------------
// SwarmOverview
// ---------------------------------------------------------------------------

export default function SwarmOverview() {
  const { state, dispatch } = useSwarmContext();
  const [selectedAgent, setSelectedAgent] = useState<string | undefined>(undefined);

  const hasActive = state.agents.some((a) => a.activeTasks > 0);

  // Blank completed list for sparkline pre-boot (we derive from state once data arrives)
  // In a full implementation, SwarmContext would expose completedTasks per agent.
  // Here we derive stub buckets from completedLast1h count distributed evenly.
  function buildSparklineBuckets(agentName: string) {
    // Generate synthetic throughput buckets from the completedLast1h count
    // distributed uniformly across 12 × 5-minute buckets.
    // The real implementation would store per-bucket counts in SwarmContext.
    const agent = state.agents.find((a) => a.name === agentName);
    if (!agent) return [];

    const total = agent.completedLast1h;
    const perBucket = Math.floor(total / 12);
    const remainder = total % 12;
    const now = Date.now();

    return Array.from({ length: 12 }, (_, i) => ({
      ts: now - (11 - i) * 5 * 60 * 1000,
      count: perBucket + (i < remainder ? 1 : 0),
    }));
  }

  return (
    <div className="space-y-8 p-4 lg:p-8">
      {/* Header */}
      <header className="flex flex-wrap items-center justify-between gap-4">
        <div className="flex items-center gap-3">
          <h1 className="text-2xl font-bold text-neutral-900">Swarm</h1>
          {hasActive && (
            <span className="rounded-full bg-green-100 px-2.5 py-0.5 text-xs font-semibold text-green-700">
              Active swarm
            </span>
          )}
          {!hasActive && state.agents.length > 0 && (
            <span className="rounded-full bg-neutral-100 px-2.5 py-0.5 text-xs font-semibold text-neutral-600">
              Idle
            </span>
          )}
        </div>

        <div className="flex items-center gap-4 text-sm text-neutral-500">
          {state.lastUpdated > 0 && <span>Last updated: {fmtTimestamp(state.lastUpdated)}</span>}
          <button
            className="rounded-md border border-neutral-200 bg-white px-2 py-1 text-xs hover:bg-neutral-50"
            aria-label="Refresh now"
            onClick={() => dispatch({ type: "CLEAR_ERROR" })}
          >
            Refresh
          </button>
        </div>
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

      {/* Agents grid */}
      <section aria-labelledby="agents-heading">
        <h2
          id="agents-heading"
          className="mb-3 text-sm font-semibold uppercase tracking-wide text-neutral-500"
        >
          Agents
        </h2>

        {state.agents.length === 0 ? (
          <p className="text-sm text-neutral-500">
            No active agents found. Start a swarm to see agent activity.
          </p>
        ) : (
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
            {state.agents.map((agent) => (
              <AgentCard
                key={agent.name}
                agent={agent}
                throughputBuckets={buildSparklineBuckets(agent.name)}
                selected={selectedAgent === agent.name}
                onClick={() =>
                  setSelectedAgent(selectedAgent === agent.name ? undefined : agent.name)
                }
              />
            ))}
          </div>
        )}
      </section>

      {/* Handoff DAG */}
      <section aria-labelledby="dag-heading">
        <h2
          id="dag-heading"
          className="mb-3 text-sm font-semibold uppercase tracking-wide text-neutral-500"
        >
          Handoff DAG
        </h2>
        <HandoffDag
          edges={state.handoffs}
          agentSummaries={state.agents.map((a) => ({
            name: a.name,
            activeTasks: a.activeTasks,
          }))}
          selectedAgent={selectedAgent}
        />
      </section>

      {/* Two-column: cycle timeline + drift alerts */}
      <div className="grid grid-cols-1 gap-8 lg:grid-cols-2">
        <section aria-labelledby="cycle-heading">
          <h2
            id="cycle-heading"
            className="mb-3 text-sm font-semibold uppercase tracking-wide text-neutral-500"
          >
            Cycle Timeline
          </h2>
          <CycleTimeline cycles={state.cycles} />
        </section>

        <section aria-labelledby="drift-heading">
          <h2
            id="drift-heading"
            className="mb-3 text-sm font-semibold uppercase tracking-wide text-neutral-500"
          >
            Drift Alerts
          </h2>
          <DriftAlerts alerts={state.driftAlerts} />
        </section>
      </div>

      {/* Bottleneck heatmap */}
      <section aria-labelledby="heatmap-heading">
        <h2
          id="heatmap-heading"
          className="mb-3 text-sm font-semibold uppercase tracking-wide text-neutral-500"
        >
          Queue Depth Heatmap (last 24h)
        </h2>
        <BottleneckHeatmap
          cells={state.heatmap}
          agents={state.heatmapAgents}
          buckets={state.heatmapBuckets}
        />
      </section>
    </div>
  );
}
