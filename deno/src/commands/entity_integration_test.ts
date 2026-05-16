// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Integration test for the entity command against a real khive-mcp binary.
 *
 * Spawns khive-mcp with an in-memory DB (--no-embed --db :memory:) and
 * exercises a full create→get→list→delete lifecycle via the Deno MCP client.
 *
 * This test ONLY runs if the binary exists; it is skipped otherwise so that
 * pure unit-test environments (no Rust build) are not broken.
 *
 * sanitizeResources/sanitizeOps are disabled because the MCP SDK transport
 * uses internal timers that it does not expose for cleanup.
 */

import { assertEquals, assertMatch } from "@std/assert";
import { KhiveMcpClient } from "../mcp/client.ts";

const BINARY = (() => {
  const url = new URL(import.meta.url);
  // Resolve relative to this file: deno/src/commands/ → crates/target/release/khive-mcp
  const parts = url.pathname.split("/");
  // Drop "src/commands/entity_integration_test.ts" (3 segments)
  const denoDir = parts.slice(0, parts.length - 3).join("/");
  return `${denoDir}/../crates/target/release/khive-mcp`;
})();

/** Check if the binary exists before running integration tests. */
async function binaryExists(): Promise<boolean> {
  try {
    const stat = await Deno.stat(BINARY);
    return stat.isFile;
  } catch {
    return false;
  }
}

/** Extract JSON payload from an MCP CallToolResult. */
function extractText(result: unknown): string {
  const r = result as { content?: Array<{ type: string; text?: string }>; isError?: boolean };
  if (r.isError) {
    const errMsg = r.content?.find((c) => c.type === "text")?.text ?? "Unknown error";
    throw new Error(errMsg);
  }
  return r.content?.find((c) => c.type === "text")?.text ?? "";
}

Deno.test(
  {
    name: "entity integration: full create→get→list→delete lifecycle",
    // MCP SDK transport uses internal timers not exposed for cleanup
    sanitizeResources: false,
    sanitizeOps: false,
  },
  async () => {
    if (!(await binaryExists())) {
      console.log(
        `SKIP: binary not found at ${BINARY}. Build with: cd crates && cargo build --release -p khive-mcp`,
      );
      return;
    }

    // Connect to a real khive-mcp process with an in-memory DB
    const command = `${BINARY} --db :memory: --no-embed --log error`;
    const client = await KhiveMcpClient.connect(command);

    try {
      // --- CREATE ---
      // Flat verb: kind="entity", entity_kind=<the entity kind>, name required
      const createResult = await client.callTool("create", {
        kind: "entity",
        entity_kind: "concept",
        name: "IntegrationTestEntity",
        description: "Created by Deno integration test",
      });
      const createText = extractText(createResult);
      const created = JSON.parse(createText);

      // create returns the entity directly (not wrapped)
      assertEquals(created.name, "IntegrationTestEntity", "create: name mismatch");
      assertEquals(created.kind, "concept", "create: kind should be the entity_kind");
      const entityId: string = created.id ?? created.full_id;
      assertMatch(entityId, /^[0-9a-f-]{8,}/, "create: id should be a UUID or short form");
      console.log(`  [create] id=${entityId} name=${created.name} kind=${created.kind}`);

      // --- GET ---
      // get returns {"kind": "entity", "data": {...}} — must read .data.*
      const getResult = await client.callTool("get", { id: entityId });
      const getText = extractText(getResult);
      const wrapped = JSON.parse(getText);

      assertEquals(wrapped.kind, "entity", "get: wrapper kind should be 'entity'");
      const fetched = wrapped.data;
      assertEquals(fetched.name, "IntegrationTestEntity", "get: data.name mismatch");
      assertEquals(
        fetched.description,
        "Created by Deno integration test",
        "get: data.description mismatch",
      );
      console.log(`  [get]    kind=${wrapped.kind} data.name=${fetched.name}`);

      // --- LIST ---
      // list requires kind="entity" discriminant; entity_kind filters within entities
      const listResult = await client.callTool("list", {
        kind: "entity",
        entity_kind: "concept",
      });
      const listText = extractText(listResult);
      const entities: Array<Record<string, unknown>> = JSON.parse(listText);

      const foundByName = entities.find((e) => e.name === "IntegrationTestEntity");
      if (!foundByName) {
        throw new Error(`list: entity not found. Got: ${JSON.stringify(entities)}`);
      }
      console.log(`  [list]   ${entities.length} concept(s), contains our entity`);

      // --- DELETE ---
      // delete auto-detects kind from UUID; no kind= needed
      const deleteResult = await client.callTool("delete", { id: entityId });
      const deleteText = extractText(deleteResult);
      const deleted = JSON.parse(deleteText);

      assertEquals(deleted.deleted, true, "delete: expected deleted=true");
      console.log(`  [delete] deleted=${deleted.deleted}`);

      // --- VERIFY GONE ---
      // After soft-delete, list should no longer return it
      const listAfterResult = await client.callTool("list", {
        kind: "entity",
        entity_kind: "concept",
      });
      const listAfterText = extractText(listAfterResult);
      const afterEntities: Array<Record<string, unknown>> = JSON.parse(listAfterText);
      const stillPresent = afterEntities.find((e) => e.name === "IntegrationTestEntity");
      if (stillPresent) {
        throw new Error(
          `delete: entity still in list after soft-delete: ${JSON.stringify(stillPresent)}`,
        );
      }
      console.log(`  [verify] entity absent from list after delete`);

      console.log("\n  ALL entity lifecycle steps PASSED");
    } finally {
      await client.close();
    }
  },
);

Deno.test(
  {
    name: "entity integration: flat verb names — entity_create rejected",
    sanitizeResources: false,
    sanitizeOps: false,
  },
  async () => {
    if (!(await binaryExists())) {
      return; // skip if binary not available
    }

    const command = `${BINARY} --db :memory: --no-embed --log error`;
    const client = await KhiveMcpClient.connect(command);

    try {
      // entity_create does NOT exist in the 11-tool surface — the server should reject it
      let gotError = false;
      try {
        const r = await client.callTool("entity_create", { kind: "concept", name: "Test" });
        // Some SDK versions return isError instead of throwing
        const text = extractText(r as unknown);
        if (!text) gotError = true; // empty text means error path
      } catch {
        gotError = true;
      }
      assertEquals(
        gotError,
        true,
        "entity_create should not be a valid tool name (flat verbs only)",
      );
      console.log("  [regression] entity_create correctly rejected");
    } finally {
      await client.close();
    }
  },
);
