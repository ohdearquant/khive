/**
 * khive CLI — entry point.
 *
 * Dispatch tree:
 *   khive kg <subcommand>    — KG git-native workflow (ADR-048 + ADR-051)
 *   khive auth <subcommand>  — optional platform auth (ADR-051 §1, phase C2)
 */

import { kgInit } from "./kg/mod.ts";
import { runCommit } from "./kg/commit.ts";
import { runConfig } from "./kg/config.ts";
import { runDiff } from "./kg/diff.ts";
import { runDoctor } from "./kg/doctor.ts";
import { runEmbed } from "./kg/embed.ts";
import { runExport } from "./kg/export.ts";
import { runHook } from "./kg/hook.ts";
import { recoverImportJournal, runImport } from "./kg/import.ts";
import { runLog } from "./kg/log.ts";
import { runMigrate } from "./kg/migrate.ts";
import { runPackCheck } from "./pack/check.ts";
import { runPackInit } from "./pack/init.ts";
import { runResolve } from "./kg/resolve.ts";
import { runStats } from "./kg/stats.ts";
import { runSync } from "./kg/sync.ts";
import { runStatus } from "./kg/status.ts";
import { runValidate } from "./kg/validate.ts";
import { getRepoRoot } from "./lib/git.ts";
import { kkernelPath } from "./lib/kernel.ts";
import { CLI_VERSION } from "./version.ts";

function printUsage(): void {
  console.log(`khive ${CLI_VERSION} — research knowledge graph CLI

Usage:
  khive mcp [flags]         Start the MCP stdio server (auto-spawns daemon)
  khive kg <subcommand>     Manage the git-native knowledge graph
  khive pack <subcommand>   Author and validate declarative packs (ADR-050)

KG subcommands:
  init          Initialise .khive/kg/ in the current git repo
  validate      Validate NDJSON files + rules.yaml (ADR-056)
  commit        Validate NDJSON files + git commit (Phase C1; DB export is Phase C2)
  sync          Validate NDJSON + create working.db placeholder (Phase C1; DB rebuild is Phase C2)
  status        Show entity/edge counts and uncommitted changes (file-level; DB diff is Phase C2)
  config        Show or modify .khive/config.toml (ADR-057)
  embed         Plan / run entity embedding (ADR-057; Phase C1 plans, Phase C2 runs)
  export        Re-write canonical .khive/kg/*.ndjson; --format archive emits a JSON bundle
  import        Import a KgArchive JSON file into NDJSON files
  resolve       Resolve NDJSON merge conflicts (ADR-053)
  hook          Manage the pre-commit validation hook (install/uninstall/status)
  migrate       Apply schema migrations from .khive/kg/migrations/ (ADR-054)
  diff          Entity-aware diff between two NDJSON states
  log           Show KG change history (commits touching .khive/kg/ files)
  stats         Show entity/edge counts, kind breakdown, schema coverage
  doctor        Validate KG integrity: syntax, refs, duplicates, orphans
  update        Advance a remote pin in schema.yaml (Phase C2 — not yet implemented)

Pack subcommands (ADR-050):
  init          Scaffold a new declarative pack
  check         Validate a pack.yaml manifest

All 10 built-in packs (kg, gtd, memory, brain, comm, schedule, knowledge,
session, git, code) load by default — no --pack flags needed.

Run 'khive <group> <subcommand> --help' for detailed usage.`);
}

function printKgUsage(): void {
  console.log(`Usage: khive kg <subcommand>

Subcommands (Phase C1 — file-level operations):
  init          Initialise .khive/kg/ in the current git repo
  validate      Validate NDJSON files + rules.yaml (ADR-056; flags: --strict, --no-rules, --format, --quiet)
  commit        Validate + stage + git commit .khive/kg/ files
  sync          Validate NDJSON (DB rebuild: Phase C2)
  status        Show entity/edge counts and uncommitted changes
  config        Show or modify .khive/config.toml
  embed         Plan embedding for entities awaiting vectors (run: Phase C2)
  export        Re-write canonical .khive/kg/*.ndjson; --format archive emits a JSON bundle
  import        Import a KgArchive JSON file (flags: --overwrite, --on-conflict <skip|replace|merge>)
  resolve       Resolve NDJSON merge conflicts after 'git merge'
  hook          Manage pre-commit validation hook (install|uninstall|status)
  migrate       Apply schema migrations (ADR-054)
  diff          Entity-aware diff between two NDJSON states (flags: --json, --name-only)
  log           Show KG change history (flags: -n <limit>, --json, --stat)
  stats         Show entity/edge counts, kind breakdown, schema coverage (flags: --json)
  doctor        Validate KG integrity: syntax, refs, duplicates, orphans (flags: --json)

Planned (Phase C2+):
  update        Advance a remote pin`);
}

async function dispatchKg(args: string[]): Promise<void> {
  const [subcommand, ...rest] = args;

  if (!subcommand || subcommand === "--help" || subcommand === "-h") {
    printKgUsage();
    return;
  }

  // Recover any interrupted import before running any KG command.
  // This is idempotent and a no-op when no journal exists.
  try {
    const root = await getRepoRoot();
    await recoverImportJournal(root);
  } catch {
    // Not in a git repo or .khive/ not present — no journal to recover.
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
    case "validate": {
      const code = await runValidate(await getRepoRoot(), rest);
      if (code !== 0) Deno.exit(code);
      break;
    }
    case "hook": {
      const code = await runHook(await getRepoRoot(), rest);
      if (code !== 0) Deno.exit(code);
      break;
    }

    case "config":
      await runConfig(await getRepoRoot(), rest);
      break;
    case "embed":
      await runEmbed(await getRepoRoot(), rest);
      break;
    case "resolve": {
      const code = await runResolve(await getRepoRoot(), rest);
      if (code !== 0) Deno.exit(code);
      break;
    }
    case "migrate": {
      const code = await runMigrate(await getRepoRoot(), rest);
      if (code !== 0) Deno.exit(code);
      break;
    }

    case "export":
      await runExport(await getRepoRoot(), rest);
      break;

    case "import":
      await runImport(await getRepoRoot(), rest);
      break;

    case "diff":
      await runDiff(await getRepoRoot(), rest);
      break;

    case "log":
      await runLog(await getRepoRoot(), rest);
      break;

    case "stats":
      await runStats(await getRepoRoot(), rest);
      break;

    case "doctor":
      await runDoctor(await getRepoRoot(), rest);
      break;

    case "update":
      console.error(
        `'khive kg update' is not yet implemented (phase C2 — v0.4+).`,
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

async function dispatchPack(args: string[]): Promise<void> {
  const [subcommand, ...rest] = args;
  if (!subcommand || subcommand === "--help" || subcommand === "-h") {
    console.log(`Usage: khive pack <subcommand>

Subcommands (ADR-050):
  init           Scaffold a new declarative pack (creates ./pack.yaml)
  check <path>   Validate a pack.yaml manifest

Planned:
  install        Install a pack from registry/local/git
  remove         Uninstall a pack
  publish        Publish to a pack registry`);
    return;
  }
  let code = 0;
  switch (subcommand) {
    case "init":
      code = await runPackInit(rest);
      break;
    case "check":
      code = await runPackCheck(rest);
      break;
    case "validate":
    case "install":
    case "remove":
    case "publish":
    case "search":
    case "info":
      console.error(
        `'khive pack ${subcommand}' is not yet implemented (deferred to Phase 2).`,
      );
      code = 1;
      break;
    default:
      console.error(`Unknown pack subcommand: '${subcommand}'`);
      console.error("Run 'khive pack --help' for available subcommands.");
      code = 1;
  }
  if (code !== 0) Deno.exit(code);
}

const [group, ...groupArgs] = Deno.args;

if (!group || group === "--help" || group === "-h") {
  printUsage();
} else if (group === "--version" || group === "-V") {
  console.log(`khive ${CLI_VERSION}`);
} else if (group === "kg") {
  await dispatchKg(groupArgs);
} else if (group === "pack") {
  await dispatchPack(groupArgs);
} else if (group === "mcp") {
  // `khive mcp [flags]` delegates to `kkernel mcp [flags]` — the MCP server
  // lives under the kkernel `mcp` subcommand (single-binary distribution).
  let repoRoot: string | undefined;
  try {
    repoRoot = await getRepoRoot();
  } catch {
    // Not in a git repo — production resolution (npm subpackage) does not need it.
  }
  const bin = kkernelPath(repoRoot);
  const proc = new Deno.Command(bin, {
    args: ["mcp", ...groupArgs],
    stdin: "inherit",
    stdout: "inherit",
    stderr: "inherit",
  });
  const status = await proc.output();
  Deno.exit(status.code);
} else {
  console.error(`Unknown command group: '${group}'`);
  console.error("Run 'khive --help' for usage.");
  Deno.exit(1);
}
