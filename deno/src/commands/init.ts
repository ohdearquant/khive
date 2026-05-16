// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

import { parseArgs } from "@std/cli/parse-args";
import { join } from "@std/path";
import { KhiveMcpClient } from "../mcp/client.ts";

const USAGE = `khive init — initialize khive configuration

Usage:
  khive init [--db <PATH>] [--namespace <NS>] [--force]

Writes a config file to:
  Linux/macOS: ~/.config/khive/config.json
  Windows:     %APPDATA%\\khive\\config.json

The init command also spawns khive-mcp to ensure the database schema is
created and migrations are applied.

Options:
  --db <PATH>         Path to khive.db (default: ~/.khive/khive.db)
  --namespace <NS>    Default namespace (default: "local")
  --force             Overwrite existing config without prompting
  --json              Output raw JSON
  --help, -h          Show this help
`;

interface KhiveConfig {
  db: string;
  namespace: string;
}

/** Platform-appropriate config directory. */
function configDir(): string {
  const platform = Deno.build.os;
  if (platform === "windows") {
    return join(Deno.env.get("APPDATA") ?? join(Deno.env.get("HOME") ?? ".", ".config"), "khive");
  }
  // macOS and Linux both respect XDG_CONFIG_HOME
  const xdg = Deno.env.get("XDG_CONFIG_HOME");
  if (xdg) return join(xdg, "khive");
  return join(Deno.env.get("HOME") ?? ".", ".config", "khive");
}

/** Default database path. */
function defaultDbPath(): string {
  return join(Deno.env.get("HOME") ?? ".", ".khive", "khive.db");
}

export async function runInit(
  args: string[],
  globals: { db?: string; namespace?: string },
): Promise<number> {
  const flags = parseArgs(args, {
    string: ["db", "namespace"],
    boolean: ["help", "force", "json"],
    alias: { h: "help", ns: "namespace" },
  });

  if (flags.help) {
    console.log(USAGE);
    return 0;
  }

  const dbPath = flags.db ?? globals.db ?? defaultDbPath();
  const namespace = flags.namespace ?? globals.namespace ?? "local";
  const config: KhiveConfig = { db: dbPath, namespace };

  const dir = configDir();
  const configPath = join(dir, "config.json");

  // Check if config already exists
  let exists = false;
  try {
    await Deno.stat(configPath);
    exists = true;
  } catch {
    // Does not exist — proceed
  }

  if (exists && !flags.force) {
    const answer = prompt(`Config already exists at ${configPath}. Overwrite? [y/N]`);
    if (!answer || !["y", "yes"].includes(answer.toLowerCase().trim())) {
      console.log("Aborted.");
      return 0;
    }
  }

  // Ensure config directory exists
  await Deno.mkdir(dir, { recursive: true });

  const configJson = JSON.stringify(config, null, 2);
  await Deno.writeTextFile(configPath, configJson + "\n");

  // Spawn khive-mcp pointed at the chosen db to initialize schema
  const mcpCommand = `khive-mcp --db ${dbPath}`;
  try {
    const client = await KhiveMcpClient.connect(mcpCommand);
    try {
      await client.listTools();
    } finally {
      await client.close();
    }
    const dbStatus = "database schema initialized";

    if (flags.json) {
      console.log(
        JSON.stringify({ config_path: configPath, config, db_status: dbStatus }, null, 2),
      );
    } else {
      console.log(`Config written to: ${configPath}`);
      console.log(configJson);
      console.log(`\nDatabase: ${dbStatus} at ${dbPath}`);
    }
  } catch (_err) {
    // khive-mcp not on PATH or failed — still write config, just warn
    if (flags.json) {
      console.log(
        JSON.stringify(
          {
            config_path: configPath,
            config,
            db_status: "khive-mcp not available — schema not initialized",
          },
          null,
          2,
        ),
      );
    } else {
      console.log(`Config written to: ${configPath}`);
      console.log(configJson);
      console.log(
        "\nWarning: khive-mcp binary not found on PATH. Database schema was not initialized.",
      );
      console.log(`  Install khive-mcp and run \`khive init\` again, or set KHIVE_MCP_COMMAND.`);
    }
  }

  return 0;
}
