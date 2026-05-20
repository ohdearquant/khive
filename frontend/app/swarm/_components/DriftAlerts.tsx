"use client";

import Link from "next/link";
import type { DriftAlert } from "@/lib/swarm/types";

// ---------------------------------------------------------------------------
// Color coding for revert rate (matches AgentCard)
// ---------------------------------------------------------------------------

function rateColor(rate: number): {
  badge: string;
  icon: string;
} {
  if (rate < 0.05) return { badge: "text-green-700 bg-green-100", icon: "text-green-500" };
  if (rate < 0.15) return { badge: "text-yellow-700 bg-yellow-100", icon: "text-yellow-500" };
  return { badge: "text-red-700 bg-red-100", icon: "text-red-500" };
}

// ---------------------------------------------------------------------------
// DriftAlerts
// ---------------------------------------------------------------------------

export interface DriftAlertsProps {
  alerts: DriftAlert[];
}

export default function DriftAlerts({ alerts }: DriftAlertsProps) {
  if (alerts.length === 0) {
    return (
      <p className="text-sm text-neutral-500">
        No drift alerts. All agents are within the 5% revert threshold.
      </p>
    );
  }

  // Sort highest rate first (already done in derive.ts, but be defensive)
  const sorted = [...alerts].sort((a, b) => b.revertRate - a.revertRate);

  return (
    <ul className="space-y-2" aria-label="Drift alerts — agents with elevated revert rates">
      {sorted.map((alert) => {
        const { badge } = rateColor(alert.revertRate);

        return (
          <li
            key={alert.agentName}
            className="flex items-center justify-between gap-4 rounded-lg border border-neutral-200 bg-white px-3 py-2 text-sm"
          >
            <div className="flex min-w-0 items-center gap-2">
              {/* Alert icon (SVG, accessible) */}
              <svg
                className="h-4 w-4 shrink-0 text-amber-500"
                viewBox="0 0 20 20"
                fill="currentColor"
                aria-hidden="true"
              >
                <path
                  fillRule="evenodd"
                  d="M8.485 2.495c.673-1.167 2.357-1.167 3.03 0l6.28 10.875c.673 1.167-.17 2.625-1.516 2.625H3.72c-1.347 0-2.189-1.458-1.515-2.625L8.485 2.495zM10 5a.75.75 0 01.75.75v3.5a.75.75 0 01-1.5 0v-3.5A.75.75 0 0110 5zm0 9a1 1 0 100-2 1 1 0 000 2z"
                  clipRule="evenodd"
                />
              </svg>

              <Link
                href={`/swarm/${encodeURIComponent(alert.agentName)}`}
                className="truncate font-mono font-semibold text-neutral-900 hover:text-blue-600"
              >
                {alert.agentName}
              </Link>
            </div>

            <div className="flex shrink-0 items-center gap-3 text-xs text-neutral-600">
              <span>
                {alert.revertCount} {alert.revertCount === 1 ? "revert" : "reverts"} /{" "}
                {alert.windowHours}h
              </span>

              <span
                className={`rounded-full px-2 py-0.5 font-mono font-semibold ${badge}`}
                aria-label={`Revert rate: ${(alert.revertRate * 100).toFixed(1)}%`}
              >
                {(alert.revertRate * 100).toFixed(1)}%
              </span>
            </div>
          </li>
        );
      })}
    </ul>
  );
}
