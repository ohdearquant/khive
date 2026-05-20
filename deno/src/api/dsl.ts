// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * DSL builder: route params → ops string (ADR-044 D1).
 *
 * Pure functions — no I/O, no side effects. Each function produces a
 * well-formed DSL op string for the `request` MCP tool.
 *
 * DSL syntax (ADR-020):
 *   verb(arg="value", arg=123, arg=true, arg=["a","b"])
 *
 * Rules:
 * - String values are always double-quoted and escaped.
 * - Numeric values are unquoted.
 * - Boolean values are unquoted (true/false).
 * - Array values use ["elem",...] syntax.
 * - undefined/null values are omitted.
 */

// ---------------------------------------------------------------------------
// Core builder
// ---------------------------------------------------------------------------

type DslValue = string | number | boolean | string[] | number[] | undefined | null;
type DslArgs = Record<string, DslValue>;

/** Serialize a single DSL argument value. */
function serializeValue(v: DslValue): string {
  if (v === undefined || v === null) return "";
  if (typeof v === "string") return `"${v.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
  if (typeof v === "number") return String(v);
  if (typeof v === "boolean") return String(v);
  if (Array.isArray(v)) {
    const inner = v.map((el) => serializeValue(el as DslValue)).join(",");
    return `[${inner}]`;
  }
  return `"${String(v)}"`;
}

/**
 * Build a single DSL op string.
 *
 * @param verb  The verb name (e.g. "list", "create")
 * @param args  Named arguments; undefined/null entries are omitted
 */
export function buildOp(verb: string, args: DslArgs): string {
  const parts: string[] = [];
  for (const [key, value] of Object.entries(args)) {
    if (value === undefined || value === null) continue;
    parts.push(`${key}=${serializeValue(value)}`);
  }
  return `${verb}(${parts.join(", ")})`;
}

/**
 * Wrap multiple ops in a batch (parallel by default).
 * A single op is passed through without wrapping.
 */
export function buildBatch(ops: string[]): string {
  if (ops.length === 1) return ops[0];
  return `[${ops.join(", ")}]`;
}

// ---------------------------------------------------------------------------
// Entity ops
// ---------------------------------------------------------------------------

export interface ListEntitiesParams {
  entity_kind?: string;
  limit?: number;
  offset?: number;
  namespace?: string;
}

export function buildListEntities(params: ListEntitiesParams): string {
  return buildOp("list", {
    kind: "entity",
    entity_kind: params.entity_kind,
    limit: params.limit,
    offset: params.offset,
    namespace: params.namespace,
  });
}

export interface GetEntityParams {
  id: string;
  namespace?: string;
}

export function buildGetEntity(params: GetEntityParams): string {
  return buildOp("get", { id: params.id, namespace: params.namespace });
}

export interface CreateEntityParams {
  entity_kind: string;
  name: string;
  description?: string;
  namespace?: string;
  tags?: string[];
  properties?: Record<string, unknown>;
}

export function buildCreateEntity(params: CreateEntityParams): string {
  // properties is serialized as a JSON string arg since the DSL does not support nested objects
  const base: DslArgs = {
    kind: "entity",
    entity_kind: params.entity_kind,
    name: params.name,
    description: params.description,
    namespace: params.namespace,
    tags: params.tags,
  };
  if (params.properties && Object.keys(params.properties).length > 0) {
    base.properties = JSON.stringify(params.properties);
  }
  return buildOp("create", base);
}

export interface UpdateEntityParams {
  id: string;
  name?: string;
  description?: string;
  namespace?: string;
  tags?: string[];
  properties?: Record<string, unknown>;
}

export function buildUpdateEntity(params: UpdateEntityParams): string {
  const base: DslArgs = {
    id: params.id,
    name: params.name,
    description: params.description,
    namespace: params.namespace,
    tags: params.tags,
  };
  if (params.properties && Object.keys(params.properties).length > 0) {
    base.properties = JSON.stringify(params.properties);
  }
  return buildOp("update", base);
}

export interface DeleteEntityParams {
  id: string;
  hard?: boolean;
  namespace?: string;
}

export function buildDeleteEntity(params: DeleteEntityParams): string {
  return buildOp("delete", {
    id: params.id,
    hard: params.hard,
    namespace: params.namespace,
  });
}

// ---------------------------------------------------------------------------
// Edge ops
// ---------------------------------------------------------------------------

export interface ListEdgesParams {
  source_id?: string;
  target_id?: string;
  relation?: string;
  namespace?: string;
}

export function buildListEdges(params: ListEdgesParams): string {
  return buildOp("list", {
    kind: "edge",
    source_id: params.source_id,
    target_id: params.target_id,
    relation: params.relation,
    namespace: params.namespace,
  });
}

export interface CreateEdgeParams {
  source_id: string;
  target_id: string;
  relation: string;
  weight?: number;
  namespace?: string;
}

export function buildCreateEdge(params: CreateEdgeParams): string {
  return buildOp("link", {
    source_id: params.source_id,
    target_id: params.target_id,
    relation: params.relation,
    weight: params.weight,
    namespace: params.namespace,
  });
}

export interface DeleteEdgeParams {
  id: string;
  namespace?: string;
}

export function buildDeleteEdge(params: DeleteEdgeParams): string {
  return buildOp("delete", { id: params.id, namespace: params.namespace });
}

// ---------------------------------------------------------------------------
// Task ops (GTD pack)
// ---------------------------------------------------------------------------

export interface ListTasksParams {
  status?: string;
  assignee?: string;
  priority?: string;
  limit?: number;
  offset?: number;
  namespace?: string;
}

export function buildListTasks(params: ListTasksParams): string {
  return buildOp("tasks", {
    status: params.status,
    assignee: params.assignee,
    priority: params.priority,
    limit: params.limit,
    offset: params.offset,
    namespace: params.namespace,
  });
}

export interface CreateTaskParams {
  title: string;
  priority?: string;
  status?: string;
  assignee?: string;
  due?: string;
  tags?: string[];
  depends_on?: string[];
  namespace?: string;
}

export function buildCreateTask(params: CreateTaskParams): string {
  return buildOp("assign", {
    title: params.title,
    priority: params.priority,
    status: params.status,
    assignee: params.assignee,
    due: params.due,
    tags: params.tags,
    depends_on: params.depends_on,
    namespace: params.namespace,
  });
}

export interface TransitionTaskParams {
  id: string;
  status: string;
  note?: string;
  namespace?: string;
}

export function buildTransitionTask(params: TransitionTaskParams): string {
  return buildOp("transition", {
    id: params.id,
    status: params.status,
    note: params.note,
    namespace: params.namespace,
  });
}

export interface CompleteTaskParams {
  id: string;
  result?: string;
  namespace?: string;
}

export function buildCompleteTask(params: CompleteTaskParams): string {
  return buildOp("complete", {
    id: params.id,
    result: params.result,
    namespace: params.namespace,
  });
}

// ---------------------------------------------------------------------------
// Search ops
// ---------------------------------------------------------------------------

export interface SearchParams {
  query: string;
  kind?: string;
  limit?: number;
  namespace?: string;
}

export function buildSearch(params: SearchParams): string {
  return buildOp("search", {
    query: params.query,
    kind: params.kind,
    limit: params.limit,
    namespace: params.namespace,
  });
}

// ---------------------------------------------------------------------------
// Graph traversal ops
// ---------------------------------------------------------------------------

export interface TraverseParams {
  roots: string[];
  max_depth?: number;
  relations?: string[];
  direction?: string;
  namespace?: string;
}

export function buildTraverse(params: TraverseParams): string {
  return buildOp("traverse", {
    roots: params.roots,
    max_depth: params.max_depth,
    relations: params.relations,
    direction: params.direction,
    namespace: params.namespace,
  });
}

export interface NeighborsParams {
  node_id: string;
  direction?: string;
  relations?: string[];
  namespace?: string;
}

export function buildNeighbors(params: NeighborsParams): string {
  return buildOp("neighbors", {
    node_id: params.node_id,
    direction: params.direction,
    relations: params.relations,
    namespace: params.namespace,
  });
}
