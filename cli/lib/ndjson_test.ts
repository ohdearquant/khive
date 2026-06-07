/**
 * Tests for NDJSON parsing utilities — specifically parseEdgeLine with ADR-048 field names.
 */

import { assertEquals } from "@std/assert";
import { parseEdgeLine, parseEntityLine } from "./ndjson.ts";

// ─── parseEntityLine ──────────────────────────────────────────────────────────

Deno.test("parseEntityLine: accepts valid entity", () => {
  const entity = parseEntityLine({
    id: "00000000-0000-0000-0000-000000000001",
    name: "LoRA",
    kind: "concept",
  });
  assertEquals(entity !== null, true);
  assertEquals(entity!.id, "00000000-0000-0000-0000-000000000001");
  assertEquals(entity!.name, "LoRA");
  assertEquals(entity!.kind, "concept");
});

Deno.test("parseEntityLine: rejects missing id", () => {
  const entity = parseEntityLine({ name: "LoRA", kind: "concept" });
  assertEquals(entity, null);
});

Deno.test("parseEntityLine: rejects invalid UUID id", () => {
  const entity = parseEntityLine({ id: "not-a-uuid", name: "LoRA", kind: "concept" });
  assertEquals(entity, null);
});

Deno.test("parseEntityLine: rejects unknown kind", () => {
  const entity = parseEntityLine({
    id: "00000000-0000-0000-0000-000000000001",
    name: "LoRA",
    kind: "unknown_kind",
  });
  assertEquals(entity, null);
});

// ─── parseEdgeLine (ADR-048 field names) ─────────────────────────────────────

Deno.test("parseEdgeLine: accepts edge with ADR-048 fields (edge_id/source/target)", () => {
  const edge = parseEdgeLine({
    edge_id: "eeeeeeee-0000-0000-0000-000000000001",
    source: "00000000-0000-0000-0000-000000000001",
    target: "00000000-0000-0000-0000-000000000002",
    relation: "implements",
    weight: 1.0,
    properties: {},
  });
  assertEquals(edge !== null, true);
  assertEquals(edge!.edge_id, "eeeeeeee-0000-0000-0000-000000000001");
  assertEquals(edge!.source, "00000000-0000-0000-0000-000000000001");
  assertEquals(edge!.target, "00000000-0000-0000-0000-000000000002");
  assertEquals(edge!.relation, "implements");
});

Deno.test("parseEdgeLine: rejects edge with old field names (id/source_id/target_id)", () => {
  const edge = parseEdgeLine({
    id: "eeeeeeee-0000-0000-0000-000000000001",
    source_id: "00000000-0000-0000-0000-000000000001",
    target_id: "00000000-0000-0000-0000-000000000002",
    relation: "implements",
  });
  assertEquals(edge, null);
});

Deno.test("parseEdgeLine: rejects missing edge_id", () => {
  const edge = parseEdgeLine({
    source: "00000000-0000-0000-0000-000000000001",
    target: "00000000-0000-0000-0000-000000000002",
    relation: "implements",
  });
  assertEquals(edge, null);
});

Deno.test("parseEdgeLine: rejects non-UUID edge_id", () => {
  const edge = parseEdgeLine({
    edge_id: "not-a-uuid",
    source: "00000000-0000-0000-0000-000000000001",
    target: "00000000-0000-0000-0000-000000000002",
    relation: "implements",
  });
  assertEquals(edge, null);
});

Deno.test("parseEdgeLine: rejects empty source", () => {
  const edge = parseEdgeLine({
    edge_id: "eeeeeeee-0000-0000-0000-000000000001",
    source: "",
    target: "00000000-0000-0000-0000-000000000002",
    relation: "implements",
  });
  assertEquals(edge, null);
});

Deno.test("parseEdgeLine: rejects empty target", () => {
  const edge = parseEdgeLine({
    edge_id: "eeeeeeee-0000-0000-0000-000000000001",
    source: "00000000-0000-0000-0000-000000000001",
    target: "",
    relation: "implements",
  });
  assertEquals(edge, null);
});

Deno.test("parseEdgeLine: rejects unknown relation", () => {
  const edge = parseEdgeLine({
    edge_id: "eeeeeeee-0000-0000-0000-000000000001",
    source: "00000000-0000-0000-0000-000000000001",
    target: "00000000-0000-0000-0000-000000000002",
    relation: "bogus_relation",
  });
  assertEquals(edge, null);
});

Deno.test("parseEdgeLine: accepts remote ref as target (non-UUID string)", () => {
  // source is a local UUID; target is a remote reference — both valid for parseEdgeLine
  const edge = parseEdgeLine({
    edge_id: "eeeeeeee-0000-0000-0000-000000000001",
    source: "00000000-0000-0000-0000-000000000001",
    target: "lattice:00000000-0000-0000-0000-000000000099",
    relation: "depends_on",
    weight: 0.8,
    properties: {},
  });
  assertEquals(edge !== null, true);
  assertEquals(edge!.target, "lattice:00000000-0000-0000-0000-000000000099");
});

Deno.test("parseEdgeLine: accepts null input gracefully", () => {
  assertEquals(parseEdgeLine(null), null);
});

Deno.test("parseEdgeLine: accepts array input gracefully", () => {
  assertEquals(parseEdgeLine([1, 2, 3]), null);
});
