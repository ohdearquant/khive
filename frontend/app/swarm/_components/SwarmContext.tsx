"use client";

import {
  createContext,
  Dispatch,
  ReactNode,
  useCallback,
  useContext,
  useEffect,
  useReducer,
  useRef,
} from "react";
import {
  buildHeatmapBuckets,
  computeAgentSummaries,
  deriveCycles,
  deriveDriftAlerts,
  deriveHandoffs,
  deriveHeatmap,
} from "@/lib/swarm/derive";
import { fetchAgentQueues, fetchDoneTasks, fetchRecentCompleted } from "@/lib/swarm/queries";
import type {
  AgentSummary,
  CycleBucket,
  DriftAlert,
  HandoffEdge,
  HeatmapCell,
} from "@/lib/swarm/types";

// ---------------------------------------------------------------------------
// State shape
// ---------------------------------------------------------------------------

interface SwarmState {
  namespace: string;
  agents: AgentSummary[];
  handoffs: HandoffEdge[];
  cycles: CycleBucket[];
  heatmap: HeatmapCell[][];
  heatmapAgents: string[];
  heatmapBuckets: string[];
  driftAlerts: DriftAlert[];
  lastUpdated: number;
  polling: boolean;
  error: string | null;
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

type SwarmAction =
  | {
      type: "SET_DATA";
      agents: AgentSummary[];
      handoffs: HandoffEdge[];
      cycles: CycleBucket[];
      heatmap: HeatmapCell[][];
      heatmapAgents: string[];
      heatmapBuckets: string[];
      driftAlerts: DriftAlert[];
      ts: number;
    }
  | { type: "SET_ERROR"; error: string }
  | { type: "CLEAR_ERROR" }
  | { type: "SET_NAMESPACE"; namespace: string };

// ---------------------------------------------------------------------------
// Reducer
// ---------------------------------------------------------------------------

function swarmReducer(state: SwarmState, action: SwarmAction): SwarmState {
  switch (action.type) {
    case "SET_DATA":
      return {
        ...state,
        agents: action.agents,
        handoffs: action.handoffs,
        cycles: action.cycles,
        heatmap: action.heatmap,
        heatmapAgents: action.heatmapAgents,
        heatmapBuckets: action.heatmapBuckets,
        driftAlerts: action.driftAlerts,
        lastUpdated: action.ts,
        polling: true,
        error: null,
      };
    case "SET_ERROR":
      return { ...state, error: action.error };
    case "CLEAR_ERROR":
      return { ...state, error: null };
    case "SET_NAMESPACE":
      return { ...state, namespace: action.namespace };
    default:
      return state;
  }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

interface SwarmContextValue {
  state: SwarmState;
  dispatch: Dispatch<SwarmAction>;
}

const SwarmContext = createContext<SwarmContextValue | null>(null);

export function useSwarmContext(): SwarmContextValue {
  const ctx = useContext(SwarmContext);
  if (!ctx) throw new Error("useSwarmContext must be used within SwarmProvider");
  return ctx;
}

// ---------------------------------------------------------------------------
// Poll intervals (ADR-045 §D6)
// ---------------------------------------------------------------------------

const ACTIVE_SWARM_POLL_MS = 5_000;
const IDLE_POLL_MS = 30_000;

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

interface SwarmProviderProps {
  children: ReactNode;
  initialNamespace?: string;
}

export function SwarmProvider({ children, initialNamespace = "local" }: SwarmProviderProps) {
  const [state, dispatch] = useReducer(swarmReducer, {
    namespace: initialNamespace,
    agents: [],
    handoffs: [],
    cycles: [],
    heatmap: [],
    heatmapAgents: [],
    heatmapBuckets: [],
    driftAlerts: [],
    lastUpdated: 0,
    polling: false,
    error: null,
  });

  const stateRef = useRef(state);
  stateRef.current = state;

  const tick = useCallback(async () => {
    const { namespace } = stateRef.current;

    try {
      // Parallel fetch — all three in one round-trip budget
      const [queueTasks, completedTasks, doneTasks] = await Promise.all([
        fetchAgentQueues(namespace),
        fetchRecentCompleted(namespace),
        fetchDoneTasks(namespace),
      ]);

      const allTasks = [...queueTasks, ...completedTasks, ...doneTasks];

      // Derive aggregates
      const agents = computeAgentSummaries(queueTasks, completedTasks, doneTasks);
      const handoffs = deriveHandoffs(allTasks);
      const cycles = deriveCycles(allTasks);
      const driftAlerts = deriveDriftAlerts(agents);

      const heatmapAgents = agents.map((a) => a.name);
      const heatmapBuckets = buildHeatmapBuckets(24);
      const heatmap = deriveHeatmap(allTasks, heatmapAgents, heatmapBuckets);

      dispatch({
        type: "SET_DATA",
        agents,
        handoffs,
        cycles,
        heatmap,
        heatmapAgents,
        heatmapBuckets,
        driftAlerts,
        ts: Date.now(),
      });
    } catch (err) {
      dispatch({
        type: "SET_ERROR",
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }, []);

  // Adaptive polling: 5s when swarm is active, 30s when idle
  useEffect(() => {
    tick();

    let timerId: ReturnType<typeof setTimeout>;

    function schedule() {
      const hasActive = stateRef.current.agents.some((a) => a.activeTasks > 0);
      const interval = hasActive ? ACTIVE_SWARM_POLL_MS : IDLE_POLL_MS;

      timerId = setTimeout(async () => {
        await tick();
        schedule();
      }, interval);
    }

    schedule();

    return () => clearTimeout(timerId);
  }, [tick]);

  return <SwarmContext.Provider value={{ state, dispatch }}>{children}</SwarmContext.Provider>;
}
