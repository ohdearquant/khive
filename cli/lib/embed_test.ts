/**
 * Tests for cli/lib/embed.ts — embedding planner (ADR-057).
 */

import { assertEquals, assertExists } from "@std/assert";
import { join } from "@std/path";
import { planEmbed } from "./embed.ts";
import { ALLOWED_FIELDS, validateEmbedFields } from "./embed.ts";
import type { EmbedConfig } from "./config.ts";

const DEFAULT_EMBED: EmbedConfig = {
  model: "mE5-small",
  dimensions: 384,
  auto_embed: true,
  batch_size: 64,
  device: "cpu",
  fields: { include: ["name", "description"] },
};

async function withTempRepo(
  fn: (root: string) => Promise<void>,
): Promise<void> {
  const root = await Deno.makeTempDir({ prefix: "khive-embed-" });
  await Deno.mkdir(join(root, ".khive/kg"), { recursive: true });
  try {
    await fn(root);
  } finally {
    await Deno.remove(root, { recursive: true });
  }
}

Deno.test("planEmbed — empty repo returns empty plan", async () => {
  await withTempRepo(async (root) => {
    const plan = await planEmbed(root, DEFAULT_EMBED);
    assertEquals(plan.total, 0);
    assertEquals(plan.pending.length, 0);
    assertEquals(plan.model, "mE5-small");
    assertEquals(plan.dimensions, 384);
  });
});

Deno.test("planEmbed — missing entities.ndjson returns empty plan", async () => {
  await withTempRepo(async (root) => {
    const plan = await planEmbed(root, DEFAULT_EMBED);
    assertEquals(plan.total, 0);
    assertEquals(plan.pending.length, 0);
  });
});

Deno.test("planEmbed — includes name and description fields", async () => {
  await withTempRepo(async (root) => {
    const entities = [
      JSON.stringify({
        id: "10000000-0000-0000-0000-000000000001",
        name: "Sinkhorn Distances",
        kind: "concept",
        properties: { description: "OT divergence with entropy regularization" },
      }),
      JSON.stringify({
        id: "20000000-0000-0000-0000-000000000002",
        name: "LoRA",
        kind: "concept",
        properties: { description: "Low-rank adapter for finetuning" },
      }),
    ];
    await Deno.writeTextFile(
      join(root, ".khive/kg/entities.ndjson"),
      entities.join("\n") + "\n",
    );

    const plan = await planEmbed(root, DEFAULT_EMBED);
    assertEquals(plan.total, 2);
    assertEquals(plan.pending.length, 2);
    assertExists(plan.pending.find((e) => e.id.startsWith("10000000")));
    assertEquals(plan.pending[0].kind, "concept");
    assertEquals(
      plan.pending[0].text,
      "Sinkhorn Distances OT divergence with entropy regularization",
    );
  });
});

Deno.test("planEmbed — entity with no embeddable text is excluded", async () => {
  await withTempRepo(async (root) => {
    const entities = [
      // No description, no other fields beyond required.
      JSON.stringify({
        id: "30000000-0000-0000-0000-000000000003",
        name: "OnlyName",
        kind: "concept",
      }),
      // Empty string description.
      JSON.stringify({
        id: "40000000-0000-0000-0000-000000000004",
        name: "WithEmpty",
        kind: "concept",
        properties: { description: "" },
      }),
    ];
    await Deno.writeTextFile(
      join(root, ".khive/kg/entities.ndjson"),
      entities.join("\n") + "\n",
    );

    const plan = await planEmbed(root, DEFAULT_EMBED);
    // Both have a name, so they should each produce a non-empty text.
    assertEquals(plan.pending.length, 2);
    assertEquals(plan.pending[0].text, "OnlyName");
    assertEquals(plan.pending[1].text, "WithEmpty");
  });
});

Deno.test("planEmbed — custom field list", async () => {
  await withTempRepo(async (root) => {
    const entities = [
      JSON.stringify({
        id: "50000000-0000-0000-0000-000000000005",
        name: "Bert",
        kind: "concept",
        properties: { title: "Bidirectional Encoder", year: 2018 },
      }),
    ];
    await Deno.writeTextFile(
      join(root, ".khive/kg/entities.ndjson"),
      entities.join("\n") + "\n",
    );

    const plan = await planEmbed(root, {
      ...DEFAULT_EMBED,
      fields: { include: ["name", "title", "year"] },
    });
    assertEquals(plan.pending.length, 1);
    assertEquals(plan.pending[0].text, "Bert Bidirectional Encoder 2018");
  });
});

// ─── validateEmbedFields tests (ADR-057 §8) ──────────────────────────────────

Deno.test("validateEmbedFields — accepts top-level fields and property keys", () => {
  assertEquals(validateEmbedFields(["name"]), null);
  assertEquals(validateEmbedFields(["name", "description"]), null);
  // Property keys (entity.properties keys) are accepted at config time.
  assertEquals(validateEmbedFields(["name", "abstract"]), null);
  assertEquals(validateEmbedFields(["name", "description", "title"]), null);
  assertEquals(validateEmbedFields(["name", "description", "summary"]), null);
});

Deno.test("validateEmbedFields — rejects empty array", () => {
  const err = validateEmbedFields([]);
  assertEquals(typeof err, "string");
  assertEquals((err ?? "").includes("non-empty"), true);
});

Deno.test("validateEmbedFields — rejects reserved field 'kind'", () => {
  const err = validateEmbedFields(["name", "kind"]);
  assertEquals(typeof err, "string");
  assertEquals((err ?? "").includes("reserved"), true);
});

Deno.test("validateEmbedFields — accepts a property key that looks like a typo (open validation)", () => {
  // "descripton" (typo) is now accepted as a property key at config time.
  // The runtime will simply return empty string for missing property keys.
  assertEquals(validateEmbedFields(["name", "descripton"]), null);
});

Deno.test("validateEmbedFields — rejects duplicate fields", () => {
  const err = validateEmbedFields(["name", "name"]);
  assertEquals(typeof err, "string");
  assertEquals((err ?? "").includes("duplicate"), true);
});

Deno.test("validateEmbedFields — ALLOWED_FIELDS contains exactly name and description", () => {
  assertEquals(ALLOWED_FIELDS.includes("name"), true);
  assertEquals(ALLOWED_FIELDS.includes("description"), true);
  // "kind" is reserved — not in the allowlist (ADR-001 closed taxonomy).
  assertEquals(ALLOWED_FIELDS.includes("kind"), false);
});

Deno.test("planEmbed — invalid entity lines are silently skipped", async () => {
  await withTempRepo(async (root) => {
    const lines = [
      JSON.stringify({
        id: "60000000-0000-0000-0000-000000000006",
        name: "Valid",
        kind: "concept",
      }),
      "{ this is not json",
      JSON.stringify({ id: "not-a-uuid", name: "BadId", kind: "concept" }),
      "",
      "# comment line",
    ];
    await Deno.writeTextFile(
      join(root, ".khive/kg/entities.ndjson"),
      lines.join("\n") + "\n",
    );

    const plan = await planEmbed(root, DEFAULT_EMBED);
    // Only the one valid entity counts.
    assertEquals(plan.pending.length, 1);
    assertEquals(plan.pending[0].id.startsWith("60000000"), true);
  });
});

Deno.test("planEmbed — preserves model, dimensions, batch_size, fields", async () => {
  await withTempRepo(async (root) => {
    const plan = await planEmbed(root, {
      model: "bge-large-en",
      dimensions: 1024,
      auto_embed: true,
      batch_size: 32,
      device: "cuda",
      fields: { include: ["name"] },
    });
    assertEquals(plan.model, "bge-large-en");
    assertEquals(plan.dimensions, 1024);
    assertEquals(plan.batchSize, 32);
    assertEquals(plan.fields, ["name"]);
  });
});
