/**
 * Tests for two-level TOML config loader (ADR-057 §1–§2).
 */

import { assertEquals } from "@std/assert";
import { join } from "@std/path";
import { loadConfig } from "./config.ts";

async function makeTempDir(): Promise<string> {
  return await Deno.makeTempDir({ prefix: "khive_config_test_" });
}

async function removeDir(path: string): Promise<void> {
  await Deno.remove(path, { recursive: true });
}

Deno.test("loadConfig: returns built-in defaults when no config files exist", async () => {
  const dir = await makeTempDir();
  try {
    const config = await loadConfig(dir);
    assertEquals(config.embed.model, "mE5-small");
    assertEquals(config.embed.dimensions, 384);
    assertEquals(config.embed.auto_embed, true);
    assertEquals(config.embed.batch_size, 64);
    assertEquals(config.embed.device, "cpu");
    assertEquals(config.embed.fields.include, ["name", "description"]);
    assertEquals(config.schema.strict, true);
    assertEquals(config.auth.api_url, "https://api.khive.ai");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("loadConfig: project config overrides defaults", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(join(dir, ".khive"), { recursive: true });
    await Deno.writeTextFile(
      join(dir, ".khive/config.toml"),
      '[embed]\nmodel = "BGE-large"\ndimensions = 1024\n',
    );

    const config = await loadConfig(dir);
    assertEquals(config.embed.model, "BGE-large");
    assertEquals(config.embed.dimensions, 1024);
    // Defaults still applied for unspecified keys.
    assertEquals(config.embed.auto_embed, true);
    assertEquals(config.embed.device, "cpu");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("loadConfig: partial project config merges with defaults", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(join(dir, ".khive"), { recursive: true });
    await Deno.writeTextFile(
      join(dir, ".khive/config.toml"),
      "[schema]\nstrict = false\n",
    );

    const config = await loadConfig(dir);
    assertEquals(config.schema.strict, false);
    // Embed defaults untouched.
    assertEquals(config.embed.model, "mE5-small");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("loadConfig: deepMerge does not pollute embed.fields with scalar", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(join(dir, ".khive"), { recursive: true });
    await Deno.writeTextFile(
      join(dir, ".khive/config.toml"),
      '[embed.fields]\ninclude = ["name"]\n',
    );

    const config = await loadConfig(dir);
    // Array replacement: only "name", not "name" + "description".
    assertEquals(config.embed.fields.include, ["name"]);
  } finally {
    await removeDir(dir);
  }
});
