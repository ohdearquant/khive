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
import { fetchAgentQueues, fetchDoneTasks } from "@/lib/swarm/queries";
import { deriveThroughputBuckets } from "@/lib/swarm/derive";
import type { Task, ThroughputBucket } from "@/lib/swarm/types";

// ---------------------------------------------------------------------------
// State shape
// ---------------------------------------------------------------------------

interface DrilldownState {
  agentName: string;
  namespace: string;
  activeTasks: Task[];
  nextTasks: Task[];
  recentCompletions: Task[];
  throughputBuckets: ThroughputBucket[];
  lastUpdated: number;
  error: string | null;
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

type DrilldownAction =
  | {
      type: "SET_DATA";
      activeTasks: Task[];
      nextTasks: Task[];
      recentCompletions: Task[];
      throughputBuckets: ThroughputBucket[];
      ts: number;
    }
  | { type: "SET_ERROR"; error: string };

// ---------------------------------------------------------------------------
// Reducer
// ---------------------------------------------------------------------------

function drilldownReducer(state: DrilldownState, action: DrilldownAction): DrilldownState {
  switch (action.type) {
    case "SET_DATA":
      return {
        ...state,
        activeTasks: action.activeTasks,
        nextTasks: action.nextTasks,
        recentCompletions: action.recentCompletions,
        throughputBuckets: action.throughputBuckets,
        lastUpdated: action.ts,
        error: null,
      };
    case "SET_ERROR":
      return { ...state, error: action.error };
    default:
      return state;
  }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

interface DrilldownContextValue {
  state: DrilldownState;
  dispatch: Dispatch<DrilldownAction>;
}

const AgentDrilldownContext = createContext<DrilldownContextValue | null>(null);

export function useAgentDrilldownContext(): DrilldownContextValue {
  const ctx = useContext(AgentDrilldownContext);
  if (!ctx) {
    throw new Error("useAgentDrilldownContext must be used within AgentDrilldownProvider");
  }
  return ctx;
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

interface AgentDrilldownProviderProps {
  children: ReactNode;
  agentName: string;
  namespace?: string;
}

export function AgentDrilldownProvider({
  children,
  agentName,
  namespace = "local",
}: AgentDrilldownProviderProps) {
  const [state, dispatch] = useReducer(drilldownReducer, {
    agentName,
    namespace,
    activeTasks: [],
    nextTasks: [],
    recentCompletions: [],
    throughputBuckets: [],
    lastUpdated: 0,
    error: null,
  });

  const stateRef = useRef(state);
  stateRef.current = state;

  const tick = useCallback(async () => {
    try {
      const [queueTasks, doneTasks] = await Promise.all([
        fetchAgentQueues(namespace),
        fetchDoneTasks(namespace),
      ]);

      const activeTasks = queueTasks.filter(
        (t) => t.assignee === agentName && t.status === "active",
      );
      const nextTasks = queueTasks.filter((t) => t.assignee === agentName && t.status === "next");

      const since = Date.now() - 3_600_000;
      const recentCompletions = doneTasks
        .filter(
          (t) =>
            t.assignee === agentName &&
            t.completedAt !== null &&
            t.completedAt !== undefined &&
            t.completedAt > since,
        )
        .sort((a, b) => (b.completedAt ?? 0) - (a.completedAt ?? 0))
        .slice(0, 20);

      const throughputBuckets = deriveThroughputBuckets(
        doneTasks.filter((t) => t.completedAt && t.completedAt > since),
        agentName,
        12,
        5 * 60 * 1000,
      );

      dispatch({
        type: "SET_DATA",
        activeTasks,
        nextTasks,
        recentCompletions,
        throughputBuckets,
        ts: Date.now(),
      });
    } catch (err) {
      dispatch({
        type: "SET_ERROR",
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }, [agentName, namespace]);

  useEffect(() => {
    tick();
    const id = setInterval(tick, 10_000);
    return () => clearInterval(id);
  }, [tick]);

  return (
    <AgentDrilldownContext.Provider value={{ state, dispatch }}>
      {children}
    </AgentDrilldownContext.Provider>
  );
}
