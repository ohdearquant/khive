"use client";

import type { CycleBucket } from "@/lib/swarm/types";

// ---------------------------------------------------------------------------
// Color per task status
// Colorblind-safe palette (ADR-045 §D3 — Tailwind semantic colors)
// ---------------------------------------------------------------------------

const STATUS_COLOR: Record<string, string> = {
  active: "#3b82f6", // blue-500
  done: "#22c55e", // green-500
  waiting: "#f59e0b", // amber-500
  cancelled: "#9ca3af", // gray-400
  next: "#a78bfa", // violet-400
  inbox: "#d1d5db", // gray-300
  someday: "#e5e7eb", // gray-200
};

function statusColor(status: string): string {
  return STATUS_COLOR[status] ?? "#e5e7eb";
}

// ---------------------------------------------------------------------------
// CycleTimeline — SVG stacked bar chart (inline, no Recharts for v0)
// ---------------------------------------------------------------------------

export interface CycleTimelineProps {
  cycles: CycleBucket[];
  height?: number;
}

export default function CycleTimeline({ cycles, height = 200 }: CycleTimelineProps) {
  if (cycles.length === 0) {
    return (
      <div
        style={{ height }}
        className="flex items-center justify-center rounded-lg border border-dashed border-neutral-300 text-sm text-neutral-500"
      >
        No cycle data. Tag tasks with &quot;cycle:N&quot; to track progression.
      </div>
    );
  }

  const PADDING = { top: 16, right: 16, bottom: 40, left: 24 };
  const barWidth = 40;
  const barGap = 20;
  const totalWidth = cycles.length * (barWidth + barGap) - barGap + PADDING.left + PADDING.right;
  const chartHeight = height - PADDING.top - PADDING.bottom;

  // Find the max total per cycle for y-scaling
  const maxTotal = Math.max(...cycles.map((c) => c.counts.reduce((s, e) => s + e.count, 0)), 1);

  // Collect all unique statuses for legend
  const statuses = Array.from(new Set(cycles.flatMap((c) => c.counts.map((e) => e.status))));

  return (
    <div>
      <svg
        width={totalWidth}
        height={height}
        role="img"
        aria-label="Cycle progression timeline"
        className="overflow-visible"
      >
        <title>Cycle progression timeline</title>
        <desc>Stacked bar chart showing task counts by status across swarm cycles.</desc>

        {/* Y-axis baseline */}
        <line
          x1={PADDING.left}
          y1={PADDING.top}
          x2={PADDING.left}
          y2={PADDING.top + chartHeight}
          stroke="#e5e7eb"
          strokeWidth={1}
        />

        {/* X-axis baseline */}
        <line
          x1={PADDING.left}
          y1={PADDING.top + chartHeight}
          x2={totalWidth - PADDING.right}
          y2={PADDING.top + chartHeight}
          stroke="#e5e7eb"
          strokeWidth={1}
        />

        {/* Stacked bars */}
        {cycles.map((cycle, ci) => {
          const barX = PADDING.left + ci * (barWidth + barGap);
          let stackY = PADDING.top + chartHeight; // start from bottom

          // Sort counts so statuses appear consistently bottom-up
          const sortedCounts = [...cycle.counts].sort((a, b) => b.count - a.count);

          return (
            <g key={cycle.cycleLabel}>
              {sortedCounts.map(({ status, count }) => {
                const barH = (count / maxTotal) * chartHeight;
                stackY -= barH;
                const y = stackY;

                return (
                  <rect
                    key={status}
                    x={barX}
                    y={y}
                    width={barWidth}
                    height={barH}
                    fill={statusColor(status)}
                    role="img"
                    aria-label={`${cycle.cycleLabel}: ${count} ${status} tasks`}
                  >
                    <title>
                      {cycle.cycleLabel}: {count} {status}
                    </title>
                  </rect>
                );
              })}

              {/* X-axis label */}
              <text
                x={barX + barWidth / 2}
                y={PADDING.top + chartHeight + 16}
                textAnchor="middle"
                className="fill-neutral-600 text-xs"
                fontSize={11}
              >
                {cycle.cycleLabel === "none" ? "untagged" : cycle.cycleLabel}
              </text>
            </g>
          );
        })}
      </svg>

      {/* Legend */}
      <div className="mt-2 flex flex-wrap gap-3">
        {statuses.map((status) => (
          <span key={status} className="flex items-center gap-1 text-xs text-neutral-600">
            <span
              className="inline-block h-2.5 w-2.5 rounded-sm"
              style={{ background: statusColor(status) }}
              aria-hidden="true"
            />
            {status}
          </span>
        ))}
      </div>
    </div>
  );
}
