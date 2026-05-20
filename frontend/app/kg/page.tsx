"use client";

import { useState, useCallback } from "react";
import { useSearchParams, useRouter } from "next/navigation";
import { EntityBrowser } from "./components/EntityBrowser";
import { PropertyInspector } from "./components/PropertyInspector";
import { NeighborhoodGraph } from "./components/NeighborhoodGraph";
import { PathFinder } from "./components/PathFinder";
import type { Entity, EntityKind } from "../../lib/types";

type Tab = "browser" | "graph" | "pathfinder";

const TABS: { id: Tab; label: string }[] = [
  { id: "browser", label: "Entity Browser" },
  { id: "graph", label: "Neighborhood Graph" },
  { id: "pathfinder", label: "Path Finder" },
];

export default function KGExplorerPage() {
  const searchParams = useSearchParams();
  const router = useRouter();

  const initialTab = (searchParams.get("tab") as Tab | null) ?? "browser";
  const initialEntityId = searchParams.get("entity");
  const initialCenterId = searchParams.get("center");
  const initialQuery = searchParams.get("q") ?? "";
  const initialKinds = (searchParams.get("kind") ?? "")
    .split(",")
    .filter(Boolean) as EntityKind[];

  const [activeTab, setActiveTab] = useState<Tab>(initialTab);
  const [inspectedId, setInspectedId] = useState<string | null>(
    initialEntityId,
  );
  const [graphCenterId, setGraphCenterId] = useState<string | null>(
    initialCenterId,
  );

  const handleSelectEntity = useCallback(
    (entity: Entity) => {
      setInspectedId(entity.id);
      const params = new URLSearchParams(searchParams.toString());
      params.set("entity", entity.id);
      router.replace(`/kg?${params.toString()}`, { scroll: false });
    },
    [searchParams, router],
  );

  const handleCloseInspector = useCallback(() => {
    setInspectedId(null);
    const params = new URLSearchParams(searchParams.toString());
    params.delete("entity");
    router.replace(`/kg?${params.toString()}`, { scroll: false });
  }, [searchParams, router]);

  const handleOpenInGraph = useCallback(
    (id: string) => {
      setGraphCenterId(id);
      setActiveTab("graph");
      setInspectedId(null);
      const params = new URLSearchParams(searchParams.toString());
      params.set("tab", "graph");
      params.set("center", id);
      params.delete("entity");
      router.replace(`/kg?${params.toString()}`, { scroll: false });
    },
    [searchParams, router],
  );

  const handleTabChange = useCallback(
    (tab: Tab) => {
      setActiveTab(tab);
      const params = new URLSearchParams(searchParams.toString());
      params.set("tab", tab);
      router.replace(`/kg?${params.toString()}`, { scroll: false });
    },
    [searchParams, router],
  );

  return (
    <div className="flex flex-col p-6">
      {/* Page header */}
      <div className="mb-4 flex items-center justify-between">
        <div>
          <h1 className="text-xl font-bold text-white">KG Explorer</h1>
          <p className="text-sm text-neutral-500">
            Browse, search, and visualize the knowledge graph
          </p>
        </div>
      </div>

      {/* Tab bar */}
      <div className="mb-4 flex border-b border-neutral-800">
        {TABS.map((tab) => (
          <button
            key={tab.id}
            onClick={() => handleTabChange(tab.id)}
            className={[
              "px-4 py-2 text-sm transition-colors",
              activeTab === tab.id
                ? "border-b-2 border-blue-500 text-blue-400"
                : "text-neutral-500 hover:text-neutral-300",
            ].join(" ")}
          >
            {tab.label}
          </button>
        ))}
      </div>

      {/* Tab content */}
      {activeTab === "browser" && (
        <EntityBrowser
          selectedId={inspectedId}
          onSelect={handleSelectEntity}
          initialQuery={initialQuery}
          initialKinds={initialKinds}
        />
      )}

      {activeTab === "graph" && (
        <div className="space-y-4">
          <NeighborhoodGraph
            centerId={graphCenterId}
            onSelectEntity={(id) => {
              setInspectedId(id);
              const params = new URLSearchParams(searchParams.toString());
              params.set("entity", id);
              router.replace(`/kg?${params.toString()}`, { scroll: false });
            }}
          />
          {!graphCenterId && (
            <p className="text-sm text-neutral-500">
              Select an entity from the Browser tab and click &quot;View in
              graph&quot; to visualize its neighborhood.
            </p>
          )}
        </div>
      )}

      {activeTab === "pathfinder" && (
        <PathFinder
          onSelectEntity={(id) => {
            setInspectedId(id);
            const params = new URLSearchParams(searchParams.toString());
            params.set("entity", id);
            router.replace(`/kg?${params.toString()}`, { scroll: false });
          }}
        />
      )}

      {/* Property Inspector slide-out drawer */}
      <PropertyInspector
        entityId={inspectedId}
        onClose={handleCloseInspector}
        onOpenInGraph={handleOpenInGraph}
      />
    </div>
  );
}
