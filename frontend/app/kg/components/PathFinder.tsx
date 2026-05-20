"use client";

import { useState, useCallback } from "react";
import { useEntities } from "../../../lib/api";
import type { Entity } from "../../../lib/types";

interface PathNode {
  entity: Entity;
  relation?: string;
}

interface Props {
  onSelectEntity?: (id: string) => void;
}

// Entity autocomplete search field
function EntityAutocomplete({
  label,
  value,
  onChange,
}: {
  label: string;
  value: Entity | null;
  onChange: (e: Entity | null) => void;
}) {
  const [inputText, setInputText] = useState(value?.name ?? "");
  const [open, setOpen] = useState(false);

  const { data } = useEntities({
    query: inputText.length >= 2 ? inputText : undefined,
    limit: 8,
  });

  return (
    <div className="relative flex-1">
      <label className="mb-1 block text-xs text-neutral-500">{label}</label>
      <input
        type="text"
        value={value ? value.name : inputText}
        onChange={(e) => {
          setInputText(e.target.value);
          if (value) onChange(null);
          setOpen(true);
        }}
        onFocus={() => setOpen(true)}
        onBlur={() => setTimeout(() => setOpen(false), 150)}
        placeholder="Search entity…"
        className="w-full rounded border border-neutral-700 bg-neutral-900 px-3 py-1.5 text-sm text-neutral-100 placeholder-neutral-500 focus:border-neutral-500 focus:outline-none"
      />
      {open && data && data.items.length > 0 && !value && (
        <ul className="absolute z-50 mt-1 w-full rounded border border-neutral-700 bg-neutral-900 shadow-lg">
          {data.items.map((entity) => (
            <li
              key={entity.id}
              onMouseDown={() => {
                onChange(entity);
                setInputText(entity.name);
                setOpen(false);
              }}
              className="cursor-pointer px-3 py-2 text-sm text-neutral-300 hover:bg-neutral-800"
            >
              <span className="mr-2 text-xs text-neutral-500">
                {entity.kind}
              </span>
              {entity.name}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// ADR-047 §1.3 — PathFinder is phase 2 (requires query crate shortest-path op).
// This component renders the UI shell with a client-side BFS fallback notice.
export function PathFinder({ onSelectEntity }: Props) {
  const [fromEntity, setFromEntity] = useState<Entity | null>(null);
  const [toEntity, setToEntity] = useState<Entity | null>(null);
  const [path, setPath] = useState<PathNode[] | null>(null);
  const [noPath, setNoPath] = useState(false);
  const [loading, setLoading] = useState(false);

  const findPath = useCallback(async () => {
    if (!fromEntity || !toEntity) return;
    setLoading(true);
    setPath(null);
    setNoPath(false);

    // Phase 2 will call query(gql_shortest_path). For now we show a direct
    // 1-hop result if the entities are directly connected (via neighbors call),
    // otherwise report "No path found" per the ADR-047 fallback note.
    try {
      const res = await fetch(
        `/api/entities/${encodeURIComponent(fromEntity.id)}/neighbors`,
      );
      if (res.ok) {
        const neighbors = (await res.json()) as Array<{
          entity_id: string;
          relation: string;
        }>;
        const direct = neighbors.find((n) => n.entity_id === toEntity.id);
        if (direct) {
          setPath([
            { entity: fromEntity },
            { entity: toEntity, relation: direct.relation },
          ]);
        } else {
          setNoPath(true);
        }
      } else {
        setNoPath(true);
      }
    } catch {
      setNoPath(true);
    } finally {
      setLoading(false);
    }
  }, [fromEntity, toEntity]);

  return (
    <div className="space-y-4">
      <div className="rounded border border-amber-900 bg-amber-950 px-3 py-2 text-xs text-amber-400">
        Path Finder is phase 2. Currently shows direct 1-hop connections only.
        Full shortest-path requires the query crate path operator (ADR-008).
      </div>

      <div className="flex items-end gap-3">
        <EntityAutocomplete
          label="From"
          value={fromEntity}
          onChange={setFromEntity}
        />
        <span className="mb-1.5 text-neutral-500">→</span>
        <EntityAutocomplete label="To" value={toEntity} onChange={setToEntity} />
        <button
          onClick={findPath}
          disabled={!fromEntity || !toEntity || loading}
          className="mb-0.5 rounded border border-neutral-700 bg-neutral-800 px-4 py-1.5 text-sm text-neutral-300 hover:bg-neutral-700 disabled:opacity-40"
        >
          {loading ? "…" : "Find path"}
        </button>
      </div>

      {/* Path result */}
      {path && (
        <div className="rounded border border-neutral-800 bg-neutral-900 p-4">
          <div className="flex flex-wrap items-center gap-2">
            {path.map((step, i) => (
              <span key={i} className="flex items-center gap-2">
                {step.relation && (
                  <span className="text-xs text-neutral-500">
                    ─{step.relation.replace(/_/g, " ")}─▶
                  </span>
                )}
                <button
                  onClick={() => onSelectEntity?.(step.entity.id)}
                  className="rounded bg-neutral-800 px-2 py-1 text-sm font-medium text-neutral-100 hover:bg-neutral-700"
                >
                  {step.entity.name}
                </button>
              </span>
            ))}
          </div>
          <p className="mt-2 text-xs text-neutral-500">
            {path.length - 1} hop{path.length - 1 !== 1 ? "s" : ""}
          </p>
        </div>
      )}

      {noPath && (
        <div className="rounded border border-neutral-800 bg-neutral-900 p-4 text-sm text-neutral-500">
          No direct path found between these two entities.
        </div>
      )}
    </div>
  );
}
