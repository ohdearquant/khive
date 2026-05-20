"use client";

import { useState } from "react";
import type { HeatmapCell } from "@/lib/swarm/types";

// ---------------------------------------------------------------------------
// Color scale: 5 steps from white → amber → red
// Colorblind-safe: uses neutral → blue → orange → red progression
// that reads clearly under deuteranopia and protanopia (ADR-045 §BottleneckHeatmap)
// ---------------------------------------------------------------------------

function cellColor(depth: number): string {
  if (depth === 0) return "bg-neutral-100";
  if (depth <= 3) return "bg-blue-200";
  if (depth <= 7) return "bg-blue-400";
  if (depth <= 15) return "bg-orange-400";
  return "bg-red-500";
}

function cellTextColor(depth: number): string {
  if (depth <= 3) return "text-neutral-700";
  if (depth <= 7) return "text-white";
  return "text-white";
}

// ---------------------------------------------------------------------------
// Format a bucket label for the column header
// E.g. "2026-05-20T14:00Z" → "14:00"
// ---------------------------------------------------------------------------

function formatBucketLabel(iso: string): string {
  try {
    const d = new Date(iso);
    return d.toLocaleTimeString(undefined, {
      hour: "2-digit",
      minute: "2-digit",
      hour12: false,
    });
  } catch {
    return iso.slice(11, 16);
  }
}

// ---------------------------------------------------------------------------
// Tooltip state
// ---------------------------------------------------------------------------

interface TooltipState {
  agentName: string;
  timeBucket: string;
  queueDepth: number;
  x: number;
  y: number;
}

// ---------------------------------------------------------------------------
// Color legend entries
// ---------------------------------------------------------------------------

const LEGEND = [
  { label: "0", color: "bg-neutral-100" },
  { label: "1–3", color: "bg-blue-200" },
  { label: "4–7", color: "bg-blue-400" },
  { label: "8–15", color: "bg-orange-400" },
  { label: "16+", color: "bg-red-500" },
];

// ---------------------------------------------------------------------------
// BottleneckHeatmap
// ---------------------------------------------------------------------------

export interface BottleneckHeatmapProps {
  /** Outer array: one row per agent; inner array: one cell per time bucket */
  cells: HeatmapCell[][];
  agents: string[];
  buckets: string[];
}

export default function BottleneckHeatmap({ cells, agents, buckets }: BottleneckHeatmapProps) {
  const [tooltip, setTooltip] = useState<TooltipState | null>(null);

  if (agents.length === 0 || buckets.length === 0) {
    return (
      <div className="flex h-32 items-center justify-center rounded-lg border border-dashed border-neutral-300 text-sm text-neutral-500">
        No heatmap data.
      </div>
    );
  }

  // Show every 4th bucket label to avoid overcrowding
  const labelEvery = Math.ceil(buckets.length / 8);

  return (
    <div className="relative overflow-x-auto">
      {/* Color legend */}
      <div className="mb-2 flex items-center gap-3 text-xs text-neutral-600">
        <span className="font-medium">Queue depth:</span>
        {LEGEND.map(({ label, color }) => (
          <span key={label} className="flex items-center gap-1">
            <span
              className={`inline-block h-3 w-4 rounded-sm border border-neutral-200 ${color}`}
              aria-hidden="true"
            />
            {label}
          </span>
        ))}
      </div>

      {/* Grid */}
      <div
        className="grid"
        style={{
          gridTemplateColumns: `8rem repeat(${buckets.length}, minmax(1.5rem, 2rem))`,
        }}
        role="grid"
        aria-label="Queue depth heatmap by agent and time bucket"
      >
        {/* Header row: empty corner + bucket labels */}
        <div role="columnheader" aria-label="Agent" />
        {buckets.map((bucket, bi) => (
          <div
            key={bucket}
            role="columnheader"
            className="px-0.5 text-center text-xs text-neutral-500"
            title={bucket}
          >
            {bi % labelEvery === 0 ? formatBucketLabel(bucket) : ""}
          </div>
        ))}

        {/* Data rows */}
        {agents.map((agent, ai) => (
          <>
            {/* Row header */}
            <div
              key={`row-${agent}`}
              role="rowheader"
              className="flex items-center pr-2 text-right font-mono text-xs text-neutral-700"
            >
              <span className="max-w-[7.5rem] overflow-hidden text-ellipsis whitespace-nowrap">
                {agent}
              </span>
            </div>

            {/* Data cells */}
            {(cells[ai] ?? []).map((cell, bi) => (
              <div
                key={`${agent}-${buckets[bi]}`}
                role="gridcell"
                className={[
                  "mx-0.5 my-0.5 h-6 cursor-default rounded-sm",
                  cellColor(cell.queueDepth),
                  cellTextColor(cell.queueDepth),
                  "flex items-center justify-center font-mono text-xs leading-none",
                ].join(" ")}
                aria-label={`${agent} at ${formatBucketLabel(buckets[bi])}: ${cell.queueDepth} queued`}
                tabIndex={0}
                onMouseEnter={(e) => {
                  const rect = (e.target as HTMLElement).getBoundingClientRect();
                  setTooltip({
                    agentName: cell.agentName,
                    timeBucket: cell.timeBucket,
                    queueDepth: cell.queueDepth,
                    x: rect.left,
                    y: rect.top,
                  });
                }}
                onMouseLeave={() => setTooltip(null)}
                onFocus={(e) => {
                  const rect = (e.target as HTMLElement).getBoundingClientRect();
                  setTooltip({
                    agentName: cell.agentName,
                    timeBucket: cell.timeBucket,
                    queueDepth: cell.queueDepth,
                    x: rect.left,
                    y: rect.top,
                  });
                }}
                onBlur={() => setTooltip(null)}
              >
                {cell.queueDepth > 0 ? cell.queueDepth : ""}
              </div>
            ))}
          </>
        ))}
      </div>

      {/* Tooltip */}
      {tooltip !== null && (
        <div
          className="pointer-events-none fixed z-50 -translate-x-1/2 -translate-y-full rounded-md border border-neutral-200 bg-white px-2 py-1 text-xs shadow-lg"
          style={{ left: tooltip.x, top: tooltip.y - 4 }}
          role="tooltip"
        >
          <div className="font-semibold">{tooltip.agentName}</div>
          <div className="text-neutral-500">{tooltip.timeBucket}</div>
          <div>{tooltip.queueDepth} queued</div>
        </div>
      )}
    </div>
  );
}
