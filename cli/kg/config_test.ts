/**
 * Tests for cli/kg/config.ts — config command (ADR-057).
 */

import { assertEquals, assertMatch, assertRejects, assertStringIncludes } from "@std/assert";
import { join } from "@std/path";
import { checkSetKey, runConfig, validateSetValue, writeConfigKey } from "./config.ts";
import { CONFIG_FILE } from "../lib/paths.ts";

const DEFAULT_CONFIG = `\
[embed]
model = "mE5-small"
dimensions = 384
auto_embed = true
batch_size = 64

[embed.fields]
include = ["name", "description"]

[schema]
strict = true
`;

async function withTempRepo(
  withConfig: boolean,
  fn: (root: string) => Promise<void>,
): Promise<void> {
  const root = await Deno.makeTempDir({ prefix: "khive-config-test-" });
  await Deno.mkdir(join(root, ".khive"), { recursive: true });
  if (withConfig) {
    await Deno.writeTextFile(join(root, CONFIG_FILE), DEFAULT_CONFIG);
  }
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

function captureStderr(): { restore: () => string } {
  const original = console.error;
  const chunks: string[] = [];
  console.error = (...args: unknown[]) => {
    chunks.push(args.map(String).join(" "));
  };
  return {
    restore: () => {
      console.error = original;
      return chunks.join("\n");
    },
  };
}

Deno.test("config (show) — prints resolved defaults when no project file", async () => {
  await withTempRepo(false, async (root) => {
    const cap = captureStdout();
    await runConfig(root, []);
    const out = cap.restore();
    assertStringIncludes(out, "[embed]");
    assertStringIncludes(out, 'model = "mE5-small"');
    assertStringIncludes(out, "auto_embed = true");
    assertStringIncludes(out, "[schema]");
  });
});

Deno.test("config (path) — prints the project config path", async () => {
  await withTempRepo(false, async (root) => {
    const cap = captureStdout();
    await runConfig(root, ["path"]);
    const out = cap.restore();
    assertStringIncludes(out, `${root}/.khive/config.toml`);
  });
});

Deno.test("config get — returns single value", async () => {
  await withTempRepo(true, async (root) => {
    const cap = captureStdout();
    await runConfig(root, ["get", "embed.model"]);
    assertEquals(cap.restore().trim(), "mE5-small");
  });
});

Deno.test("config get — nested boolean reads as 'true'", async () => {
  await withTempRepo(true, async (root) => {
    const cap = captureStdout();
    await runConfig(root, ["get", "embed.auto_embed"]);
    assertEquals(cap.restore().trim(), "true");
  });
});

Deno.test("config get — array reads as JSON", async () => {
  await withTempRepo(true, async (root) => {
    const cap = captureStdout();
    await runConfig(root, ["get", "embed.fields.include"]);
    assertEquals(cap.restore().trim(), '["name","description"]');
  });
});

Deno.test("config set — updates an existing key", async () => {
  await withTempRepo(true, async (root) => {
    const cap = captureStdout();
    await runConfig(root, ["set", "embed.model", "bge-large-en"]);
    cap.restore();

    const text = await Deno.readTextFile(join(root, CONFIG_FILE));
    assertStringIncludes(text, 'model = "bge-large-en"');

    const cap2 = captureStdout();
    await runConfig(root, ["get", "embed.model"]);
    assertEquals(cap2.restore().trim(), "bge-large-en");
  });
});

Deno.test("config set — boolean literal preserved", async () => {
  await withTempRepo(true, async (root) => {
    const cap = captureStdout();
    await runConfig(root, ["set", "embed.auto_embed", "false"]);
    cap.restore();

    const text = await Deno.readTextFile(join(root, CONFIG_FILE));
    assertStringIncludes(text, "auto_embed = false");
  });
});

Deno.test("config set — integer literal preserved", async () => {
  await withTempRepo(true, async (root) => {
    const cap = captureStdout();
    await runConfig(root, ["set", "embed.batch_size", "128"]);
    cap.restore();

    const text = await Deno.readTextFile(join(root, CONFIG_FILE));
    assertStringIncludes(text, "batch_size = 128");
  });
});

// ─── Key governance tests (ADR-057 §2) ───────────────────────────────────────

Deno.test("checkSetKey — accepts project-level key without --global", () => {
  assertEquals(checkSetKey("embed.model", false), null);
  assertEquals(checkSetKey("embed.dimensions", false), null);
  assertEquals(checkSetKey("embed.auto_embed", false), null);
  assertEquals(checkSetKey("embed.batch_size", false), null);
  assertEquals(checkSetKey("schema.strict", false), null);
});

Deno.test("checkSetKey — rejects embed.fields.include (array key, edit manually)", () => {
  const err = checkSetKey("embed.fields.include", false);
  assertMatch(err ?? "", /Unknown config key/);
});

Deno.test("checkSetKey — rejects user-level key without --global", () => {
  const err = checkSetKey("embed.device", false);
  assertMatch(err ?? "", /user-level/);
  assertMatch(err ?? "", /--global/);
});

Deno.test("checkSetKey — rejects auth.api_url without --global", () => {
  const err = checkSetKey("auth.api_url", false);
  assertMatch(err ?? "", /user-level/);
  assertMatch(err ?? "", /--global/);
});

Deno.test("checkSetKey — accepts user-level key with --global", () => {
  assertEquals(checkSetKey("embed.device", true), null);
  assertEquals(checkSetKey("auth.api_url", true), null);
});

Deno.test("checkSetKey — rejects unknown key", () => {
  const err = checkSetKey("embed.unknownkey", false);
  assertMatch(err ?? "", /Unknown config key/);
});

Deno.test("config set — rejects embed.device without --global (user-level key)", async () => {
  await withTempRepo(true, async (root) => {
    const errCap = captureStderr();
    let exitCalled = false;
    const origExit = Deno.exit;
    // @ts-ignore: stub for test
    Deno.exit = () => {
      exitCalled = true;
      throw new Error("__exit__");
    };
    try {
      await runConfig(root, ["set", "embed.device", "cuda"]);
    } catch (e) {
      if (!(e instanceof Error) || e.message !== "__exit__") throw e;
    } finally {
      Deno.exit = origExit;
      errCap.restore();
    }
    assertEquals(exitCalled, true);
    // Project config must NOT contain device = "cuda"
    const text = await Deno.readTextFile(join(root, CONFIG_FILE));
    assertEquals(text.includes("device"), false);
  });
});

// ─── --global append tests (Major 4 fix) ─────────────────────────────────────

Deno.test("writeConfigKey --global: creates file from scratch when missing", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive-global-test-" });
  const configPath = `${dir}/config.toml`;
  try {
    const msg = await writeConfigKey(configPath, "embed.device", "metal", true);
    assertStringIncludes(msg, "embed.device");
    assertStringIncludes(msg, "metal");
    const text = await Deno.readTextFile(configPath);
    assertStringIncludes(text, "[embed]");
    assertStringIncludes(text, 'device = "metal"');
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("writeConfigKey --global: appends [embed] table to empty file", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive-global-test-" });
  const configPath = `${dir}/config.toml`;
  try {
    await Deno.writeTextFile(configPath, "");
    await writeConfigKey(configPath, "embed.device", "cuda", true);
    const text = await Deno.readTextFile(configPath);
    assertStringIncludes(text, "[embed]");
    assertStringIncludes(text, 'device = "cuda"');
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("writeConfigKey --global: appends [embed] while preserving existing [auth]", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive-global-test-" });
  const configPath = `${dir}/config.toml`;
  try {
    await Deno.writeTextFile(configPath, '[auth]\napi_url = "https://api.khive.ai"\n');
    await writeConfigKey(configPath, "embed.device", "cpu", true);
    const text = await Deno.readTextFile(configPath);
    assertStringIncludes(text, "[auth]");
    assertStringIncludes(text, 'api_url = "https://api.khive.ai"');
    assertStringIncludes(text, "[embed]");
    assertStringIncludes(text, 'device = "cpu"');
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("writeConfigKey --global: adds device under existing [embed] table", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive-global-test-" });
  const configPath = `${dir}/config.toml`;
  try {
    await Deno.writeTextFile(configPath, '[embed]\nmodel = "mE5-small"\n');
    await writeConfigKey(configPath, "embed.device", "metal", true);
    const text = await Deno.readTextFile(configPath);
    assertStringIncludes(text, 'model = "mE5-small"');
    assertStringIncludes(text, 'device = "metal"');
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("writeConfigKey --global: overwrites existing embed.device in place", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive-global-test-" });
  const configPath = `${dir}/config.toml`;
  try {
    await Deno.writeTextFile(configPath, '[embed]\ndevice = "cpu"\n');
    await writeConfigKey(configPath, "embed.device", "metal", true);
    const text = await Deno.readTextFile(configPath);
    assertStringIncludes(text, 'device = "metal"');
    assertEquals(text.includes('"cpu"'), false);
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("writeConfigKey project mode: throws when table is missing (not --global)", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive-proj-test-" });
  const configPath = `${dir}/config.toml`;
  try {
    await Deno.writeTextFile(configPath, "[schema]\nstrict = true\n");
    await assertRejects(
      () => writeConfigKey(configPath, "embed.model", "BGE-large", false),
      Error,
      "Edit the file manually",
    );
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

// ─── validateSetValue tests (MAJOR 2 — value-semantic validation) ─────────────

Deno.test("validateSetValue: accepts valid positive integer for embed.dimensions", () => {
  assertEquals(validateSetValue("embed.dimensions", "768"), null);
  assertEquals(validateSetValue("embed.batch_size", "128"), null);
});

Deno.test("validateSetValue: rejects decimal for embed.dimensions", () => {
  const err = validateSetValue("embed.dimensions", "3.14");
  assertMatch(err ?? "", /positive integer/);
});

Deno.test("validateSetValue: rejects zero for embed.dimensions", () => {
  const err = validateSetValue("embed.dimensions", "0");
  assertMatch(err ?? "", /positive integer/);
});

Deno.test("validateSetValue: rejects negative for embed.dimensions", () => {
  const err = validateSetValue("embed.dimensions", "-5");
  assertMatch(err ?? "", /positive integer/);
});

Deno.test("validateSetValue: rejects non-numeric for embed.batch_size", () => {
  const err = validateSetValue("embed.batch_size", "abc");
  assertMatch(err ?? "", /positive integer/);
});

Deno.test("validateSetValue: rejects zero for embed.batch_size", () => {
  const err = validateSetValue("embed.batch_size", "0");
  assertMatch(err ?? "", /positive integer/);
});

Deno.test("validateSetValue: rejects negative for embed.batch_size", () => {
  const err = validateSetValue("embed.batch_size", "-1");
  assertMatch(err ?? "", /positive integer/);
});

Deno.test("validateSetValue: accepts valid booleans for embed.auto_embed", () => {
  assertEquals(validateSetValue("embed.auto_embed", "true"), null);
  assertEquals(validateSetValue("embed.auto_embed", "false"), null);
});

Deno.test("validateSetValue: rejects non-boolean for embed.auto_embed", () => {
  const err = validateSetValue("embed.auto_embed", "maybe");
  assertMatch(err ?? "", /boolean/);
});

Deno.test("validateSetValue: rejects '1' for embed.auto_embed (not strict bool)", () => {
  const err = validateSetValue("embed.auto_embed", "1");
  assertMatch(err ?? "", /boolean/);
});

Deno.test("validateSetValue: rejects 'TRUE' for embed.auto_embed (case-sensitive)", () => {
  const err = validateSetValue("embed.auto_embed", "TRUE");
  assertMatch(err ?? "", /boolean/);
});

Deno.test("validateSetValue: rejects non-boolean for schema.strict", () => {
  const err = validateSetValue("schema.strict", "1");
  assertMatch(err ?? "", /boolean/);
});

Deno.test("validateSetValue: rejects 'TRUE' for schema.strict (case-sensitive)", () => {
  const err = validateSetValue("schema.strict", "TRUE");
  assertMatch(err ?? "", /boolean/);
});

Deno.test("validateSetValue: rejects empty string for embed.model", () => {
  const err = validateSetValue("embed.model", "");
  assertMatch(err ?? "", /non-empty/);
});

Deno.test("validateSetValue: rejects whitespace-only for embed.model", () => {
  const err = validateSetValue("embed.model", "  ");
  assertMatch(err ?? "", /non-empty/);
});

Deno.test("validateSetValue: rejects empty string for auth.api_url", () => {
  const err = validateSetValue("auth.api_url", "");
  assertMatch(err ?? "", /non-empty/);
});

Deno.test("validateSetValue: rejects whitespace-only for auth.api_url", () => {
  const err = validateSetValue("auth.api_url", "  ");
  assertMatch(err ?? "", /non-empty/);
});

Deno.test("validateSetValue: accepts valid device enum for embed.device", () => {
  assertEquals(validateSetValue("embed.device", "metal"), null);
  assertEquals(validateSetValue("embed.device", "cuda"), null);
  assertEquals(validateSetValue("embed.device", "cpu"), null);
});

Deno.test("validateSetValue: rejects invalid device for embed.device", () => {
  const err = validateSetValue("embed.device", "tpu");
  assertMatch(err ?? "", /metal|cuda|cpu/);
});

// ─── config set integration: invalid values must not write to disk ────────────

Deno.test("config set: rejects embed.dimensions=3.14 and leaves file unchanged", async () => {
  await withTempRepo(true, async (root) => {
    const before = await Deno.readTextFile(join(root, CONFIG_FILE));
    const errCap = captureStderr();
    let exitCalled = false;
    const origExit = Deno.exit;
    // @ts-ignore: stub for test
    Deno.exit = () => {
      exitCalled = true;
      throw new Error("__exit__");
    };
    try {
      await runConfig(root, ["set", "embed.dimensions", "3.14"]);
    } catch (e) {
      if (!(e instanceof Error) || e.message !== "__exit__") throw e;
    } finally {
      Deno.exit = origExit;
      errCap.restore();
    }
    assertEquals(exitCalled, true);
    const after = await Deno.readTextFile(join(root, CONFIG_FILE));
    assertEquals(after, before);
  });
});

Deno.test("config set: rejects embed.dimensions=0 and leaves file unchanged", async () => {
  await withTempRepo(true, async (root) => {
    const before = await Deno.readTextFile(join(root, CONFIG_FILE));
    const errCap = captureStderr();
    let exitCalled = false;
    const origExit = Deno.exit;
    // @ts-ignore: stub for test
    Deno.exit = () => {
      exitCalled = true;
      throw new Error("__exit__");
    };
    try {
      await runConfig(root, ["set", "embed.dimensions", "0"]);
    } catch (e) {
      if (!(e instanceof Error) || e.message !== "__exit__") throw e;
    } finally {
      Deno.exit = origExit;
      errCap.restore();
    }
    assertEquals(exitCalled, true);
    const after = await Deno.readTextFile(join(root, CONFIG_FILE));
    assertEquals(after, before);
  });
});

Deno.test("config set: rejects embed.batch_size=abc and leaves file unchanged", async () => {
  await withTempRepo(true, async (root) => {
    const before = await Deno.readTextFile(join(root, CONFIG_FILE));
    const errCap = captureStderr();
    let exitCalled = false;
    const origExit = Deno.exit;
    // @ts-ignore: stub for test
    Deno.exit = () => {
      exitCalled = true;
      throw new Error("__exit__");
    };
    try {
      await runConfig(root, ["set", "embed.batch_size", "abc"]);
    } catch (e) {
      if (!(e instanceof Error) || e.message !== "__exit__") throw e;
    } finally {
      Deno.exit = origExit;
      errCap.restore();
    }
    assertEquals(exitCalled, true);
    const after = await Deno.readTextFile(join(root, CONFIG_FILE));
    assertEquals(after, before);
  });
});

Deno.test("config set: rejects embed.auto_embed=maybe and leaves file unchanged", async () => {
  await withTempRepo(true, async (root) => {
    const before = await Deno.readTextFile(join(root, CONFIG_FILE));
    const errCap = captureStderr();
    let exitCalled = false;
    const origExit = Deno.exit;
    // @ts-ignore: stub for test
    Deno.exit = () => {
      exitCalled = true;
      throw new Error("__exit__");
    };
    try {
      await runConfig(root, ["set", "embed.auto_embed", "maybe"]);
    } catch (e) {
      if (!(e instanceof Error) || e.message !== "__exit__") throw e;
    } finally {
      Deno.exit = origExit;
      errCap.restore();
    }
    assertEquals(exitCalled, true);
    const after = await Deno.readTextFile(join(root, CONFIG_FILE));
    assertEquals(after, before);
  });
});

Deno.test("config set: accepts embed.dimensions=768 and writes to disk", async () => {
  await withTempRepo(true, async (root) => {
    const cap = captureStdout();
    await runConfig(root, ["set", "embed.dimensions", "768"]);
    cap.restore();
    const text = await Deno.readTextFile(join(root, CONFIG_FILE));
    assertStringIncludes(text, "dimensions = 768");
  });
});
