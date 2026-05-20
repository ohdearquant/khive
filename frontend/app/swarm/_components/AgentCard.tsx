"use client";

import Link from "next/link";
import type { AgentSummary, ThroughputBucket } from "@/lib/swarm/types";

// ---------------------------------------------------------------------------
// ThroughputSparkline — inline SVG, no external chart library (ADR-045 §D3)
// ---------------------------------------------------------------------------

interface ThroughputSparklineProps {
  buckets: ThroughputBucket[];
  height?: number;
  width?: number;
}

function ThroughputSparkline({ buckets, height = 40, width = 80 }: ThroughputSparklineProps) {
  const maxCount = Math.max(...buckets.map((b) => b.count), 1);
  const n = buckets.length;

  if (n === 0 || maxCount === 0) {
    // Flat baseline when no data
    return (
      <svg width={width} height={height} aria-label="No throughput data" role="img">
        <line x1={0} y1={height - 2} x2={width} y2={height - 2} stroke="#e5e7eb" strokeWidth={1} />
      </svg>
    );
  }

  const xStep = width / (n - 1 || 1);
  const yPad = 4;
  const usableHeight = height - yPad * 2;

  const points = buckets
    .map((b, i) => {
      const x = i * xStep;
      const y = yPad + usableHeight - (b.count / maxCount) * usableHeight;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");

  return (
    <svg
      width={width}
      height={height}
      aria-label={`Throughput: ${buckets.map((b) => b.count).join(", ")} tasks per 5-minute bucket`}
      role="img"
    >
      <polyline
        points={points}
        fill="none"
        stroke="#3b82f6"
        strokeWidth={1.5}
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

// ---------------------------------------------------------------------------
// Status indicator
// ---------------------------------------------------------------------------

type AgentStatus = "idle" | "active" | "bottleneck";

function getAgentStatus(agent: AgentSummary): AgentStatus {
  if (agent.activeTasks === 0) return "idle";
  if (agent.activeTasks + agent.nextTasks > 10) return "bottleneck";
  return "active";
}

const STATUS_DOT: Record<AgentStatus, string> = {
  idle: "bg-neutral-400",
  active: "bg-green-500",
  bottleneck: "bg-red-500",
};

const STATUS_LABEL: Record<AgentStatus, string> = {
  idle: "Idle",
  active: "Active",
  bottleneck: "Bottleneck",
};

// ---------------------------------------------------------------------------
// Revert rate chip color
// ---------------------------------------------------------------------------

function revertRateColor(rate: number): string {
  if (rate < 0.05) return "text-green-700 bg-green-100";
  if (rate < 0.15) return "text-yellow-700 bg-yellow-100";
  return "text-red-700 bg-red-100";
}

// ---------------------------------------------------------------------------
// Format duration
// ---------------------------------------------------------------------------

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  if (ms < 3_600_000) return `${(ms / 60_000).toFixed(1)}m`;
  return `${(ms / 3_600_000).toFixed(1)}h`;
}

// ---------------------------------------------------------------------------
// AgentCard
// ---------------------------------------------------------------------------

export interface AgentCardProps {
  agent: AgentSummary;
  throughputBuckets: ThroughputBucket[];
  selected?: boolean;
  onClick?: () => void;
}

export default function AgentCard({
  agent,
  throughputBuckets,
  selected = false,
  onClick,
}: AgentCardProps) {
  const status = getAgentStatus(agent);

  return (
    <article
      onClick={onClick}
      className={[
        "cursor-pointer select-none rounded-xl border p-4 transition-colors",
        selected
          ? "border-blue-500 bg-blue-50 shadow-sm"
          : "border-neutral-200 bg-white hover:border-neutral-300 hover:shadow-sm",
      ].join(" ")}
      aria-label={`Agent ${agent.name} — ${STATUS_LABEL[status]}`}
    >
      {/* Header row */}
      <div className="flex items-center justify-between gap-2">
        <Link
          href={`/swarm/${encodeURIComponent(agent.name)}`}
          className="font-mono text-sm font-semibold text-neutral-900 hover:text-blue-600"
          onClick={(e) => e.stopPropagation()}
        >
          {agent.name}
        </Link>
        <span
          className={`flex h-2 w-2 rounded-full ${STATUS_DOT[status]}`}
          title={STATUS_LABEL[status]}
          aria-label={STATUS_LABEL[status]}
        />
      </div>

      {/* Counts */}
      <div className="mt-2 flex gap-3 text-xs text-neutral-600">
        <span>
          <span className="font-semibold text-neutral-900">{agent.activeTasks}</span> active
        </span>
        <span>
          <span className="font-semibold text-neutral-900">{agent.nextTasks}</span> next
        </span>
        <span>
          <span className="font-semibold text-neutral-900">{agent.completedLast1h}</span>
          /h
        </span>
      </div>

      {/* Sparkline */}
      <div className="mt-3">
        <ThroughputSparkline buckets={throughputBuckets} width={120} height={36} />
      </div>

      {/* Duration + revert rate */}
      <div className="mt-2 flex items-center justify-between text-xs">
        {agent.meanDurationMs !== null ? (
          <span className="text-neutral-500">avg {formatDuration(agent.meanDurationMs)}</span>
        ) : (
          <span className="text-neutral-400">no duration data</span>
        )}

        <span
          className={`rounded-full px-2 py-0.5 font-mono ${revertRateColor(agent.revertRate)}`}
          title={`Revert rate: ${(agent.revertRate * 100).toFixed(1)}%`}
        >
          {(agent.revertRate * 100).toFixed(1)}% rev
        </span>
      </div>
    </article>
  );
}
