/**
 * Tests for cli/kg/migrate.ts — `khive kg migrate` (ADR-054).
 *
 * Regression suite added to prevent the defects identified during review:
 *  - reg-endpoint-kind-filtering: remove_relation_endpoint must filter by
 *    (relation, source_kind, target_kind), not relation alone.
 *  - reg-atomicity: schema write failure must leave all files at original version.
 *  - reg-init-ontology-version: init schema must carry ontology_version.
 *  - reg-property-schema-mutation: add/remove/rename_property must mutate
 *    entity_properties in schema.yaml.
 *  - reg-endpoint-schema-mutation: add/remove_relation_endpoint must mutate
 *    endpoint_rules in schema.yaml.
 *  - reg-taxonomy-validation: add_kind rejects invalid names; add/remove
 *    relation_endpoint rejects non-ADR-002 relations.
 */

import { assertEquals, assertStringIncludes } from "@std/assert";
import { join } from "@std/path";
import { applyMigrations, runMigrate } from "./migrate.ts";

const SCHEMA = `format_version: "1.0.0"
ontology_version: "1.0.0"
entity_kinds:
  - concept
  - document
edge_relations:
  - relation: depends_on
    category: dependency
  - relation: extends
    category: derivation
`;

async function withTempRepo(
  entitiesContent: string,
  edgesContent: string,
  fn: (root: string) => Promise<void>,
): Promise<void> {
  const root = await Deno.makeTempDir({ prefix: "khive-migrate-" });
  await Deno.mkdir(join(root, ".khive/kg/migrations"), { recursive: true });
  await Deno.writeTextFile(join(root, ".khive/kg/schema.yaml"), SCHEMA);
  await Deno.writeTextFile(join(root, ".khive/kg/entities.ndjson"), entitiesContent);
  await Deno.writeTextFile(join(root, ".khive/kg/edges.ndjson"), edgesContent);
  try {
    await fn(root);
  } finally {
    await Deno.remove(root, { recursive: true });
  }
}

async function writeMigration(
  root: string,
  seq: number,
  content: string,
): Promise<void> {
  const name = `${seq.toString().padStart(4, "0")}_test.yaml`;
  await Deno.writeTextFile(join(root, ".khive/kg/migrations", name), content);
}

function captureStdout(): { restore: () => string } {
  const origLog = console.log;
  const origErr = console.error;
  const chunks: string[] = [];
  const push = (...args: unknown[]) => chunks.push(args.map(String).join(" "));
  console.log = push;
  console.error = push;
  return {
    restore: () => {
      console.log = origLog;
      console.error = origErr;
      return chunks.join("\n");
    },
  };
}

// ─── add_kind ────────────────────────────────────────────────────────────────

Deno.test("migrate add_kind — adds to schema, no entity rewrite", async () => {
  await withTempRepo("", "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "Add dataset kind"
operations:
  - add_kind:
      name: dataset
`,
    );
    const res = await applyMigrations(root);
    assertEquals(res.applied.length, 1);
    assertEquals(res.finalVersion, "1.1.0");
    assertEquals(res.counts.entitiesRewritten, 0);

    const schemaText = await Deno.readTextFile(join(root, ".khive/kg/schema.yaml"));
    assertStringIncludes(schemaText, "dataset");
    assertStringIncludes(schemaText, "1.1.0");
  });
});

// ─── reg-taxonomy-validation: add_kind rejects invalid names ─────────────────

Deno.test("reg-taxonomy-validation: add_kind rejects empty name", async () => {
  await withTempRepo("", "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "bad kind"
operations:
  - add_kind:
      name: ""
`,
    );
    let threw = false;
    try {
      await applyMigrations(root);
    } catch (err) {
      threw = true;
      assertStringIncludes((err as Error).message, "invalid kind name");
    }
    if (!threw) throw new Error("add_kind with empty name should throw");
  });
});

Deno.test("reg-taxonomy-validation: add_kind rejects uppercase name", async () => {
  await withTempRepo("", "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "bad kind"
operations:
  - add_kind:
      name: MyKind
`,
    );
    let threw = false;
    try {
      await applyMigrations(root);
    } catch (err) {
      threw = true;
      assertStringIncludes((err as Error).message, "invalid kind name");
    }
    if (!threw) throw new Error("add_kind with uppercase name should throw");
  });
});

// ─── reg-taxonomy-validation: relation endpoint validation ───────────────────

Deno.test("reg-taxonomy-validation: add_relation_endpoint rejects non-canonical relation", async () => {
  await withTempRepo("", "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "bad relation"
operations:
  - add_relation_endpoint:
      relation: uses
      source_kind: concept
      target_kind: project
`,
    );
    let threw = false;
    try {
      await applyMigrations(root);
    } catch (err) {
      threw = true;
      assertStringIncludes((err as Error).message, "unknown relation 'uses'");
    }
    if (!threw) throw new Error("add_relation_endpoint with non-canonical relation should throw");
  });
});

Deno.test("reg-taxonomy-validation: remove_relation_endpoint rejects non-canonical relation", async () => {
  await withTempRepo("", "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "bad relation"
operations:
  - remove_relation_endpoint:
      relation: related_to
      source_kind: concept
      target_kind: document
      on_existing: error
`,
    );
    let threw = false;
    try {
      await applyMigrations(root);
    } catch (err) {
      threw = true;
      assertStringIncludes((err as Error).message, "unknown relation 'related_to'");
    }
    if (!threw) {
      throw new Error("remove_relation_endpoint with non-canonical relation should throw");
    }
  });
});

// ─── rename_kind ─────────────────────────────────────────────────────────────

Deno.test("migrate rename_kind — rewrites entity kind fields", async () => {
  const entities = [
    JSON.stringify({
      id: "10000000-0000-0000-0000-000000000001",
      name: "X",
      kind: "concept",
    }),
    JSON.stringify({
      id: "20000000-0000-0000-0000-000000000002",
      name: "Y",
      kind: "document",
    }),
  ].join("\n") + "\n";

  await withTempRepo(entities, "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "rename concept -> project"
operations:
  - rename_kind:
      from: concept
      to: project
`,
    );
    const res = await applyMigrations(root);
    assertEquals(res.applied.length, 1);
    assertEquals(res.counts.entitiesRewritten, 1);

    const after = await Deno.readTextFile(join(root, ".khive/kg/entities.ndjson"));
    assertStringIncludes(after, '"kind":"project"');
    if (after.includes('"kind":"concept"')) {
      throw new Error("rename_kind should have rewritten all concept→project");
    }
  });
});

// ─── remove_kind: error ─────────────────────────────────────────────────────

Deno.test("migrate remove_kind on_existing=error — aborts when entities exist", async () => {
  const entities = JSON.stringify({
    id: "30000000-0000-0000-0000-000000000003",
    name: "X",
    kind: "concept",
  }) + "\n";

  await withTempRepo(entities, "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "kill concept"
operations:
  - remove_kind:
      name: concept
      on_existing: error
`,
    );
    let threw = false;
    try {
      await applyMigrations(root);
    } catch (err) {
      threw = true;
      assertStringIncludes((err as Error).message, "remove_kind 'concept'");
    }
    if (!threw) throw new Error("remove_kind with existing entities should throw");
  });
});

// ─── remove_kind: migrate_to ────────────────────────────────────────────────

Deno.test("migrate remove_kind on_existing=migrate_to — rewrites to target", async () => {
  const entities = JSON.stringify({
    id: "40000000-0000-0000-0000-000000000004",
    name: "X",
    kind: "concept",
  }) + "\n";

  await withTempRepo(entities, "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "migrate concept -> document"
operations:
  - remove_kind:
      name: concept
      on_existing: migrate_to
      target: document
`,
    );
    const res = await applyMigrations(root);
    assertEquals(res.applied.length, 1);
    assertEquals(res.counts.entitiesRewritten, 1);
    const after = await Deno.readTextFile(join(root, ".khive/kg/entities.ndjson"));
    assertStringIncludes(after, '"kind":"document"');
  });
});

// ─── add_property required=true ─────────────────────────────────────────────

Deno.test("migrate add_property required=true — aborts when any entity lacks it", async () => {
  const entities = JSON.stringify({
    id: "50000000-0000-0000-0000-000000000005",
    name: "X",
    kind: "concept",
    properties: {},
  }) + "\n";

  await withTempRepo(entities, "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "require paper_url"
operations:
  - add_property:
      kind: concept
      name: paper_url
      type: string
      required: true
`,
    );
    let threw = false;
    try {
      await applyMigrations(root);
    } catch (err) {
      threw = true;
      assertStringIncludes((err as Error).message, "add_property");
      assertStringIncludes((err as Error).message, "paper_url");
    }
    if (!threw) throw new Error("required add_property without backfill should throw");
  });
});

Deno.test("migrate add_property required=false — does not abort", async () => {
  const entities = JSON.stringify({
    id: "60000000-0000-0000-0000-000000000006",
    name: "X",
    kind: "concept",
    properties: {},
  }) + "\n";

  await withTempRepo(entities, "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "optional paper_url"
operations:
  - add_property:
      kind: concept
      name: paper_url
      type: string
      required: false
`,
    );
    const res = await applyMigrations(root);
    assertEquals(res.applied.length, 1);
  });
});

// ─── reg-property-schema-mutation: add_property writes to entity_properties ──

Deno.test("reg-property-schema-mutation: add_property records in schema entity_properties", async () => {
  await withTempRepo("", "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "add optional paper_url"
operations:
  - add_property:
      kind: concept
      name: paper_url
      type: string
      required: false
      description: "Paper URL"
`,
    );
    await applyMigrations(root);
    const schemaText = await Deno.readTextFile(join(root, ".khive/kg/schema.yaml"));
    assertStringIncludes(schemaText, "entity_properties");
    assertStringIncludes(schemaText, "paper_url");
    assertStringIncludes(schemaText, "concept");
  });
});

Deno.test("reg-property-schema-mutation: remove_property removes from schema entity_properties", async () => {
  // Schema pre-seeded with a property entry.
  const schemaWithProps = `format_version: "1.0.0"
ontology_version: "1.0.0"
entity_kinds:
  - concept
  - document
edge_relations:
  - relation: depends_on
    category: dependency
entity_properties:
  concept:
    - name: paper_url
      type: string
      required: false
`;
  const root = await Deno.makeTempDir({ prefix: "khive-migrate-" });
  await Deno.mkdir(join(root, ".khive/kg/migrations"), { recursive: true });
  await Deno.writeTextFile(join(root, ".khive/kg/schema.yaml"), schemaWithProps);
  await Deno.writeTextFile(join(root, ".khive/kg/entities.ndjson"), "");
  await Deno.writeTextFile(join(root, ".khive/kg/edges.ndjson"), "");
  try {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "remove paper_url"
operations:
  - remove_property:
      kind: concept
      name: paper_url
`,
    );
    await applyMigrations(root);
    const schemaText = await Deno.readTextFile(join(root, ".khive/kg/schema.yaml"));
    if (schemaText.includes("paper_url")) {
      throw new Error("remove_property should have removed paper_url from schema");
    }
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("reg-property-schema-mutation: rename_property updates schema entity_properties", async () => {
  const schemaWithProps = `format_version: "1.0.0"
ontology_version: "1.0.0"
entity_kinds:
  - concept
entity_properties:
  concept:
    - name: old_name
      type: string
`;
  const entities = JSON.stringify({
    id: "aa000000-0000-0000-0000-000000000001",
    name: "E",
    kind: "concept",
    properties: { old_name: "Sinkhorn" },
  }) + "\n";

  const root = await Deno.makeTempDir({ prefix: "khive-migrate-" });
  await Deno.mkdir(join(root, ".khive/kg/migrations"), { recursive: true });
  await Deno.writeTextFile(join(root, ".khive/kg/schema.yaml"), schemaWithProps);
  await Deno.writeTextFile(join(root, ".khive/kg/entities.ndjson"), entities);
  await Deno.writeTextFile(join(root, ".khive/kg/edges.ndjson"), "");
  try {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "rename old_name -> title"
operations:
  - rename_property:
      kind: concept
      from: old_name
      to: title
`,
    );
    await applyMigrations(root);
    const schemaText = await Deno.readTextFile(join(root, ".khive/kg/schema.yaml"));
    assertStringIncludes(schemaText, "title");
    if (schemaText.includes("old_name")) {
      throw new Error("rename_property should have updated schema entity_properties");
    }
    const entText = await Deno.readTextFile(join(root, ".khive/kg/entities.ndjson"));
    assertStringIncludes(entText, '"title":"Sinkhorn"');
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});

// ─── rename_property (original test) ────────────────────────────────────────

Deno.test("migrate rename_property — rewrites property key", async () => {
  const entities = JSON.stringify({
    id: "70000000-0000-0000-0000-000000000007",
    name: "X",
    kind: "concept",
    properties: { old_name: "Sinkhorn", year: 2013 },
  }) + "\n";

  await withTempRepo(entities, "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "rename property"
operations:
  - rename_property:
      kind: concept
      from: old_name
      to: title
`,
    );
    const res = await applyMigrations(root);
    assertEquals(res.applied.length, 1);
    assertEquals(res.counts.entitiesRewritten, 1);
    const after = await Deno.readTextFile(join(root, ".khive/kg/entities.ndjson"));
    assertStringIncludes(after, '"title":"Sinkhorn"');
    if (after.includes('"old_name":')) {
      throw new Error("rename_property should remove old_name");
    }
  });
});

// ─── reg-endpoint-schema-mutation: add_relation_endpoint records rule ────────

Deno.test("reg-endpoint-schema-mutation: add_relation_endpoint records in schema endpoint_rules", async () => {
  await withTempRepo("", "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "add project->concept depends_on endpoint"
operations:
  - add_relation_endpoint:
      relation: depends_on
      source_kind: project
      target_kind: concept
`,
    );
    await applyMigrations(root);
    const schemaText = await Deno.readTextFile(join(root, ".khive/kg/schema.yaml"));
    assertStringIncludes(schemaText, "endpoint_rules");
    assertStringIncludes(schemaText, "depends_on");
    assertStringIncludes(schemaText, "project");
    assertStringIncludes(schemaText, "concept");
  });
});

Deno.test("reg-endpoint-schema-mutation: remove_relation_endpoint removes rule from schema", async () => {
  const schemaWithRules = `format_version: "1.0.0"
ontology_version: "1.0.0"
entity_kinds:
  - concept
  - project
edge_relations:
  - relation: depends_on
    category: dependency
endpoint_rules:
  depends_on:
    - source_kind: project
      target_kind: concept
`;
  const root = await Deno.makeTempDir({ prefix: "khive-migrate-" });
  await Deno.mkdir(join(root, ".khive/kg/migrations"), { recursive: true });
  await Deno.writeTextFile(join(root, ".khive/kg/schema.yaml"), schemaWithRules);
  await Deno.writeTextFile(join(root, ".khive/kg/entities.ndjson"), "");
  await Deno.writeTextFile(join(root, ".khive/kg/edges.ndjson"), "");
  try {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "remove project->concept endpoint"
operations:
  - remove_relation_endpoint:
      relation: depends_on
      source_kind: project
      target_kind: concept
      on_existing: error
`,
    );
    await applyMigrations(root);
    const schemaText = await Deno.readTextFile(join(root, ".khive/kg/schema.yaml"));
    // The only endpoint rule was project->concept depends_on. After removal the
    // endpoint_rules key should be absent from the schema entirely.
    // Note: "project" may still appear in entity_kinds — that is expected.
    if (schemaText.includes("endpoint_rules")) {
      throw new Error("remove_relation_endpoint should have removed endpoint_rules from schema");
    }
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});

// ─── reg-endpoint-kind-filtering: remove_relation_endpoint filters by kind ───

Deno.test(
  "reg-endpoint-kind-filtering: remove_relation_endpoint drops only matching (relation,source_kind,target_kind)",
  async () => {
    // Two depends_on edges: one concept→document (remove), one project→document (keep).
    const conceptId = "c1000000-0000-0000-0000-000000000001";
    const projectId = "c2000000-0000-0000-0000-000000000002";
    const docId = "d1000000-0000-0000-0000-000000000003";

    const entities = [
      JSON.stringify({ id: conceptId, name: "C", kind: "concept" }),
      JSON.stringify({ id: projectId, name: "P", kind: "project" }),
      JSON.stringify({ id: docId, name: "D", kind: "document" }),
    ].join("\n") + "\n";

    const edges = [
      JSON.stringify({
        edge_id: "e1000000-0000-0000-0000-000000000001",
        source: conceptId,
        target: docId,
        relation: "depends_on",
      }),
      JSON.stringify({
        edge_id: "e2000000-0000-0000-0000-000000000002",
        source: projectId,
        target: docId,
        relation: "depends_on",
      }),
    ].join("\n") + "\n";

    await withTempRepo(entities, edges, async (root) => {
      await writeMigration(
        root,
        1,
        `version_from: "1.0.0"
version_to: "1.1.0"
description: "remove concept->document depends_on"
operations:
  - remove_relation_endpoint:
      relation: depends_on
      source_kind: concept
      target_kind: document
      on_existing: drop
`,
      );
      const res = await applyMigrations(root);
      assertEquals(res.applied.length, 1);
      assertEquals(res.counts.edgesDropped, 1);

      const after = await Deno.readTextFile(join(root, ".khive/kg/edges.ndjson"));
      // The project→document edge must survive.
      assertStringIncludes(after, projectId);
      // The concept→document edge must be gone.
      if (after.includes(conceptId)) {
        throw new Error("concept->document depends_on edge should have been dropped");
      }
    });
  },
);

Deno.test(
  "reg-endpoint-kind-filtering: remove_relation_endpoint error fires only on matching endpoint pair",
  async () => {
    // One depends_on edge: project→document. Migration removes concept→document.
    // Should NOT abort (no matching edges).
    const projectId = "b1000000-0000-0000-0000-000000000001";
    const docId = "b2000000-0000-0000-0000-000000000002";

    const entities = [
      JSON.stringify({ id: projectId, name: "P", kind: "project" }),
      JSON.stringify({ id: docId, name: "D", kind: "document" }),
    ].join("\n") + "\n";

    const edges = JSON.stringify({
      edge_id: "be000000-0000-0000-0000-000000000001",
      source: projectId,
      target: docId,
      relation: "depends_on",
    }) + "\n";

    await withTempRepo(entities, edges, async (root) => {
      await writeMigration(
        root,
        1,
        `version_from: "1.0.0"
version_to: "1.1.0"
description: "remove concept->document depends_on (no such edges)"
operations:
  - remove_relation_endpoint:
      relation: depends_on
      source_kind: concept
      target_kind: document
      on_existing: error
`,
      );
      // Should succeed: no concept→document edges exist, so no abort.
      const res = await applyMigrations(root);
      assertEquals(res.applied.length, 1);
      assertEquals(res.counts.edgesDropped, 0);
      // The project→document edge must be untouched.
      const after = await Deno.readTextFile(join(root, ".khive/kg/edges.ndjson"));
      assertStringIncludes(after, projectId);
    });
  },
);

// ─── reg-atomicity: write failure leaves files at original version ────────────

Deno.test(
  "reg-atomicity: failed operation leaves schema.yaml at original ontology_version",
  async () => {
    // A migration whose operation throws (required property missing) must
    // leave the original schema.yaml unchanged.
    const entities = JSON.stringify({
      id: "fa000000-0000-0000-0000-000000000001",
      name: "X",
      kind: "concept",
      properties: {},
    }) + "\n";

    await withTempRepo(entities, "", async (root) => {
      await writeMigration(
        root,
        1,
        `version_from: "1.0.0"
version_to: "2.0.0"
description: "fail because required property missing"
operations:
  - add_property:
      kind: concept
      name: must_have
      type: string
      required: true
`,
      );
      const before = await Deno.readTextFile(join(root, ".khive/kg/schema.yaml"));
      let threw = false;
      try {
        await applyMigrations(root);
      } catch {
        threw = true;
      }
      if (!threw) throw new Error("Migration should have thrown");
      // File must be unchanged because the operation failed before any write.
      const after = await Deno.readTextFile(join(root, ".khive/kg/schema.yaml"));
      assertEquals(before, after, "schema.yaml must not be modified on migration failure");
    });
  },
);

Deno.test(
  "reg-atomicity: dry-run never writes files even when migrations would succeed",
  async () => {
    await withTempRepo("", "", async (root) => {
      await writeMigration(
        root,
        1,
        `version_from: "1.0.0"
version_to: "1.1.0"
description: "no-op"
operations:
  - add_kind:
      name: dataset
`,
      );
      const before = await Deno.readTextFile(join(root, ".khive/kg/schema.yaml"));
      const res = await applyMigrations(root, { dryRun: true });
      assertEquals(res.applied.length, 1);
      assertEquals(res.finalVersion, "1.1.0");
      const after = await Deno.readTextFile(join(root, ".khive/kg/schema.yaml"));
      assertEquals(before, after);
    });
  },
);

// ─── remove_relation_endpoint: drop (original regression) ───────────────────

Deno.test("migrate remove_relation_endpoint drop — drops matching edges", async () => {
  const entities = "";
  const edges = [
    JSON.stringify({
      edge_id: "80000000-0000-0000-0000-000000000008",
      source: "a",
      target: "b",
      relation: "depends_on",
    }),
    JSON.stringify({
      edge_id: "90000000-0000-0000-0000-000000000009",
      source: "c",
      target: "d",
      relation: "extends",
    }),
  ].join("\n") + "\n";

  await withTempRepo(entities, edges, async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "drop depends_on (any endpoints)"
operations:
  - remove_relation_endpoint:
      relation: depends_on
      source_kind: any
      target_kind: any
      on_existing: drop
`,
    );
    const res = await applyMigrations(root);
    assertEquals(res.applied.length, 1);
    assertEquals(res.counts.edgesDropped, 1);

    const after = await Deno.readTextFile(join(root, ".khive/kg/edges.ndjson"));
    if (after.includes("depends_on")) {
      throw new Error("drop should have removed depends_on edges");
    }
    assertStringIncludes(after, "extends");
  });
});

// ─── --to limit ─────────────────────────────────────────────────────────────

Deno.test("migrate --to <version> — stops at the requested version", async () => {
  await withTempRepo("", "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "first"
operations:
  - add_kind:
      name: dataset
`,
    );
    await writeMigration(
      root,
      2,
      `version_from: "1.1.0"
version_to: "1.2.0"
description: "second"
operations:
  - add_kind:
      name: person
`,
    );
    const res = await applyMigrations(root, { toVersion: "1.1.0" });
    assertEquals(res.applied.length, 1);
    assertEquals(res.finalVersion, "1.1.0");
  });
});

// ─── runMigrate CLI ─────────────────────────────────────────────────────────

Deno.test("runMigrate — list reports pending and applied", async () => {
  await withTempRepo("", "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "first"
operations:
  - add_kind:
      name: dataset
`,
    );
    const cap = captureStdout();
    await runMigrate(root, ["--list"]);
    const out = cap.restore();
    assertStringIncludes(out, "pending");
    assertStringIncludes(out, "1.0.0 → 1.1.0");
  });
});

Deno.test("runMigrate — apply prints summary", async () => {
  await withTempRepo("", "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "first"
operations:
  - add_kind:
      name: dataset
`,
    );
    const cap = captureStdout();
    const code = await runMigrate(root, []);
    const out = cap.restore();
    assertEquals(code, 0);
    assertStringIncludes(out, "Applied 1 migration");
    assertStringIncludes(out, "1.1.0");
  });
});

// ─── sequence gap ───────────────────────────────────────────────────────────

Deno.test("migrate — sequence gap is an error", async () => {
  await withTempRepo("", "", async (root) => {
    await writeMigration(
      root,
      1,
      `version_from: "1.0.0"
version_to: "1.1.0"
description: "first"
operations: []
`,
    );
    await writeMigration(
      root,
      3,
      `version_from: "1.1.0"
version_to: "1.2.0"
description: "gap"
operations: []
`,
    );
    let threw = false;
    try {
      await applyMigrations(root);
    } catch (err) {
      threw = true;
      assertStringIncludes((err as Error).message, "sequence gap");
    }
    if (!threw) throw new Error("sequence gap should have been detected");
  });
});
