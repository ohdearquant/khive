/**
 * khive CLI — entry point.
 *
 * Dispatch tree:
 *   khive kg <subcommand>    — KG git-native workflow (ADR-048 + ADR-051)
 *   khive auth <subcommand>  — optional platform auth (ADR-051 §1, phase C2)
 */

import { kgInit } from "./kg/mod.ts";
import { runCommit } from "./kg/commit.ts";
import { runSync } from "./kg/sync.ts";
import { runStatus } from "./kg/status.ts";
import { runValidate } from "./kg/validate.ts";
import { getRepoRoot } from "./lib/git.ts";

const VERSION = "0.1.0";

function printUsage(): void {
  console.log(`khive ${VERSION} — research knowledge graph CLI

Usage:
  khive kg <subcommand>    Manage the git-native knowledge graph
  khive auth <subcommand>  Authenticate with khive.ai (optional)

KG subcommands:
  init          Initialise .khive/kg/ in the current git repo
  export        Export live DB to .khive/kg/entities.ndjson + edges.ndjson
  import        Import .khive/kg/ NDJSON files into working.db
  validate      Check NDJSON files against schema (no DB writes)
  commit        Export + validate + git commit in one step
  sync          Rebuild working.db from committed NDJSON files
  status        Show entity/edge counts and uncommitted changes
  diff          Entity-aware diff between two NDJSON states
  embed         Embed entities into working.db (for vector search)
  update        Advance a remote pin in schema.yaml

Auth subcommands:
  login         Sign in to khive.ai via GitHub OAuth
  status        Show current authentication state
  logout        Remove stored credentials

All 'khive kg' commands work without a khive.ai account.

Run 'khive <group> <subcommand> --help' for detailed usage.`);
}

function printKgUsage(): void {
  console.log(`Usage: khive kg <subcommand>

Subcommands:
  init          Initialise .khive/kg/ in the current git repo
  export        Export live DB to NDJSON files
  import        Import NDJSON files into working.db
  validate      Validate NDJSON files against schema
  commit        Export + validate + git commit
  sync          Rebuild working.db from NDJSON
  status        Show KG status
  diff          Entity-aware diff
  embed         Embed entities for vector search
  update        Advance a remote pin`);
}

function printAuthUsage(): void {
  console.log(`Usage: khive auth <subcommand>

Subcommands:
  login         Sign in to khive.ai
  status        Show authentication state
  logout        Remove stored credentials`);
}

async function dispatchKg(args: string[]): Promise<void> {
  const [subcommand, ...rest] = args;

  if (!subcommand || subcommand === "--help" || subcommand === "-h") {
    printKgUsage();
    return;
  }

  switch (subcommand) {
    case "init":
      await kgInit();
      break;

    case "commit":
      await runCommit(await getRepoRoot(), rest);
      break;
    case "sync":
      await runSync(await getRepoRoot(), rest);
      break;
    case "status":
      await runStatus(await getRepoRoot(), rest);
      break;
    case "validate":
      await runValidate(await getRepoRoot());
      break;

    case "export":
    case "import":
    case "diff":
    case "embed":
    case "update":
      console.error(
        `'khive kg ${subcommand}' is not yet implemented (phase E3 — v0.4+).`,
      );
      Deno.exit(1);
      break;

    default:
      console.error(`Unknown kg subcommand: '${subcommand}'`);
      console.error("Run 'khive kg --help' for available subcommands.");
      Deno.exit(1);
  }

  void rest; // future flags
}

function dispatchAuth(args: string[]): void {
  const [subcommand] = args;

  if (!subcommand || subcommand === "--help" || subcommand === "-h") {
    printAuthUsage();
    return;
  }

  // Auth is phase C2 (v0.4+).
  switch (subcommand) {
    case "login":
    case "status":
    case "logout":
      console.error(
        `'khive auth ${subcommand}' is not yet implemented (phase C2 — v0.4+).`,
      );
      Deno.exit(1);
      break;

    default:
      console.error(`Unknown auth subcommand: '${subcommand}'`);
      console.error("Run 'khive auth --help' for available subcommands.");
      Deno.exit(1);
  }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

const [group, ...groupArgs] = Deno.args;

if (!group || group === "--help" || group === "-h") {
  printUsage();
} else if (group === "--version" || group === "-V") {
  console.log(`khive ${VERSION}`);
} else if (group === "kg") {
  await dispatchKg(groupArgs);
} else if (group === "auth") {
  dispatchAuth(groupArgs);
} else {
  console.error(`Unknown command group: '${group}'`);
  console.error("Run 'khive --help' for usage.");
  Deno.exit(1);
}
