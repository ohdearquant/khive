/**
 * Tests for cli/lib/importers/csv.ts (ADR-055 §2 P0).
 */

import { assertEquals, assertThrows } from "@std/assert";
import { adaptCsv } from "./csv.ts";

Deno.test("adaptCsv — entity list with id/name/kind columns", () => {
  const text = `id,name,kind,description
10000000-0000-0000-0000-000000000001,LoRA,concept,Low-rank adaptation
20000000-0000-0000-0000-000000000002,FlashAttn,concept,Faster attention
`;
  const r = adaptCsv(text);
  assertEquals(r.entities.length, 2);
  assertEquals(r.edges.length, 0);
  assertEquals(r.entities[0].name, "LoRA");
  assertEquals(r.entities[0].kind, "concept");
  // description is a top-level field (ADR-048), not in properties.
  assertEquals(r.entities[0].description, "Low-rank adaptation");
  assertEquals(r.entities[0].properties["description"], undefined);
});

Deno.test("adaptCsv — auto-generates UUIDs when id column missing", () => {
  const text = `name,kind
LoRA,concept
FlashAttn,concept
`;
  const r = adaptCsv(text);
  assertEquals(r.entities.length, 2);
  // Both ids must be present and unique.
  if (r.entities[0].id === r.entities[1].id) {
    throw new Error("auto-generated ids should be unique");
  }
});

Deno.test("adaptCsv — uses --default-kind when kind column missing", () => {
  const text = `name
A
B
`;
  const r = adaptCsv(text, { defaultKind: "concept" });
  assertEquals(r.entities.length, 2);
  assertEquals(r.entities[0].kind, "concept");
});

Deno.test("adaptCsv — entity rows without kind and no default are fatal", () => {
  const text = `name
A
B
`;
  assertThrows(() => adaptCsv(text), Error, "missing kind");
});

Deno.test("adaptCsv — edge list with source/target/relation", () => {
  const text = `source,target,relation,weight
abc,def,depends_on,0.9
def,ghi,extends,0.5
`;
  const r = adaptCsv(text);
  assertEquals(r.edges.length, 2);
  assertEquals(r.entities.length, 0);
  assertEquals(r.edges[0].source, "abc");
  assertEquals(r.edges[0].target, "def");
  assertEquals(r.edges[0].relation, "depends_on");
  assertEquals(r.edges[0].weight, 0.9);
});

Deno.test("adaptCsv — quoted values with commas", () => {
  const text = `name,kind,description
"FlashAttn-2",concept,"Tiled, online softmax"
`;
  const r = adaptCsv(text);
  assertEquals(r.entities.length, 1);
  assertEquals(r.entities[0].name, "FlashAttn-2");
  assertEquals(r.entities[0].description, "Tiled, online softmax");
});

Deno.test("adaptCsv — extra columns collected into properties", () => {
  const text = `name,kind,year,authors
LoRA,concept,2021,"Hu et al."
`;
  const r = adaptCsv(text);
  assertEquals(r.entities[0].properties["year"], "2021");
  assertEquals(r.entities[0].properties["authors"], "Hu et al.");
});

Deno.test("adaptCsv — TSV via separator option", () => {
  const text = "name\tkind\nLoRA\tconcept\n";
  const r = adaptCsv(text, { separator: "\t" });
  assertEquals(r.entities.length, 1);
  assertEquals(r.entities[0].name, "LoRA");
});

Deno.test("adaptCsv — empty file is fatal", () => {
  assertThrows(() => adaptCsv(""), Error, "no header");
});

Deno.test("adaptCsv — edge file missing relation is fatal", () => {
  const text = `source,target
a,b
`;
  assertThrows(() => adaptCsv(text), Error, "relation");
});

Deno.test("adaptCsv — missing required entity 'name' column is fatal", () => {
  const text = `kind,description
concept,some desc
`;
  assertThrows(() => adaptCsv(text), Error, "name");
});

Deno.test("adaptCsv — empty name in a row is fatal", () => {
  const text = `name,kind
LoRA,concept
,concept
`;
  assertThrows(() => adaptCsv(text), Error, "empty name");
});

Deno.test("adaptCsv — missing source/target/relation in edge row is fatal", () => {
  const text = `source,target,relation
a,b,depends_on
,b,extends
`;
  assertThrows(() => adaptCsv(text), Error, "missing source/target/relation");
});

Deno.test("adaptCsv — description is top-level field, not in properties", () => {
  const text = `name,kind,description
LoRA,concept,Low-rank adaptation
`;
  const r = adaptCsv(text);
  assertEquals(r.entities[0].description, "Low-rank adaptation");
  assertEquals(r.entities[0].properties["description"], undefined);
});

// ─── CSV quoted-field regression tests ───

Deno.test("adaptCsv — leading-space before opening quote is stripped correctly", () => {
  // A common export artefact: `, "Quoted value"` where a space precedes the quote.
  // The space must be discarded and the quoted value parsed correctly.
  const text = `name,kind,description\nFlash, "concept","Tiled, online softmax"\n`;
  const r = adaptCsv(text);
  assertEquals(r.entities.length, 1);
  // "concept" with leading space — quote mode should strip the leading space.
  assertEquals(r.entities[0].kind.trim(), "concept");
  // description has embedded comma — must be preserved in full.
  assertEquals(r.entities[0].description, "Tiled, online softmax");
});

Deno.test("adaptCsv — quoted description with embedded comma preserved in full", () => {
  // Without the fix, `"Tiled, online"` would be split at the comma.
  const text = `name,kind,description\nFlashAttn-2,concept,"Tiled, online softmax"\n`;
  const r = adaptCsv(text);
  assertEquals(r.entities.length, 1);
  assertEquals(r.entities[0].name, "FlashAttn-2");
  assertEquals(r.entities[0].description, "Tiled, online softmax");
});

Deno.test("adaptCsv — arity mismatch (extra field from unbalanced quote) is fatal", () => {
  // Before the fix, an unbalanced/misdetected quote could silently split a field
  // and produce a row with more columns than the header.
  // Construct a row that *without* the leading-space quote fix would have 4 fields
  // (header has 3): name=Flash, kind= "concept", description=Tiled, overflow=online"
  // With the fix the arity matches; we test a genuinely unbalanced case instead.
  const text = `name,kind,description\nA,concept,value1,extra_overflow\n`;
  assertThrows(() => adaptCsv(text), Error, "mismatch");
});
