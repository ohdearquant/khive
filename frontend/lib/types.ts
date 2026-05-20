// Closed taxonomies from ADR-001 and ADR-002 — typed at compile time, never extended without an ADR.

export type EntityKind =
  | "concept"
  | "document"
  | "dataset"
  | "project"
  | "person"
  | "org";

export type EdgeRelation =
  // Structure
  | "contains"
  | "part_of"
  | "instance_of"
  // Derivation
  | "extends"
  | "variant_of"
  | "introduced_by"
  | "supersedes"
  // Dependency
  | "depends_on"
  | "enables"
  // Implementation
  | "implements"
  // Lateral
  | "competes_with"
  | "composed_with"
  // Annotation
  | "annotates";

export type EdgeCategory =
  | "structure"
  | "derivation"
  | "dependency"
  | "implementation"
  | "lateral"
  | "annotation";

export const EDGE_CATEGORY: Record<EdgeRelation, EdgeCategory> = {
  contains: "structure",
  part_of: "structure",
  instance_of: "structure",
  extends: "derivation",
  variant_of: "derivation",
  introduced_by: "derivation",
  supersedes: "derivation",
  depends_on: "dependency",
  enables: "dependency",
  implements: "implementation",
  competes_with: "lateral",
  composed_with: "lateral",
  annotates: "annotation",
};

// GTD statuses from ADR-026 — 6 board columns (someday excluded)
export type TaskStatus =
  | "inbox"
  | "next"
  | "active"
  | "waiting"
  | "done"
  | "cancelled";

export const BOARD_STATUSES: TaskStatus[] = [
  "inbox",
  "next",
  "active",
  "waiting",
  "done",
  "cancelled",
];

export type Priority = "p0" | "p1" | "p2" | "p3";

// Entity record as returned by the gateway
export interface Entity {
  id: string;
  full_id: string;
  name: string;
  kind: EntityKind;
  description?: string;
  properties?: Record<string, string>;
  tags?: string[];
  created_at: string;
  updated_at: string;
  edge_count?: number;
}

// Edge record
export interface Edge {
  id: string;
  source_id: string;
  target_id: string;
  relation: EdgeRelation;
  weight: number;
}

// Neighbor result entry
export interface NeighborEntry {
  entity_id: string;
  relation: EdgeRelation;
  direction: "outbound" | "inbound";
  weight: number;
}

// Task (GTD note with kind=task)
export interface Task {
  id: string;
  full_id: string;
  title: string;
  status: TaskStatus;
  priority: Priority;
  assignee?: string;
  due?: string;
  tags?: string[];
  description?: string;
  created_at: string;
  updated_at: string;
  properties?: Record<string, string>;
}

// Paginated list response
export interface ListResponse<T> {
  items: T[];
  total: number;
  offset: number;
  limit: number;
}

// Three-state view model from ADR-047
export type ViewState<T> =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ok"; data: T };
