"use client";

import { useEffect, useRef } from "react";
import { useEntity, useNeighbors } from "../../../lib/api";
import type { EdgeRelation, Entity } from "../../../lib/types";
import { EDGE_CATEGORY } from "../../../lib/types";

interface Props {
  entityId: string | null;
  onClose: () => void;
  onOpenInGraph?: (id: string) => void;
}

const RELATION_LABEL: Record<string, string> = {
  contains: "contains",
  part_of: "part of",
  instance_of: "instance of",
  extends: "extends",
  variant_of: "variant of",
  introduced_by: "introduced by",
  supersedes: "supersedes",
  depends_on: "depends on",
  enables: "enables",
  implements: "implements",
  competes_with: "competes with",
  composed_with: "composed with",
  annotates: "annotates",
};

function KindBadge({ kind }: { kind: Entity["kind"] }) {
  const colors: Record<Entity["kind"], string> = {
    concept: "bg-blue-900 text-blue-200",
    document: "bg-amber-900 text-amber-200",
    dataset: "bg-teal-900 text-teal-200",
    project: "bg-emerald-900 text-emerald-200",
    person: "bg-violet-900 text-violet-200",
    org: "bg-slate-700 text-slate-200",
  };
  return (
    <span
      className={`rounded px-2 py-0.5 text-xs font-medium ${colors[kind]}`}
    >
      {kind}
    </span>
  );
}

export function PropertyInspector({ entityId, onClose, onOpenInGraph }: Props) {
  const panelRef = useRef<HTMLDivElement>(null);
  const { data: entity, isLoading, isError, error } = useEntity(entityId);
  const { data: neighbors } = useNeighbors(entityId);

  // Close on Escape
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  // Close on click outside
  useEffect(() => {
    function onPointer(e: PointerEvent) {
      if (
        entityId &&
        panelRef.current &&
        !panelRef.current.contains(e.target as Node)
      ) {
        onClose();
      }
    }
    document.addEventListener("pointerdown", onPointer);
    return () => document.removeEventListener("pointerdown", onPointer);
  }, [entityId, onClose]);

  // Relation count summary
  const relationCounts: Partial<Record<EdgeRelation, number>> = {};
  if (neighbors) {
    for (const n of neighbors) {
      relationCounts[n.relation] = (relationCounts[n.relation] ?? 0) + 1;
    }
  }

  const isOpen = entityId != null;

  return (
    <>
      {/* Backdrop */}
      {isOpen && (
        <div
          className="fixed inset-0 z-40 bg-black/30"
          aria-hidden="true"
          onClick={onClose}
        />
      )}
      {/* Drawer */}
      <div
        ref={panelRef}
        className={[
          "fixed right-0 top-0 z-50 flex h-full w-96 flex-col border-l border-neutral-800 bg-neutral-950 shadow-2xl",
          "transition-transform duration-200",
          isOpen ? "translate-x-0" : "translate-x-full",
        ].join(" ")}
        aria-label="Property inspector"
      >
        {/* Header */}
        <div className="flex items-center justify-between border-b border-neutral-800 px-4 py-3">
          <span className="font-semibold text-white">Property Inspector</span>
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
          {!entityId && (
            <p className="text-neutral-500">
              Click a row to inspect an entity.
            </p>
          )}

          {entityId && isLoading && (
            <div className="space-y-3">
              {[...Array(5)].map((_, i) => (
                <div
                  key={i}
                  className="h-4 animate-pulse rounded bg-neutral-800"
                />
              ))}
            </div>
          )}

          {entityId && isError && (
            <div className="rounded border border-red-800 bg-red-950 p-3 text-red-300">
              <p className="font-medium">Error loading entity</p>
              <p className="mt-1 text-xs">
                {error instanceof Error ? error.message : String(error)}
              </p>
            </div>
          )}

          {entity && (
            <div className="space-y-4">
              {/* Name + kind */}
              <div>
                <div className="flex items-center gap-2">
                  <KindBadge kind={entity.kind} />
                  <h2 className="text-base font-semibold text-white">
                    {entity.name}
                  </h2>
                </div>
                <p className="mt-1 font-mono text-xs text-neutral-500">
                  {entity.full_id}
                  <button
                    className="ml-2 text-neutral-400 hover:text-white"
                    onClick={() =>
                      navigator.clipboard.writeText(entity.full_id)
                    }
                    aria-label="Copy UUID"
                  >
                    ⎘
                  </button>
                </p>
              </div>

              {/* Description */}
              {entity.description && (
                <div>
                  <h3 className="mb-1 text-xs font-medium uppercase tracking-wider text-neutral-500">
                    Description
                  </h3>
                  <p className="text-neutral-300">{entity.description}</p>
                </div>
              )}

              {/* Tags */}
              {entity.tags && entity.tags.length > 0 && (
                <div>
                  <h3 className="mb-1 text-xs font-medium uppercase tracking-wider text-neutral-500">
                    Tags
                  </h3>
                  <div className="flex flex-wrap gap-1">
                    {entity.tags.map((tag) => (
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

              {/* Properties */}
              {entity.properties &&
                Object.keys(entity.properties).length > 0 && (
                  <div>
                    <h3 className="mb-1 text-xs font-medium uppercase tracking-wider text-neutral-500">
                      Properties
                    </h3>
                    <table className="w-full text-xs">
                      <tbody>
                        {Object.entries(entity.properties)
                          .sort(([a], [b]) => a.localeCompare(b))
                          .map(([k, v]) => (
                            <tr
                              key={k}
                              className="border-t border-neutral-800 first:border-0"
                            >
                              <td className="py-1 pr-3 font-medium text-neutral-400">
                                {k}
                              </td>
                              <td className="py-1 text-neutral-200">{v}</td>
                            </tr>
                          ))}
                      </tbody>
                    </table>
                  </div>
                )}

              {/* Edge summary */}
              {neighbors && neighbors.length > 0 && (
                <div>
                  <h3 className="mb-1 text-xs font-medium uppercase tracking-wider text-neutral-500">
                    Edges ({neighbors.length})
                  </h3>
                  <table className="w-full text-xs">
                    <tbody>
                      {Object.entries(relationCounts).map(([rel, count]) => (
                        <tr
                          key={rel}
                          className="border-t border-neutral-800 first:border-0"
                        >
                          <td className="py-1 pr-3 text-neutral-400">
                            {RELATION_LABEL[rel] ?? rel}
                          </td>
                          <td className="py-1 text-right font-medium text-neutral-200">
                            {count}
                          </td>
                          <td className="py-1 pl-3 text-neutral-600">
                            {EDGE_CATEGORY[rel as EdgeRelation]}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}

              {/* Open in graph */}
              {onOpenInGraph && (
                <button
                  onClick={() => onOpenInGraph(entity.id)}
                  className="w-full rounded border border-neutral-700 px-3 py-2 text-xs text-neutral-300 hover:border-neutral-500 hover:text-white"
                >
                  View in graph →
                </button>
              )}
            </div>
          )}
        </div>
      </div>
    </>
  );
}
