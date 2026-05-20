"use client";

import type { ThroughputBucket } from "@/lib/swarm/types";

// ---------------------------------------------------------------------------
// AgentThroughputChart — SVG line chart for per-agent throughput over time
// Uses inline SVG, no Recharts, consistent with sparkline approach in AgentCard
// ---------------------------------------------------------------------------

interface AgentThroughputChartProps {
  buckets: ThroughputBucket[];
  height?: number;
}

function formatBucketTime(ts: number): string {
  return new Date(ts).toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

export default function AgentThroughputChart({ buckets, height = 160 }: AgentThroughputChartProps) {
  const PADDING = { top: 16, right: 16, bottom: 32, left: 40 };

  if (buckets.length < 2) {
    return (
      <div
        style={{ height }}
        className="flex items-center justify-center rounded-lg border border-dashed border-neutral-300 text-sm text-neutral-500"
      >
        Not enough data to display throughput chart.
      </div>
    );
  }

  const maxCount = Math.max(...buckets.map((b) => b.count), 1);
  const n = buckets.length;
  const svgWidth = "100%";
  // We use a viewBox to make this responsive
  const viewBoxWidth = 500;
  const viewBoxHeight = height;
  const chartW = viewBoxWidth - PADDING.left - PADDING.right;
  const chartH = viewBoxHeight - PADDING.top - PADDING.bottom;
  const xStep = chartW / (n - 1);

  const points = buckets.map((b, i) => {
    const x = PADDING.left + i * xStep;
    const y = PADDING.top + chartH - (b.count / maxCount) * chartH;
    return { x, y, bucket: b };
  });

  const polylinePoints = points.map((p) => `${p.x.toFixed(1)},${p.y.toFixed(1)}`).join(" ");

  // Area path for fill-under
  const firstPt = points[0];
  const lastPt = points[points.length - 1];
  const areaPath = [
    `M ${firstPt.x.toFixed(1)} ${(PADDING.top + chartH).toFixed(1)}`,
    ...points.map((p) => `L ${p.x.toFixed(1)} ${p.y.toFixed(1)}`),
    `L ${lastPt.x.toFixed(1)} ${(PADDING.top + chartH).toFixed(1)}`,
    "Z",
  ].join(" ");

  // Y-axis tick values
  const yTicks = [0, Math.round(maxCount / 2), maxCount];

  // X-axis labels: show first, middle, last
  const xLabelIndices = [0, Math.floor(n / 2), n - 1];

  return (
    <svg
      width={svgWidth}
      height={viewBoxHeight}
      viewBox={`0 0 ${viewBoxWidth} ${viewBoxHeight}`}
      preserveAspectRatio="xMidYMid meet"
      role="img"
      aria-label="Agent throughput over time"
    >
      <title>Agent throughput (tasks completed per 5-minute bucket)</title>

      {/* Grid lines */}
      {yTicks.map((tick) => {
        const y = PADDING.top + chartH - (tick / maxCount) * chartH;
        return (
          <g key={tick}>
            <line
              x1={PADDING.left}
              y1={y}
              x2={PADDING.left + chartW}
              y2={y}
              stroke="#f3f4f6"
              strokeWidth={1}
            />
            <text
              x={PADDING.left - 4}
              y={y + 4}
              textAnchor="end"
              fontSize={10}
              className="fill-neutral-400"
            >
              {tick}
            </text>
          </g>
        );
      })}

      {/* Axes */}
      <line
        x1={PADDING.left}
        y1={PADDING.top}
        x2={PADDING.left}
        y2={PADDING.top + chartH}
        stroke="#e5e7eb"
        strokeWidth={1}
      />
      <line
        x1={PADDING.left}
        y1={PADDING.top + chartH}
        x2={PADDING.left + chartW}
        y2={PADDING.top + chartH}
        stroke="#e5e7eb"
        strokeWidth={1}
      />

      {/* Area fill */}
      <path d={areaPath} fill="#eff6ff" />

      {/* Line */}
      <polyline
        points={polylinePoints}
        fill="none"
        stroke="#3b82f6"
        strokeWidth={2}
        strokeLinecap="round"
        strokeLinejoin="round"
      />

      {/* Data dots */}
      {points.map((p, i) => (
        <circle
          key={i}
          cx={p.x}
          cy={p.y}
          r={3}
          fill="#3b82f6"
          aria-label={`${formatBucketTime(p.bucket.ts)}: ${p.bucket.count} tasks`}
        />
      ))}

      {/* X-axis labels */}
      {xLabelIndices.map((idx) => {
        const p = points[idx];
        if (!p) return null;
        return (
          <text
            key={idx}
            x={p.x}
            y={PADDING.top + chartH + 20}
            textAnchor="middle"
            fontSize={10}
            className="fill-neutral-500"
          >
            {formatBucketTime(p.bucket.ts)}
          </text>
        );
      })}
    </svg>
  );
}
