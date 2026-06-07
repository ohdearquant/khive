/**
 * Tests for two-level TOML config loader (ADR-057 §1–§2).
 */

import { assertEquals, assertRejects, assertThrows } from "@std/assert";
import { join } from "@std/path";
import { ALLOWED_DEVICES, loadConfig, validateConfig, validateEmbedFields } from "./config.ts";

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

// ─── validateConfig tests (ADR-057 §8) ───────────────────────────────────────

Deno.test("validateConfig: accepts property key in embed.fields.include", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(join(dir, ".khive"), { recursive: true });
    // Property keys (entity.properties keys) are valid per ADR-057 §8.
    // "abstract" is not a top-level field but is a valid property key.
    await Deno.writeTextFile(
      join(dir, ".khive/config.toml"),
      '[embed.fields]\ninclude = ["name", "abstract"]\n',
    );
    const config = await loadConfig(dir);
    assertEquals(config.embed.fields.include, ["name", "abstract"]);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validateConfig: throws on reserved field 'kind' in embed.fields.include", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(join(dir, ".khive"), { recursive: true });
    await Deno.writeTextFile(
      join(dir, ".khive/config.toml"),
      '[embed.fields]\ninclude = ["name", "kind"]\n',
    );
    await assertRejects(
      () => loadConfig(dir),
      Error,
      "reserved",
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validateConfig: throws on empty embed.fields.include", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(join(dir, ".khive"), { recursive: true });
    await Deno.writeTextFile(
      join(dir, ".khive/config.toml"),
      "[embed.fields]\ninclude = []\n",
    );
    await assertRejects(
      () => loadConfig(dir),
      Error,
      "non-empty",
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validateConfig: throws on duplicate embed fields", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(join(dir, ".khive"), { recursive: true });
    await Deno.writeTextFile(
      join(dir, ".khive/config.toml"),
      '[embed.fields]\ninclude = ["name", "name"]\n',
    );
    await assertRejects(
      () => loadConfig(dir),
      Error,
      "duplicate",
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validateEmbedFields: exported from config.ts", () => {
  assertEquals(validateEmbedFields(["name", "description"]), null);
  // Property keys are accepted (open validation per ADR-057 §8).
  assertEquals(validateEmbedFields(["name", "abstract"]), null);
  // Reserved fields are still rejected.
  const err = validateEmbedFields(["name", "kind"]);
  assertEquals(typeof err, "string");
  assertEquals((err ?? "").includes("reserved"), true);
});

// ─── validateConfig direct tests (ADR-057 §8) ────────────────────────────────

function makeValidConfig() {
  return {
    embed: {
      model: "mE5-small",
      dimensions: 384,
      auto_embed: true,
      batch_size: 64,
      device: "cpu",
      fields: { include: ["name", "description"] },
    },
    schema: { strict: true },
    auth: { api_url: "https://api.khive.ai" },
  };
}

Deno.test("validateConfig: accepts valid config without throwing", () => {
  const config = makeValidConfig();
  // Must not throw — this directly exercises the validateConfig export.
  validateConfig(config);
});

Deno.test("validateConfig: accepts all valid embed.device values", () => {
  for (const device of ALLOWED_DEVICES) {
    const config = makeValidConfig();
    config.embed.device = device;
    validateConfig(config); // must not throw
  }
});

Deno.test("validateConfig: throws on invalid embed.device", () => {
  const config = makeValidConfig();
  config.embed.device = "tpu";
  assertThrows(
    () => validateConfig(config),
    Error,
    "tpu",
  );
});

Deno.test("validateConfig: throws on empty embed.model", () => {
  const config = makeValidConfig();
  config.embed.model = "";
  assertThrows(() => validateConfig(config), Error, "embed.model");
});

// ─── device validation integration tests ─────────────────────────────────────

Deno.test("loadConfig: embed.device in project config is REJECTED (user-only key)", async () => {
  // ADR-057 §2/§8: embed.device must not be committed to project config.
  // A manually-edited .khive/config.toml with embed.device must fail loudly.
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(join(dir, ".khive"), { recursive: true });
    await Deno.writeTextFile(
      join(dir, ".khive/config.toml"),
      `[embed]\ndevice = "cuda"\n`,
    );
    await assertRejects(
      () => loadConfig(dir),
      Error,
      "embed.device cannot be set in project config",
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("loadConfig: any embed.device in project config throws on load (user-only key)", async () => {
  // Even an otherwise-valid device value like "tpu" is rejected because
  // embed.device cannot appear in the project config at all (ADR-057 §2/§8).
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(join(dir, ".khive"), { recursive: true });
    await Deno.writeTextFile(
      join(dir, ".khive/config.toml"),
      `[embed]\ndevice = "tpu"\n`,
    );
    await assertRejects(
      () => loadConfig(dir),
      Error,
      "embed.device cannot be set in project config",
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("loadConfig: malformed embed (non-table) yields clear schema error, not raw 'in' error", async () => {
  // Regression: rejectUserOnlyKeys must guard the shape before using `in`.
  // A project config with `embed = "oops"` (string, not table) should produce
  // a clear schema error rather than `Cannot use 'in' operator to search for 'device' in oops`.
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(join(dir, ".khive"), { recursive: true });
    await Deno.writeTextFile(
      join(dir, ".khive/config.toml"),
      `embed = "oops"\n`,
    );
    await assertRejects(
      () => loadConfig(dir),
      Error,
      "embed must be a TOML table",
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("loadConfig: malformed auth (non-table) yields clear schema error, not raw 'in' error", async () => {
  // Regression: same shape guard for auth.
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(join(dir, ".khive"), { recursive: true });
    await Deno.writeTextFile(
      join(dir, ".khive/config.toml"),
      `auth = "oops"\n`,
    );
    await assertRejects(
      () => loadConfig(dir),
      Error,
      "auth must be a TOML table",
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("loadConfig: default device (cpu) loads without error", async () => {
  const dir = await makeTempDir();
  try {
    const config = await loadConfig(dir);
    assertEquals(config.embed.device, "cpu");
  } finally {
    await removeDir(dir);
  }
});
