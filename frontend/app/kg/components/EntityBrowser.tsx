"use client";

import { useState, useCallback } from "react";
import { useEntities } from "../../../lib/api";
import type { EntityKind, Entity } from "../../../lib/types";

const ALL_KINDS: EntityKind[] = [
  "concept",
  "document",
  "dataset",
  "project",
  "person",
  "org",
];

const KIND_COLOR: Record<EntityKind, string> = {
  concept: "bg-blue-900 text-blue-300",
  document: "bg-amber-900 text-amber-300",
  dataset: "bg-teal-900 text-teal-300",
  project: "bg-emerald-900 text-emerald-300",
  person: "bg-violet-900 text-violet-300",
  org: "bg-slate-700 text-slate-300",
};

const PAGE_SIZE = 25;

interface Props {
  selectedId: string | null;
  onSelect: (entity: Entity) => void;
  initialQuery?: string;
  initialKinds?: EntityKind[];
}

export function EntityBrowser({
  selectedId,
  onSelect,
  initialQuery = "",
  initialKinds = [],
}: Props) {
  const [query, setQuery] = useState(initialQuery);
  const [activeQuery, setActiveQuery] = useState(initialQuery);
  const [selectedKinds, setSelectedKinds] = useState<EntityKind[]>(initialKinds);
  const [page, setPage] = useState(0);

  const { data, isLoading, isError, error, refetch } = useEntities({
    query: activeQuery || undefined,
    kinds: selectedKinds.length > 0 ? selectedKinds : undefined,
    limit: PAGE_SIZE,
    offset: page * PAGE_SIZE,
  });

  const toggleKind = useCallback((kind: EntityKind) => {
    setSelectedKinds((prev) =>
      prev.includes(kind) ? prev.filter((k) => k !== kind) : [...prev, kind],
    );
    setPage(0);
  }, []);

  const handleSearch = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      setActiveQuery(query);
      setPage(0);
    },
    [query],
  );

  const totalPages = data ? Math.ceil(data.total / PAGE_SIZE) : 0;

  return (
    <div className="flex flex-col gap-3">
      {/* Search + filter row */}
      <div className="flex flex-wrap items-center gap-2">
        <form onSubmit={handleSearch} className="flex gap-2">
          <input
            type="search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search entities…"
            className="w-64 rounded border border-neutral-700 bg-neutral-900 px-3 py-1.5 text-sm text-neutral-100 placeholder-neutral-500 focus:border-neutral-500 focus:outline-none"
          />
          <button
            type="submit"
            className="rounded border border-neutral-700 bg-neutral-800 px-3 py-1.5 text-sm text-neutral-300 hover:bg-neutral-700"
          >
            Search
          </button>
        </form>

        {/* Kind toggles */}
        <div className="flex flex-wrap gap-1">
          {ALL_KINDS.map((kind) => {
            const active = selectedKinds.includes(kind);
            return (
              <button
                key={kind}
                onClick={() => toggleKind(kind)}
                className={[
                  "rounded px-2 py-1 text-xs font-medium transition-opacity",
                  active
                    ? KIND_COLOR[kind]
                    : "bg-neutral-800 text-neutral-500 hover:text-neutral-300",
                ].join(" ")}
              >
                {kind}
              </button>
            );
          })}
          {selectedKinds.length > 0 && (
            <button
              onClick={() => {
                setSelectedKinds([]);
                setPage(0);
              }}
              className="rounded px-2 py-1 text-xs text-neutral-500 hover:text-neutral-300"
            >
              clear ✕
            </button>
          )}
        </div>

        {/* Refresh */}
        <button
          onClick={() => refetch()}
          className="ml-auto rounded border border-neutral-700 px-2 py-1 text-xs text-neutral-500 hover:text-neutral-300"
          aria-label="Refresh"
        >
          ↺
        </button>
      </div>

      {/* Table */}
      <div className="overflow-hidden rounded border border-neutral-800">
        <table className="w-full text-sm">
          <thead className="border-b border-neutral-800 bg-neutral-900">
            <tr>
              <th className="px-3 py-2 text-left text-xs font-medium text-neutral-500">
                Kind
              </th>
              <th className="px-3 py-2 text-left text-xs font-medium text-neutral-500">
                Name
              </th>
              <th className="px-3 py-2 text-left text-xs font-medium text-neutral-500">
                Domain
              </th>
              <th className="px-3 py-2 text-left text-xs font-medium text-neutral-500">
                Status
              </th>
              <th className="px-3 py-2 text-right text-xs font-medium text-neutral-500">
                Edges
              </th>
            </tr>
          </thead>
          <tbody>
            {isLoading && (
              <>
                {[...Array(8)].map((_, i) => (
                  <tr key={i} className="border-b border-neutral-800">
                    {[...Array(5)].map((_, j) => (
                      <td key={j} className="px-3 py-2">
                        <div className="h-3 animate-pulse rounded bg-neutral-800" />
                      </td>
                    ))}
                  </tr>
                ))}
              </>
            )}

            {isError && (
              <tr>
                <td
                  colSpan={5}
                  className="px-3 py-4 text-center text-red-400"
                >
                  Error loading entities:{" "}
                  {error instanceof Error ? error.message : String(error)}
                </td>
              </tr>
            )}

            {data && data.items.length === 0 && (
              <tr>
                <td
                  colSpan={5}
                  className="px-3 py-8 text-center text-neutral-500"
                >
                  <p>No entities found.</p>
                  <p className="mt-1 text-xs">
                    Run <code className="text-blue-400">/kg-digest</code> to
                    ingest your first paper.
                  </p>
                </td>
              </tr>
            )}

            {data?.items.map((entity) => (
              <tr
                key={entity.id}
                onClick={() => onSelect(entity)}
                className={[
                  "cursor-pointer border-b border-neutral-800 transition-colors",
                  selectedId === entity.id
                    ? "bg-neutral-800"
                    : "hover:bg-neutral-900",
                ].join(" ")}
              >
                <td className="px-3 py-2">
                  <span
                    className={`rounded px-1.5 py-0.5 text-xs font-medium ${KIND_COLOR[entity.kind]}`}
                  >
                    {entity.kind}
                  </span>
                </td>
                <td className="max-w-xs px-3 py-2">
                  <span className="block truncate font-medium text-neutral-100">
                    {entity.name}
                  </span>
                  {entity.description && (
                    <span className="block truncate text-xs text-neutral-500">
                      {entity.description}
                    </span>
                  )}
                </td>
                <td className="px-3 py-2 text-xs text-neutral-400">
                  {entity.properties?.domain ?? (
                    <span className="text-neutral-700">—</span>
                  )}
                </td>
                <td className="px-3 py-2 text-xs text-neutral-400">
                  {entity.properties?.status ?? (
                    <span className="text-neutral-700">—</span>
                  )}
                </td>
                <td className="px-3 py-2 text-right text-xs font-mono text-neutral-400">
                  {entity.edge_count ?? "—"}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {/* Pagination */}
      {data && totalPages > 1 && (
        <div className="flex items-center justify-center gap-3 text-sm text-neutral-400">
          <button
            onClick={() => setPage((p) => Math.max(0, p - 1))}
            disabled={page === 0}
            className="rounded px-2 py-1 hover:text-neutral-200 disabled:opacity-30"
          >
            ← prev
          </button>
          <span>
            Page {page + 1} of {totalPages}
          </span>
          <button
            onClick={() => setPage((p) => Math.min(totalPages - 1, p + 1))}
            disabled={page >= totalPages - 1}
            className="rounded px-2 py-1 hover:text-neutral-200 disabled:opacity-30"
          >
            next →
          </button>
        </div>
      )}
    </div>
  );
}
