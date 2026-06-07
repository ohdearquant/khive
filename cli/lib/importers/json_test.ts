/**
 * Tests for cli/lib/importers/json.ts (ADR-055 §2 P0 — JSON adapter).
 */

import { assertEquals, assertThrows } from "@std/assert";
import { adaptJson } from "./json.ts";

Deno.test("adaptJson — array of entities", () => {
  const text = JSON.stringify([
    {
      id: "10000000-0000-0000-0000-000000000001",
      name: "LoRA",
      kind: "concept",
      description: "Low-rank adaptation",
      year: 2021,
    },
    {
      id: "20000000-0000-0000-0000-000000000002",
      name: "Sinkhorn",
      kind: "concept",
    },
  ]);
  const r = adaptJson(text);
  assertEquals(r.entities.length, 2);
  assertEquals(r.edges.length, 0);
  assertEquals(r.entities[0].name, "LoRA");
  // description is a top-level field (ADR-048), not in properties.
  assertEquals(r.entities[0].description, "Low-rank adaptation");
  assertEquals(r.entities[0].properties["description"], undefined);
  assertEquals(r.entities[0].properties["year"], 2021);
});

Deno.test("adaptJson — entity missing kind without defaultKind is fatal (throws)", () => {
  // ADR-055 §5: missing required fields are fatal, never silently promoted to warnings.
  const text = JSON.stringify([{ name: "X" }]);
  assertThrows(() => adaptJson(text), Error, "kind");
});

Deno.test("adaptJson — defaultKind fills in missing kind", () => {
  const text = JSON.stringify([{ name: "X" }, { name: "Y" }]);
  const r = adaptJson(text, "concept");
  assertEquals(r.entities.length, 2);
  assertEquals(r.entities[0].kind, "concept");
});

Deno.test("adaptJson — edge objects detected by source+target", () => {
  const text = JSON.stringify([
    {
      source: "a",
      target: "b",
      relation: "depends_on",
      weight: 0.8,
    },
  ]);
  const r = adaptJson(text);
  assertEquals(r.edges.length, 1);
  assertEquals(r.entities.length, 0);
  assertEquals(r.edges[0].source, "a");
  assertEquals(r.edges[0].weight, 0.8);
});

Deno.test("adaptJson — mixed array (entities + edges)", () => {
  const text = JSON.stringify([
    { name: "A", kind: "concept" },
    { name: "B", kind: "concept" },
    { source: "a", target: "b", relation: "depends_on" },
  ]);
  const r = adaptJson(text);
  assertEquals(r.entities.length, 2);
  assertEquals(r.edges.length, 1);
});

Deno.test("adaptJson — invalid top-level JSON throws", () => {
  assertThrows(() => adaptJson("not-json"), Error, "parse");
});

Deno.test("adaptJson — non-array top-level throws", () => {
  assertThrows(() => adaptJson('{"not":"an-array"}'), Error, "array");
});

Deno.test("adaptJson — properties object passed through and merged", () => {
  const text = JSON.stringify([
    {
      name: "X",
      kind: "concept",
      properties: { score: 42 },
      extra: "top-level",
    },
  ]);
  const r = adaptJson(text);
  assertEquals(r.entities.length, 1);
  assertEquals(r.entities[0].properties["score"], 42);
  assertEquals(r.entities[0].properties["extra"], "top-level");
});

Deno.test("adaptJson — tags array preserved as top-level field", () => {
  const text = JSON.stringify([
    { name: "X", kind: "concept", tags: ["alpha", "beta"] },
  ]);
  const r = adaptJson(text);
  assertEquals(r.entities[0].tags, ["alpha", "beta"]);
});

Deno.test("adaptJson — case-insensitive field lookup for entity", () => {
  const text = JSON.stringify([
    { Name: "CaseTest", Kind: "concept", Description: "Mixed case keys" },
  ]);
  const r = adaptJson(text);
  assertEquals(r.entities.length, 1);
  assertEquals(r.entities[0].name, "CaseTest");
  assertEquals(r.entities[0].kind, "concept");
  assertEquals(r.entities[0].description, "Mixed case keys");
});

Deno.test("adaptJson — case-insensitive edge detection (Source/Target)", () => {
  const text = JSON.stringify([
    { Source: "a", Target: "b", relation: "depends_on" },
  ]);
  const r = adaptJson(text);
  assertEquals(r.edges.length, 1);
  assertEquals(r.edges[0].source, "a");
  assertEquals(r.edges[0].target, "b");
});

Deno.test("adaptJson — description is top-level field, not in properties", () => {
  const text = JSON.stringify([
    { name: "X", kind: "concept", description: "My description" },
  ]);
  const r = adaptJson(text);
  assertEquals(r.entities[0].description, "My description");
  assertEquals(r.entities[0].properties["description"], undefined);
});
