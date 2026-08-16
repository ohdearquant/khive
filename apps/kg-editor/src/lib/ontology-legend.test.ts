import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

import {
  DERIVED_EDGE_MARK,
  EDGE_RELATION_FAMILIES,
  EDGE_RELATION_LEGEND,
  EDGE_RELATIONS,
  ENTITY_KIND_LEGEND,
  ENTITY_KINDS,
  NOTE_KIND_LEGEND,
  NOTE_KINDS,
} from "@/lib/ontology-legend";

const vocabSource = readFileSync(
  resolve(process.cwd(), "../../crates/khive-pack-kg/src/vocab.rs"),
  "utf8",
);
const edgeSource = readFileSync(
  resolve(process.cwd(), "../../crates/khive-types/src/edge.rs"),
  "utf8",
);

function rustStringConst(
  source: string,
  scope: string,
  name: string,
): string[] {
  const scopeStart = source.indexOf(`impl ${scope} {`);
  const declaration = source.indexOf(`const ${name}`, scopeStart);
  const arrayStart = source.indexOf("&[", declaration);
  const arrayEnd = source.indexOf("];", arrayStart);
  if (scopeStart < 0 || declaration < 0 || arrayStart < 0 || arrayEnd < 0) {
    throw new Error(`could not locate ${scope}::${name}`);
  }
  return [...source.slice(arrayStart, arrayEnd).matchAll(/"([a-z_]+)"/g)].map((
    match,
  ) => match[1]);
}

function rustEnumVariants(source: string, name: string): string[] {
  const body = source.match(
    new RegExp(`pub enum ${name} \\{([\\s\\S]*?)\\n\\}`),
  )?.[1];
  if (!body) throw new Error(`could not locate enum ${name}`);
  return [...body.matchAll(/^\s*([A-Z][A-Za-z0-9]+),$/gm)].map((match) =>
    match[1].toLowerCase()
  );
}

function rustRelationFamilies(source: string): Record<string, string> {
  const start = source.indexOf("pub const fn category(&self)");
  const end = source.indexOf("/// Canonical snake_case name", start);
  if (start < 0 || end < 0) {
    throw new Error("could not locate EdgeRelation::category");
  }
  const result: Record<string, string> = {};
  const arms = source.slice(start, end).matchAll(
    /(Self::[A-Za-z]+(?:\s*\|\s*Self::[A-Za-z]+)*)\s*=>\s*(?:\{\s*)?EdgeCategory::([A-Za-z]+)/g,
  );
  for (const arm of arms) {
    const family = arm[2].toLowerCase();
    for (const variant of arm[1].matchAll(/Self::([A-Za-z]+)/g)) {
      const relation = variant[1].replace(/([a-z0-9])([A-Z])/g, "$1_$2")
        .toLowerCase();
      result[relation] = family;
    }
  }
  return result;
}

describe("ontology legend", () => {
  it("exactly covers the base ontology", () => {
    const canonicalEntities = rustStringConst(
      vocabSource,
      "EntityKind",
      "NAMES",
    );
    const canonicalRelations = rustStringConst(
      edgeSource,
      "EdgeRelation",
      "VALID_NAMES",
    );
    const canonicalNotes = rustStringConst(vocabSource, "NoteKind", "NAMES");
    const canonicalFamilies = rustEnumVariants(edgeSource, "EdgeCategory");

    expect([...ENTITY_KINDS]).toEqual(canonicalEntities);
    expect([...EDGE_RELATIONS]).toEqual(canonicalRelations);
    expect([...NOTE_KINDS]).toEqual(canonicalNotes);
    expect(Object.keys(ENTITY_KIND_LEGEND)).toEqual(canonicalEntities);
    expect(Object.keys(EDGE_RELATION_LEGEND)).toEqual(canonicalRelations);
    expect(Object.keys(NOTE_KIND_LEGEND)).toEqual(canonicalNotes);
    expect(Object.keys(EDGE_RELATION_FAMILIES)).toEqual(canonicalFamilies);
  });

  it("assigns every relation to its authoritative family", () => {
    const canonicalRelationFamilies = rustRelationFamilies(edgeSource);
    const registered = Object.fromEntries(
      Object.entries(EDGE_RELATION_FAMILIES).flatMap(([family, relations]) =>
        relations.map((relation) => [relation, family])
      ),
    );
    const rendered = Object.fromEntries(
      Object.entries(EDGE_RELATION_LEGEND).map((
        [relation, entry],
      ) => [relation, entry.family]),
    );
    expect(registered).toEqual(canonicalRelationFamilies);
    expect(rendered).toEqual(canonicalRelationFamilies);
    for (const [family, relations] of Object.entries(EDGE_RELATION_FAMILIES)) {
      for (const relation of relations) {
        expect(EDGE_RELATION_LEGEND[relation].family).toBe(family);
      }
    }
  });

  it("keeps every distinction legible without hue", () => {
    expect(
      new Set(Object.values(ENTITY_KIND_LEGEND).map((entry) => entry.icon))
        .size,
    ).toBe(9);
    expect(
      new Set(Object.values(NOTE_KIND_LEGEND).map((entry) => entry.icon)).size,
    ).toBe(5);
    expect(
      new Set(Object.values(EDGE_RELATION_LEGEND).map((entry) => entry.glyph))
        .size,
    ).toBe(17);
    expect(DERIVED_EDGE_MARK).toMatchObject({
      geometry: "diamond",
      glyph: "◇",
    });
  });
});
