// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Shared TypeScript types for the khive HTTP API.
 *
 * Every route handler returns one of these shapes. The ok discriminant lets
 * callers branch without inspecting HTTP status codes.
 */

// ---------------------------------------------------------------------------
// Envelope shapes (ADR-044 D5)
// ---------------------------------------------------------------------------

export interface OkResponse<T> {
  ok: true;
  result: T;
}

export interface ErrorResponse {
  ok: false;
  error: {
    code: ErrorCode;
    message: string;
  };
}

export type ApiResponse<T> = OkResponse<T> | ErrorResponse;

// ---------------------------------------------------------------------------
// Error codes → HTTP status mapping (ADR-044 D5)
// ---------------------------------------------------------------------------

export type ErrorCode =
  | "BAD_REQUEST"
  | "UNAUTHORIZED"
  | "FORBIDDEN"
  | "NOT_FOUND"
  | "CONFLICT"
  | "UNPROCESSABLE_ENTITY"
  | "NOT_IMPLEMENTED"
  | "INTERNAL_SERVER_ERROR";

export const ERROR_STATUS: Record<ErrorCode, number> = {
  BAD_REQUEST: 400,
  UNAUTHORIZED: 401,
  FORBIDDEN: 403,
  NOT_FOUND: 404,
  CONFLICT: 409,
  UNPROCESSABLE_ENTITY: 422,
  NOT_IMPLEMENTED: 501,
  INTERNAL_SERVER_ERROR: 500,
};

// ---------------------------------------------------------------------------
// Entity shapes
// ---------------------------------------------------------------------------

export type EntityKind = "concept" | "document" | "dataset" | "project" | "person" | "org";

export interface Entity {
  id: string;
  full_id?: string;
  kind: EntityKind;
  name: string;
  description?: string;
  namespace?: string;
  tags?: string[];
  properties?: Record<string, unknown>;
  created_at?: string;
  updated_at?: string;
}

export interface EntityListResult {
  items: Entity[];
  total?: number;
  limit: number;
  offset: number;
}

export interface CreateEntityBody {
  kind: EntityKind;
  name: string;
  description?: string;
  namespace?: string;
  tags?: string[];
  properties?: Record<string, unknown>;
}

export interface UpdateEntityBody {
  name?: string;
  description?: string;
  tags?: string[];
  properties?: Record<string, unknown>;
}

// ---------------------------------------------------------------------------
// Edge shapes
// ---------------------------------------------------------------------------

export type EdgeRelation =
  | "contains"
  | "part_of"
  | "instance_of"
  | "extends"
  | "variant_of"
  | "introduced_by"
  | "supersedes"
  | "depends_on"
  | "enables"
  | "implements"
  | "competes_with"
  | "composed_with"
  | "annotates";

export interface Edge {
  id: string;
  full_id?: string;
  source_id: string;
  target_id: string;
  relation: EdgeRelation;
  weight?: number;
  namespace?: string;
  created_at?: string;
}

export interface CreateEdgeBody {
  source_id: string;
  target_id: string;
  relation: EdgeRelation;
  weight?: number;
  namespace?: string;
}

// ---------------------------------------------------------------------------
// Task shapes (GTD pack)
// ---------------------------------------------------------------------------

export type TaskStatus = "inbox" | "next" | "waiting" | "someday" | "active" | "done" | "cancelled";
export type TaskPriority = "p0" | "p1" | "p2" | "p3";

export interface Task {
  id: string;
  full_id?: string;
  title: string;
  status: TaskStatus;
  priority: TaskPriority;
  assignee?: string;
  due?: string;
  tags?: string[];
  completed_at?: string;
  created_at?: string;
}

export interface CreateTaskBody {
  title: string;
  priority?: TaskPriority;
  status?: TaskStatus;
  assignee?: string;
  due?: string;
  tags?: string[];
  depends_on?: string[];
}

export interface TransitionTaskBody {
  status: TaskStatus;
  note?: string;
}

export interface CompleteTaskBody {
  result?: string;
}

// ---------------------------------------------------------------------------
// Search shapes
// ---------------------------------------------------------------------------

export interface SearchResult {
  items: Array<Entity | Task | Record<string, unknown>>;
  total?: number;
}

// ---------------------------------------------------------------------------
// Graph traversal shapes
// ---------------------------------------------------------------------------

export interface TraversalNode {
  id: string;
  full_id?: string;
  name?: string;
  kind?: string;
  relation?: string;
  depth?: number;
  source_id?: string;
}

export interface TraversalResult {
  nodes: TraversalNode[];
  total: number;
}

// ---------------------------------------------------------------------------
// Raw request DSL passthrough
// ---------------------------------------------------------------------------

export interface RequestBody {
  ops: string;
}

export interface RequestResult {
  results: Array<{ ok: boolean; tool?: string; result?: unknown; error?: string }>;
  summary: {
    total: number;
    succeeded: number;
    failed: number;
  };
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

export interface ResolvedAuth {
  namespace?: string;
}
