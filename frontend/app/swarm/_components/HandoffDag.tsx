"use client";

import {
  Background,
  Controls,
  Edge,
  Handle,
  MiniMap,
  Node,
  NodeProps,
  Position,
  ReactFlow,
  ReactFlowProvider,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import type { HandoffEdge as HandoffEdgeData } from "@/lib/swarm/types";

// ---------------------------------------------------------------------------
// Custom node data type
// ---------------------------------------------------------------------------

interface AgentNodeData extends Record<string, unknown> {
  name: string;
  activeTasks: number;
  isSelected: boolean;
}

// ---------------------------------------------------------------------------
// AgentNode — custom React Flow node
// ---------------------------------------------------------------------------

function AgentNode({ data }: NodeProps<Node<AgentNodeData>>) {
  return (
    <div
      className={[
        "min-w-[100px] rounded-lg border px-3 py-2 font-mono text-sm shadow-sm",
        data.isSelected
          ? "border-blue-500 bg-blue-50 text-blue-900"
          : "border-neutral-300 bg-white text-neutral-900",
      ].join(" ")}
    >
      <Handle type="target" position={Position.Top} />
      <div className="font-semibold">{data.name}</div>
      {data.activeTasks > 0 && (
        <div className="mt-0.5 text-xs text-green-600">{data.activeTasks} active</div>
      )}
      <Handle type="source" position={Position.Bottom} />
    </div>
  );
}

const NODE_TYPES = { agentNode: AgentNode };

// ---------------------------------------------------------------------------
// Edge thickness from task count (ADR-045 §D3)
// ---------------------------------------------------------------------------

function edgeStrokeWidth(taskCount: number): number {
  if (taskCount <= 5) return 1.5;
  if (taskCount <= 20) return 3;
  return 5;
}

// ---------------------------------------------------------------------------
// Build React Flow nodes and edges from HandoffEdge[]
// ---------------------------------------------------------------------------

interface NodePosition {
  x: number;
  y: number;
}

function buildGraph(
  handoffEdges: HandoffEdgeData[],
  agentSummaries: { name: string; activeTasks: number }[],
  selectedAgent?: string,
): { nodes: Node<AgentNodeData>[]; edges: Edge[] } {
  const agentNames = new Set<string>();

  for (const e of handoffEdges) {
    agentNames.add(e.fromAgent);
    agentNames.add(e.toAgent);
  }
  // Also add agents with no handoffs yet (from summaries)
  for (const s of agentSummaries) {
    agentNames.add(s.name);
  }

  const names = Array.from(agentNames);

  // Simple arc layout: place nodes in a horizontal row, spaced 160px apart
  const positions: Map<string, NodePosition> = new Map();
  names.forEach((name, i) => {
    positions.set(name, { x: i * 160, y: 0 });
  });

  const activeLookup = new Map(agentSummaries.map((s) => [s.name, s.activeTasks]));

  const nodes: Node<AgentNodeData>[] = names.map((name) => ({
    id: name,
    type: "agentNode",
    position: positions.get(name)!,
    data: {
      name,
      activeTasks: activeLookup.get(name) ?? 0,
      isSelected: name === selectedAgent,
    },
  }));

  const edges: Edge[] = handoffEdges.map((e) => {
    const isIncident =
      selectedAgent !== undefined && (e.fromAgent === selectedAgent || e.toAgent === selectedAgent);

    return {
      id: `${e.fromAgent}->${e.toAgent}`,
      source: e.fromAgent,
      target: e.toAgent,
      label: String(e.taskCount),
      animated: false,
      style: {
        strokeWidth: edgeStrokeWidth(e.taskCount),
        stroke: isIncident ? "#3b82f6" : "#9ca3af",
      },
      markerEnd: {
        type: "arrowclosed" as const,
        color: isIncident ? "#3b82f6" : "#9ca3af",
      },
    };
  });

  return { nodes, edges };
}

// ---------------------------------------------------------------------------
// HandoffDag props
// ---------------------------------------------------------------------------

interface HandoffDagProps {
  edges: HandoffEdgeData[];
  agentSummaries?: { name: string; activeTasks: number }[];
  selectedAgent?: string;
}

// ---------------------------------------------------------------------------
// HandoffDag — React Flow container
// ---------------------------------------------------------------------------

function HandoffDagInner({ edges, agentSummaries = [], selectedAgent }: HandoffDagProps) {
  const { nodes, edges: flowEdges } = buildGraph(edges, agentSummaries, selectedAgent);

  if (nodes.length === 0) {
    return (
      <div className="flex h-64 items-center justify-center rounded-lg border border-dashed border-neutral-300 text-sm text-neutral-500">
        No handoff data yet. Start a swarm to see agent-to-agent handoffs.
      </div>
    );
  }

  return (
    <div className="h-64 w-full rounded-lg border border-neutral-200 bg-white">
      <ReactFlow
        nodes={nodes}
        edges={flowEdges}
        nodeTypes={NODE_TYPES}
        fitView
        fitViewOptions={{ padding: 0.3 }}
        minZoom={0.3}
        maxZoom={2}
        proOptions={{ hideAttribution: true }}
      >
        <Background gap={16} color="#f3f4f6" />
        <Controls showInteractive={false} />
        <MiniMap
          nodeColor={(n) => ((n.data as AgentNodeData).isSelected ? "#3b82f6" : "#e5e7eb")}
          maskColor="rgba(255,255,255,0.7)"
        />
      </ReactFlow>
    </div>
  );
}

export default function HandoffDag(props: HandoffDagProps) {
  return (
    <ReactFlowProvider>
      <HandoffDagInner {...props} />
    </ReactFlowProvider>
  );
}
