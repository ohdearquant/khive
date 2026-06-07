/**
 * Tests for cli/kg/embed.ts — embed command (ADR-057).
 */

import { assertStringIncludes } from "@std/assert";
import { join } from "@std/path";
import { runEmbed } from "./embed.ts";

const SAMPLE_ENTITIES = [
  JSON.stringify({
    id: "10000000-0000-0000-0000-000000000001",
    name: "Sinkhorn",
    kind: "concept",
    properties: { description: "OT divergence" },
  }),
  JSON.stringify({
    id: "20000000-0000-0000-0000-000000000002",
    name: "LoRA",
    kind: "concept",
    properties: { description: "Low-rank adapter" },
  }),
].join("\n") + "\n";

async function withTempRepo(
  fn: (root: string) => Promise<void>,
): Promise<void> {
  const root = await Deno.makeTempDir({ prefix: "khive-embed-cmd-" });
  await Deno.mkdir(join(root, ".khive/kg"), { recursive: true });
  await Deno.writeTextFile(join(root, ".khive/kg/entities.ndjson"), SAMPLE_ENTITIES);
  try {
    await fn(root);
  } finally {
    await Deno.remove(root, { recursive: true });
  }
}

function captureStdout(): { restore: () => string } {
  const original = console.log;
  const chunks: string[] = [];
  console.log = (...args: unknown[]) => {
    chunks.push(args.map(String).join(" "));
  };
  return {
    restore: () => {
      console.log = original;
      return chunks.join("\n");
    },
  };
}

Deno.test("embed (default) — prints embed plan", async () => {
  await withTempRepo(async (root) => {
    const cap = captureStdout();
    await runEmbed(root, []);
    const out = cap.restore();
    assertStringIncludes(out, "Embed dry-run plan: 2/2 entities pending");
    assertStringIncludes(out, "model=mE5-small");
  });
});

Deno.test("embed --dry-run — silences trailing notice", async () => {
  await withTempRepo(async (root) => {
    const cap = captureStdout();
    await runEmbed(root, ["--dry-run"]);
    const out = cap.restore();
    assertStringIncludes(out, "Embed dry-run plan");
    // Notice about Phase C1 should NOT appear when --dry-run is set.
    if (out.includes("Dry-run only: embedding runtime is not wired")) {
      throw new Error("--dry-run should silence the Phase C1 notice");
    }
  });
});

Deno.test("embed --ids — filters to specified short or full IDs", async () => {
  await withTempRepo(async (root) => {
    const cap = captureStdout();
    await runEmbed(root, ["--ids", "10000000"]);
    const out = cap.restore();
    assertStringIncludes(out, "Embed dry-run plan: 1/2 entities pending");
  });
});

Deno.test("embed --json — emits machine-readable JSON", async () => {
  await withTempRepo(async (root) => {
    const cap = captureStdout();
    await runEmbed(root, ["--json"]);
    const out = cap.restore().trim();
    const parsed = JSON.parse(out);
    if (parsed.model !== "mE5-small") {
      throw new Error(`expected model=mE5-small, got ${parsed.model}`);
    }
    if (parsed.pending !== 2) {
      throw new Error(`expected pending=2, got ${parsed.pending}`);
    }
    if (parsed.ids.length !== 2) {
      throw new Error(`expected 2 ids, got ${parsed.ids.length}`);
    }
  });
});

Deno.test("embed empty repo — prints empty plan", async () => {
  const root = await Deno.makeTempDir({ prefix: "khive-embed-empty-" });
  await Deno.mkdir(join(root, ".khive/kg"), { recursive: true });
  await Deno.writeTextFile(join(root, ".khive/kg/entities.ndjson"), "");
  try {
    const cap = captureStdout();
    await runEmbed(root, []);
    const out = cap.restore();
    assertStringIncludes(out, "Embeddings are up-to-date, nothing to do.");
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});
