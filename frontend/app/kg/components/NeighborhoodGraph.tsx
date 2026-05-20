"use client";

import {
  useCallback,
  useEffect,
} from "react";
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  useEdgesState,
  useNodesState,
  type Edge as FlowEdge,
  type Node as FlowNode,
  type NodeMouseHandler,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useNeighbors, useEntity, fetchEntity } from "../../../lib/api";
import type { EntityKind, EdgeCategory, EdgeRelation } from "../../../lib/types";
import { EDGE_CATEGORY } from "../../../lib/types";

// Color palette by entity kind (ADR-047 §1.2)
const KIND_COLOR: Record<EntityKind, string> = {
  concept: "#3b82f6",
  document: "#f59e0b",
  dataset: "#14b8a6",
  project: "#10b981",
  person: "#8b5cf6",
  org: "#64748b",
};

// Edge category colors
const CATEGORY_COLOR: Record<EdgeCategory, string> = {
  structure: "#6b7280",
  derivation: "#a78bfa",
  dependency: "#f97316",
  implementation: "#22c55e",
  lateral: "#ef4444",
  annotation: "#60a5fa",
};

function edgeColor(relation: EdgeRelation): string {
  const cat = EDGE_CATEGORY[relation];
  return CATEGORY_COLOR[cat] ?? "#6b7280";
}

interface EntityNodeData extends Record<string, unknown> {
  label: string;
  kind: EntityKind;
  entityId: string;
}

interface Props {
  centerId: string | null;
  onSelectEntity?: (id: string) => void;
}

export function NeighborhoodGraph({ centerId, onSelectEntity }: Props) {
  const [nodes, setNodes, onNodesChange] = useNodesState<FlowNode<EntityNodeData>>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<FlowEdge>([]);

  const { data: centerEntity } = useEntity(centerId);
  const { data: neighbors, isLoading, isError, error } = useNeighbors(centerId);

  // Build graph when center + neighbors are loaded
  useEffect(() => {
    if (!centerId || !centerEntity || !neighbors) return;

    const centerNode: FlowNode<EntityNodeData> = {
      id: centerId,
      type: "default",
      position: { x: 300, y: 200 },
      data: {
        label: centerEntity.name.slice(0, 28),
        kind: centerEntity.kind,
        entityId: centerId,
      },
      style: {
        background: KIND_COLOR[centerEntity.kind],
        color: "#fff",
        border: "2px solid #fff",
        borderRadius: 6,
        fontFamily: "monospace",
        fontSize: 12,
        padding: "8px 12px",
        minWidth: 100,
      },
    };

    // Resolve neighbor names async
    const resolveNeighbors = async () => {
      const neighborNodes: FlowNode<EntityNodeData>[] = [];
      const flowEdges: FlowEdge[] = [];
      const angleStep = (2 * Math.PI) / Math.max(neighbors.length, 1);
      const radius = 220;

      const resolved = await Promise.all(
        neighbors.map((n) =>
          fetchEntity(n.entity_id).catch(() => null),
        ),
      );

      neighbors.forEach((n, i) => {
        const entity = resolved[i];
        if (!entity) return;
        const angle = i * angleStep - Math.PI / 2;
        neighborNodes.push({
          id: n.entity_id,
          type: "default",
          position: {
            x: 300 + radius * Math.cos(angle),
            y: 200 + radius * Math.sin(angle),
          },
          data: {
            label: entity.name.slice(0, 28),
            kind: entity.kind,
            entityId: n.entity_id,
          },
          style: {
            background: KIND_COLOR[entity.kind],
            color: "#fff",
            borderRadius: 6,
            fontFamily: "monospace",
            fontSize: 11,
            padding: "6px 10px",
            opacity: 0.85,
          },
        });

        const isOutbound = n.direction === "outbound";
        flowEdges.push({
          id: `${n.direction}-${centerId}-${n.entity_id}-${n.relation}`,
          source: isOutbound ? centerId : n.entity_id,
          target: isOutbound ? n.entity_id : centerId,
          label: n.relation.replace(/_/g, " "),
          labelStyle: { fontSize: 9, fill: "#9ca3af" },
          labelBgStyle: { fill: "#111827", fillOpacity: 0.8 },
          style: { stroke: edgeColor(n.relation), strokeWidth: 1.5 },
          markerEnd: {
            type: "arrowclosed",
            color: edgeColor(n.relation),
          },
          animated: false,
        });
      });

      setNodes([centerNode, ...neighborNodes]);
      setEdges(flowEdges);
    };

    resolveNeighbors();
  }, [centerId, centerEntity, neighbors, setNodes, setEdges]);

  const onNodeClick: NodeMouseHandler<FlowNode<EntityNodeData>> = useCallback(
    (_, node) => {
      onSelectEntity?.(node.data.entityId);
    },
    [onSelectEntity],
  );

  if (!centerId) {
    return (
      <div className="flex h-64 items-center justify-center rounded border border-neutral-800 text-sm text-neutral-500">
        Select an entity to view its neighborhood graph.
      </div>
    );
  }

  if (isLoading) {
    return (
      <div className="flex h-64 items-center justify-center rounded border border-neutral-800 text-sm text-neutral-500">
        Loading graph…
      </div>
    );
  }

  if (isError) {
    return (
      <div className="flex h-64 items-center justify-center rounded border border-red-900 bg-red-950 text-sm text-red-400">
        Error: {error instanceof Error ? error.message : String(error)}
      </div>
    );
  }

  return (
    <div className="h-[480px] overflow-hidden rounded border border-neutral-800 bg-neutral-950">
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onNodeClick={onNodeClick}
        fitView
        proOptions={{ hideAttribution: true }}
      >
        <Background color="#374151" gap={20} />
        <Controls />
        <MiniMap
          nodeColor={(n) =>
            KIND_COLOR[(n.data as EntityNodeData)?.kind] ?? "#6b7280"
          }
          style={{ background: "#0a0a0a" }}
        />
      </ReactFlow>

      {/* Legend */}
      <div className="flex flex-wrap gap-3 border-t border-neutral-800 bg-neutral-950 px-4 py-2 text-xs text-neutral-400">
        {(
          Object.entries(KIND_COLOR) as [EntityKind, string][]
        ).map(([kind, color]) => (
          <span key={kind} className="flex items-center gap-1">
            <span
              className="inline-block h-2.5 w-2.5 rounded-sm"
              style={{ background: color }}
            />
            {kind}
          </span>
        ))}
      </div>
    </div>
  );
}
