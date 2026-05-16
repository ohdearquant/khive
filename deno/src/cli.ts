/**
 * khive CLI — single binary entry point.
 *
 * Spawns khive-mcp via stdio and dispatches subcommands to its MCP tools.
 *
 * Usage:
 *   khive entity add concept "FlashAttention" --description "..."
 *   khive query "MATCH (c:concept) WHERE c.name CONTAINS 'attention' RETURN c"
 *   khive traverse <node-id> --depth 3
 *   khive init
 */

import { parseArgs } from "@std/cli/parse-args";
import { runEntity } from "./commands/entity.ts";
import { runQuery } from "./commands/query.ts";
import { runTraverse } from "./commands/traverse.ts";
import { runInit } from "./commands/init.ts";

const USAGE = `khive — research knowledge graph CLI

Usage:
  khive <command> [options]

Commands:
  entity      Manage entities (add, get, list, delete)
  query       Execute a SPARQL or GQL query
  traverse    Walk the graph from a starting node
  init        Initialize a new khive database

Global options:
  --namespace <ns>    Namespace to operate in (default: "local")
  --db <path>         Path to khive.db (default: ~/.khive/khive.db)
  --help, -h          Show this help
`;

async function main(args: string[]): Promise<number> {
  const flags = parseArgs(args, {
    string: ["namespace", "db"],
    boolean: ["help"],
    alias: { h: "help" },
    stopEarly: true,
  });

  if (flags.help || flags._.length === 0) {
    console.log(USAGE);
    return flags.help ? 0 : 1;
  }

  const [command, ...rest] = flags._.map(String);

  switch (command) {
    case "entity":
      return await runEntity(rest, flags);
    case "query":
      return await runQuery(rest, flags);
    case "traverse":
      return await runTraverse(rest, flags);
    case "init":
      return await runInit(rest, flags);
    default:
      console.error(`Unknown command: ${command}\n`);
      console.error(USAGE);
      return 1;
  }
}

Deno.exit(await main(Deno.args));
