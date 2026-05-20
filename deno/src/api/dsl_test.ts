// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Unit tests for the DSL builder (dsl.ts).
 *
 * Pure functions — no I/O required.
 */

import { assertEquals } from "@std/assert";
import {
  buildCompleteTask,
  buildCreateEdge,
  buildCreateEntity,
  buildCreateTask,
  buildDeleteEdge,
  buildDeleteEntity,
  buildGetEntity,
  buildListEdges,
  buildListEntities,
  buildListTasks,
  buildNeighbors,
  buildOp,
  buildSearch,
  buildTransitionTask,
  buildTraverse,
  buildUpdateEntity,
} from "./dsl.ts";

// ---------------------------------------------------------------------------
// buildOp core
// ---------------------------------------------------------------------------

Deno.test("buildOp: simple string args", () => {
  assertEquals(buildOp("get", { id: "abc" }), `get(id="abc")`);
});

Deno.test("buildOp: numeric args are unquoted", () => {
  assertEquals(buildOp("list", { limit: 10 }), "list(limit=10)");
});

Deno.test("buildOp: boolean args are unquoted", () => {
  assertEquals(buildOp("delete", { id: "abc", hard: true }), `delete(id="abc", hard=true)`);
});

Deno.test("buildOp: array args", () => {
  assertEquals(
    buildOp("traverse", { roots: ["a", "b"] }),
    `traverse(roots=["a","b"])`,
  );
});

Deno.test("buildOp: undefined args are omitted", () => {
  assertEquals(
    buildOp("create", { kind: "entity", description: undefined }),
    `create(kind="entity")`,
  );
});

Deno.test("buildOp: null args are omitted", () => {
  assertEquals(
    buildOp("create", { kind: "entity", description: null }),
    `create(kind="entity")`,
  );
});

Deno.test("buildOp: string with quotes is escaped", () => {
  const op = buildOp("create", { name: 'say "hello"' });
  assertEquals(op, `create(name="say \\"hello\\"")`);
});

Deno.test("buildOp: string with backslash is escaped", () => {
  const op = buildOp("create", { name: "path\\to\\file" });
  assertEquals(op, `create(name="path\\\\to\\\\file")`);
});

// ---------------------------------------------------------------------------
// Entity ops
// ---------------------------------------------------------------------------

Deno.test("buildListEntities: minimal", () => {
  assertEquals(buildListEntities({}), `list(kind="entity")`);
});

Deno.test("buildListEntities: with entity_kind and pagination", () => {
  assertEquals(
    buildListEntities({ entity_kind: "concept", limit: 25, offset: 0 }),
    `list(kind="entity", entity_kind="concept", limit=25, offset=0)`,
  );
});

Deno.test("buildListEntities: with namespace", () => {
  assertEquals(
    buildListEntities({ namespace: "lambda:khive" }),
    `list(kind="entity", namespace="lambda:khive")`,
  );
});

Deno.test("buildGetEntity: with id", () => {
  assertEquals(buildGetEntity({ id: "abc12345" }), `get(id="abc12345")`);
});

Deno.test("buildGetEntity: with namespace", () => {
  assertEquals(
    buildGetEntity({ id: "abc12345", namespace: "ns" }),
    `get(id="abc12345", namespace="ns")`,
  );
});

Deno.test("buildCreateEntity: required fields", () => {
  assertEquals(
    buildCreateEntity({ entity_kind: "concept", name: "FlashAttention" }),
    `create(kind="entity", entity_kind="concept", name="FlashAttention")`,
  );
});

Deno.test("buildCreateEntity: with all fields", () => {
  const op = buildCreateEntity({
    entity_kind: "concept",
    name: "LoRA",
    description: "Low-rank adaptation",
    namespace: "papers",
    tags: ["ml", "fine-tuning"],
  });
  assertEquals(
    op,
    `create(kind="entity", entity_kind="concept", name="LoRA", description="Low-rank adaptation", namespace="papers", tags=["ml","fine-tuning"])`,
  );
});

Deno.test("buildUpdateEntity: minimal (id only)", () => {
  assertEquals(buildUpdateEntity({ id: "abc" }), `update(id="abc")`);
});

Deno.test("buildUpdateEntity: with patch fields", () => {
  assertEquals(
    buildUpdateEntity({ id: "abc", name: "NewName", description: "New desc" }),
    `update(id="abc", name="NewName", description="New desc")`,
  );
});

Deno.test("buildDeleteEntity: soft delete", () => {
  assertEquals(buildDeleteEntity({ id: "abc" }), `delete(id="abc")`);
});

Deno.test("buildDeleteEntity: hard delete", () => {
  assertEquals(buildDeleteEntity({ id: "abc", hard: true }), `delete(id="abc", hard=true)`);
});

// ---------------------------------------------------------------------------
// Edge ops
// ---------------------------------------------------------------------------

Deno.test("buildListEdges: minimal", () => {
  assertEquals(buildListEdges({}), `list(kind="edge")`);
});

Deno.test("buildListEdges: with filters", () => {
  assertEquals(
    buildListEdges({ source_id: "src1", relation: "implements" }),
    `list(kind="edge", source_id="src1", relation="implements")`,
  );
});

Deno.test("buildCreateEdge: required fields", () => {
  assertEquals(
    buildCreateEdge({ source_id: "a", target_id: "b", relation: "implements" }),
    `link(source_id="a", target_id="b", relation="implements")`,
  );
});

Deno.test("buildCreateEdge: with weight", () => {
  assertEquals(
    buildCreateEdge({ source_id: "a", target_id: "b", relation: "extends", weight: 0.8 }),
    `link(source_id="a", target_id="b", relation="extends", weight=0.8)`,
  );
});

Deno.test("buildDeleteEdge: minimal", () => {
  assertEquals(buildDeleteEdge({ id: "edge-id" }), `delete(id="edge-id")`);
});

// ---------------------------------------------------------------------------
// Task ops
// ---------------------------------------------------------------------------

Deno.test("buildListTasks: minimal", () => {
  assertEquals(buildListTasks({}), "tasks()");
});

Deno.test("buildListTasks: with filters", () => {
  assertEquals(
    buildListTasks({ status: "next", priority: "p1", limit: 10 }),
    `tasks(status="next", priority="p1", limit=10)`,
  );
});

Deno.test("buildCreateTask: required fields", () => {
  assertEquals(buildCreateTask({ title: "Fix bug" }), `assign(title="Fix bug")`);
});

Deno.test("buildCreateTask: with all fields", () => {
  assertEquals(
    buildCreateTask({ title: "Fix bug", priority: "p0", status: "next" }),
    `assign(title="Fix bug", priority="p0", status="next")`,
  );
});

Deno.test("buildTransitionTask: required fields", () => {
  assertEquals(
    buildTransitionTask({ id: "task-1", status: "active" }),
    `transition(id="task-1", status="active")`,
  );
});

Deno.test("buildTransitionTask: with note", () => {
  assertEquals(
    buildTransitionTask({ id: "task-1", status: "done", note: "shipped" }),
    `transition(id="task-1", status="done", note="shipped")`,
  );
});

Deno.test("buildCompleteTask: minimal", () => {
  assertEquals(buildCompleteTask({ id: "task-1" }), `complete(id="task-1")`);
});

Deno.test("buildCompleteTask: with result", () => {
  assertEquals(
    buildCompleteTask({ id: "task-1", result: "PR merged" }),
    `complete(id="task-1", result="PR merged")`,
  );
});

// ---------------------------------------------------------------------------
// Search ops
// ---------------------------------------------------------------------------

Deno.test("buildSearch: minimal", () => {
  assertEquals(buildSearch({ query: "attention" }), `search(query="attention")`);
});

Deno.test("buildSearch: with kind and limit", () => {
  assertEquals(
    buildSearch({ query: "attention", kind: "entity", limit: 10 }),
    `search(query="attention", kind="entity", limit=10)`,
  );
});

// ---------------------------------------------------------------------------
// Graph traversal ops
// ---------------------------------------------------------------------------

Deno.test("buildTraverse: minimal", () => {
  assertEquals(
    buildTraverse({ roots: ["root-id"] }),
    `traverse(roots=["root-id"])`,
  );
});

Deno.test("buildTraverse: with depth and relations", () => {
  assertEquals(
    buildTraverse({ roots: ["a", "b"], max_depth: 2, relations: ["implements", "extends"] }),
    `traverse(roots=["a","b"], max_depth=2, relations=["implements","extends"])`,
  );
});

Deno.test("buildNeighbors: minimal", () => {
  assertEquals(
    buildNeighbors({ node_id: "node-1" }),
    `neighbors(node_id="node-1")`,
  );
});

Deno.test("buildNeighbors: with direction and relations", () => {
  assertEquals(
    buildNeighbors({ node_id: "node-1", direction: "in", relations: ["contains"] }),
    `neighbors(node_id="node-1", direction="in", relations=["contains"])`,
  );
});
