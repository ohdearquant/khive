"use client";

import {
  useQuery,
  type UseQueryResult,
} from "@tanstack/react-query";
import type {
  Entity,
  EntityKind,
  ListResponse,
  NeighborEntry,
  Task,
  TaskStatus,
} from "./types";

// Base URL for the Deno HTTP gateway. Default matches `deno task server` (:8000).
// The Next.js rewrite in next.config.ts maps /api/server/* → gateway, so we
// use /api/server to avoid CORS in production. In dev we hit the gateway directly.
const GATEWAY_URL =
  typeof window !== "undefined"
    ? "/api/server"
    : (process.env.NEXT_PUBLIC_KHIVE_GATEWAY_URL ?? "http://localhost:8000");

async function gw<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${GATEWAY_URL}${path}`, {
    headers: { "Content-Type": "application/json" },
    ...init,
  });
  if (!res.ok) {
    const text = await res.text().catch(() => res.statusText);
    throw new Error(`${res.status} ${text}`);
  }
  return res.json() as Promise<T>;
}

// ── Entities ────────────────────────────────────────────────────────────────

export interface FetchEntitiesOptions {
  kinds?: EntityKind[];
  query?: string;
  limit?: number;
  offset?: number;
}

export function fetchEntities(
  opts: FetchEntitiesOptions = {},
): Promise<ListResponse<Entity>> {
  const params = new URLSearchParams();
  if (opts.query) params.set("q", opts.query);
  if (opts.kinds?.length) params.set("kind", opts.kinds.join(","));
  if (opts.limit != null) params.set("limit", String(opts.limit));
  if (opts.offset != null) params.set("offset", String(opts.offset));
  const qs = params.toString();
  return gw<ListResponse<Entity>>(`/api/entities${qs ? `?${qs}` : ""}`);
}

export function fetchEntity(id: string): Promise<Entity> {
  return gw<Entity>(`/api/entities/${encodeURIComponent(id)}`);
}

export function fetchNeighbors(id: string): Promise<NeighborEntry[]> {
  return gw<NeighborEntry[]>(
    `/api/entities/${encodeURIComponent(id)}/neighbors`,
  );
}

// ── Tasks ────────────────────────────────────────────────────────────────────

export interface FetchTasksOptions {
  status?: TaskStatus;
  assignee?: string;
  priority?: string;
  limit?: number;
  offset?: number;
}

export function fetchTasks(
  opts: FetchTasksOptions = {},
): Promise<ListResponse<Task>> {
  const params = new URLSearchParams();
  if (opts.status) params.set("status", opts.status);
  if (opts.assignee) params.set("assignee", opts.assignee);
  if (opts.priority) params.set("priority", opts.priority);
  if (opts.limit != null) params.set("limit", String(opts.limit));
  if (opts.offset != null) params.set("offset", String(opts.offset));
  const qs = params.toString();
  return gw<ListResponse<Task>>(`/api/tasks${qs ? `?${qs}` : ""}`);
}

export function fetchTask(id: string): Promise<Task> {
  return gw<Task>(`/api/tasks/${encodeURIComponent(id)}`);
}

// ── TanStack Query hooks ─────────────────────────────────────────────────────

export function useEntities(
  opts: FetchEntitiesOptions = {},
): UseQueryResult<ListResponse<Entity>> {
  return useQuery({
    queryKey: ["entities", opts],
    queryFn: () => fetchEntities(opts),
    staleTime: 60_000,
  });
}

export function useEntity(id: string | null): UseQueryResult<Entity> {
  return useQuery({
    queryKey: ["entity", id],
    queryFn: () => fetchEntity(id!),
    enabled: id != null,
    staleTime: 60_000,
  });
}

export function useNeighbors(
  id: string | null,
): UseQueryResult<NeighborEntry[]> {
  return useQuery({
    queryKey: ["neighbors", id],
    queryFn: () => fetchNeighbors(id!),
    enabled: id != null,
    staleTime: 60_000,
  });
}

export function useTasks(
  opts: FetchTasksOptions = {},
): UseQueryResult<ListResponse<Task>> {
  return useQuery({
    queryKey: ["tasks", opts],
    queryFn: () => fetchTasks(opts),
    staleTime: 60_000,
  });
}

export function useTask(id: string | null): UseQueryResult<Task> {
  return useQuery({
    queryKey: ["task", id],
    queryFn: () => fetchTask(id!),
    enabled: id != null,
    staleTime: 60_000,
  });
}
