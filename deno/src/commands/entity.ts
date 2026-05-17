// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

import { parseArgs } from "@std/cli/parse-args";
import { KhiveMcpClient } from "../mcp/client.ts";

const USAGE = `khive entity — manage entities in the knowledge graph

Usage:
  khive entity create --kind <K> --name <N> [options]
  khive entity get <ID>
  khive entity list [--kind <K>] [--limit N]
  khive entity delete <ID> [--hard]

Subcommands:
  create    Create a new entity
  get       Fetch a single entity by UUID
  list      List entities, optionally filtered by kind
  delete    Delete an entity (soft by default, --hard for permanent)

Entity kinds: concept | document | dataset | project | person | org

Options for create:
  --kind <K>          Required. One of the entity kinds above
  --name <N>          Required. Human-readable name
  --description <D>   Optional description
  --namespace <NS>    Namespace (defaults to server default)
  --tags <a,b,c>      Comma-separated tags

Global options:
  --json              Output raw JSON
  --help, -h          Show this help
`;

/** Extract text content from an MCP CallToolResult.
 *
 * The MCP SDK returns a CallToolResult with:
 *   { content: [{ type: "text", text: "<json-string>" }], isError?: boolean }
 * This helper unwraps the text payload for JSON.parse.
 */
function extractText(result: unknown): string {
  const r = result as { content?: Array<{ type: string; text?: string }>; isError?: boolean };
  if (r.isError) {
    const errMsg = r.content?.find((c) => c.type === "text")?.text ?? "Unknown error";
    throw new Error(errMsg);
  }
  return r.content?.find((c) => c.type === "text")?.text ?? "";
}

export async function runEntity(
  args: string[],
  globals: { namespace?: string; db?: string },
): Promise<number> {
  const [subcommand, ...rest] = args;

  if (!subcommand || subcommand === "--help" || subcommand === "-h") {
    console.log(USAGE);
    return subcommand ? 0 : 1;
  }

  const client = await KhiveMcpClient.connect();
  try {
    switch (subcommand) {
      case "create":
        return await runCreate(client, rest, globals);
      case "get":
        return await runGet(client, rest, globals);
      case "list":
        return await runList(client, rest, globals);
      case "delete":
        return await runDelete(client, rest, globals);
      default:
        console.error(`Unknown entity subcommand: ${subcommand}\n`);
        console.error(USAGE);
        return 1;
    }
  } finally {
    await client.close();
  }
}

async function runCreate(
  client: KhiveMcpClient,
  args: string[],
  globals: { namespace?: string },
): Promise<number> {
  const flags = parseArgs(args, {
    string: ["kind", "name", "description", "namespace", "tags"],
    boolean: ["help", "json"],
    alias: { h: "help", k: "kind", n: "name", d: "description", ns: "namespace" },
  });

  if (flags.help) {
    console.log(`Usage: khive entity create --kind <K> --name <N> [--description <D>] \\
  [--namespace <NS>] [--tags tag1,tag2]`);
    return 0;
  }

  if (!flags.kind) {
    console.error("Error: --kind is required\n");
    console.error(
      "  Kinds: concept | document | dataset | project | person | org",
    );
    return 1;
  }
  if (!flags.name) {
    console.error("Error: --name is required");
    return 1;
  }

  const namespace = flags.namespace ?? globals.namespace;
  const tags = flags.tags ? flags.tags.split(",").map((t: string) => t.trim()) : undefined;

  // The flat `create` verb requires:
  //   kind="entity"         — discriminant for the create handler
  //   entity_kind=<K>       — the user-supplied entity kind (concept|document|...)
  //   name=<N>              — required for entities
  const toolArgs: Record<string, unknown> = {
    kind: "entity",
    entity_kind: flags.kind,
    name: flags.name,
  };
  if (flags.description) toolArgs.description = flags.description;
  if (namespace) toolArgs.namespace = namespace;
  if (tags) toolArgs.tags = tags;

  try {
    const result = await client.callTool("create", toolArgs);
    const text = extractText(result);
    if (flags.json) {
      console.log(text);
    } else {
      // create returns the entity object directly (not wrapped)
      const entity = JSON.parse(text);
      console.log(`Created entity:`);
      console.log(`  id:          ${entity.id ?? entity.full_id}`);
      console.log(`  kind:        ${entity.kind}`);
      console.log(`  name:        ${entity.name}`);
      if (entity.description) console.log(`  description: ${entity.description}`);
      if (entity.namespace) console.log(`  namespace:   ${entity.namespace}`);
      if (entity.tags?.length) console.log(`  tags:        ${entity.tags.join(", ")}`);
    }
    return 0;
  } catch (err) {
    console.error(`Error: ${(err as Error).message}`);
    return 1;
  }
}

async function runGet(
  client: KhiveMcpClient,
  args: string[],
  globals: { namespace?: string },
): Promise<number> {
  const flags = parseArgs(args, {
    string: ["namespace"],
    boolean: ["help", "json"],
    alias: { h: "help", ns: "namespace" },
  });

  if (flags.help) {
    console.log("Usage: khive entity get <ID>");
    return 0;
  }

  const id = flags._[0] as string | undefined;
  if (!id) {
    console.error("Error: entity ID is required\nUsage: khive entity get <ID>");
    return 1;
  }

  const namespace = flags.namespace ?? globals.namespace;
  const toolArgs: Record<string, unknown> = { id: String(id) };
  if (namespace) toolArgs.namespace = namespace;

  try {
    const result = await client.callTool("get", toolArgs);
    const text = extractText(result);
    if (flags.json) {
      console.log(text);
    } else {
      // get returns {"kind": "entity"|"note"|"edge", "data": {...}}
      const wrapped = JSON.parse(text);
      const entity = wrapped.data;
      console.log(`Entity:`);
      console.log(`  id:          ${entity.id ?? entity.full_id}`);
      console.log(`  kind:        ${entity.kind}`);
      console.log(`  name:        ${entity.name}`);
      if (entity.description) console.log(`  description: ${entity.description}`);
      if (entity.namespace) console.log(`  namespace:   ${entity.namespace}`);
      if (entity.tags?.length) console.log(`  tags:        ${entity.tags.join(", ")}`);
      if (entity.properties && Object.keys(entity.properties).length > 0) {
        console.log(`  properties:`);
        for (const [k, v] of Object.entries(entity.properties)) {
          console.log(`    ${k}: ${JSON.stringify(v)}`);
        }
      }
    }
    return 0;
  } catch (err) {
    console.error(`Error: ${(err as Error).message}`);
    return 1;
  }
}

async function runList(
  client: KhiveMcpClient,
  args: string[],
  globals: { namespace?: string },
): Promise<number> {
  const flags = parseArgs(args, {
    string: ["kind", "namespace"],
    boolean: ["help", "json"],
    default: { limit: 50 },
    alias: { h: "help", k: "kind", ns: "namespace", l: "limit" },
  });

  if (flags.help) {
    console.log("Usage: khive entity list [--kind <K>] [--limit N]");
    return 0;
  }

  const namespace = flags.namespace ?? globals.namespace;
  // The flat `list` verb requires kind="entity" as the discriminant.
  // The user's --kind flag (entity_kind filter) maps to entity_kind.
  const toolArgs: Record<string, unknown> = {
    kind: "entity",
    limit: Number(flags.limit),
  };
  if (flags.kind) toolArgs.entity_kind = flags.kind;
  if (namespace) toolArgs.namespace = namespace;

  try {
    const result = await client.callTool("list", toolArgs);
    const text = extractText(result);
    if (flags.json) {
      console.log(text);
    } else {
      const entities: Array<Record<string, unknown>> = JSON.parse(text);
      if (entities.length === 0) {
        console.log("No entities found.");
        return 0;
      }
      const idW = 8;
      const kindW = 10;
      const nameW = Math.min(
        40,
        Math.max(...entities.map((e) => String(e.name ?? "").length), 4),
      );
      const header = "ID".padEnd(idW) +
        "  " +
        "KIND".padEnd(kindW) +
        "  " +
        "NAME".padEnd(nameW);
      console.log(header);
      console.log("-".repeat(header.length));
      for (const e of entities) {
        const id = String(e.id ?? e.full_id ?? "").slice(0, idW).padEnd(idW);
        const kind = String(e.kind ?? "").padEnd(kindW);
        const name = String(e.name ?? "").slice(0, nameW).padEnd(nameW);
        console.log(`${id}  ${kind}  ${name}`);
      }
      console.log(`\n${entities.length} entity(ies)`);
    }
    return 0;
  } catch (err) {
    console.error(`Error: ${(err as Error).message}`);
    return 1;
  }
}

async function runDelete(
  client: KhiveMcpClient,
  args: string[],
  globals: { namespace?: string },
): Promise<number> {
  const flags = parseArgs(args, {
    string: ["namespace"],
    boolean: ["help", "hard", "json"],
    alias: { h: "help", ns: "namespace" },
  });

  if (flags.help) {
    console.log("Usage: khive entity delete <ID> [--hard]");
    return 0;
  }

  const id = flags._[0] as string | undefined;
  if (!id) {
    console.error("Error: entity ID is required\nUsage: khive entity delete <ID>");
    return 1;
  }

  const namespace = flags.namespace ?? globals.namespace;
  // delete auto-detects kind from UUID — only id (and optional hard/namespace) needed.
  const toolArgs: Record<string, unknown> = { id: String(id) };
  if (flags.hard) toolArgs.hard = true;
  if (namespace) toolArgs.namespace = namespace;

  try {
    const result = await client.callTool("delete", toolArgs);
    const text = extractText(result);
    if (flags.json) {
      console.log(text);
    } else {
      const res = JSON.parse(text);
      const verb = flags.hard ? "Permanently deleted" : "Soft-deleted";
      if (res.deleted) {
        console.log(`${verb} entity ${id}`);
      } else {
        console.log(`Entity ${id} not found or already deleted.`);
      }
    }
    return 0;
  } catch (err) {
    console.error(`Error: ${(err as Error).message}`);
    return 1;
  }
}
