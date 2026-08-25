import { assertEquals } from "@std/assert";
import { join } from "@std/path";
import { isolatedTestEnv } from "./helpers.ts";

Deno.test("isolated test environment ignores host Git configuration", async () => {
  const root = await Deno.makeTempDir({ prefix: "khive_git_config_" });
  try {
    const globalConfig = join(root, "gitconfig");
    await Deno.writeTextFile(globalConfig, "[core]\n\thooksPath = /hostile/hooks\n");

    const env = isolatedTestEnv({
      ...Deno.env.toObject(),
      GIT_CONFIG_GLOBAL: globalConfig,
      GIT_CONFIG_SYSTEM: globalConfig,
    });
    const result = await new Deno.Command("git", {
      args: ["config", "--global", "--get", "core.hooksPath"],
      env,
      stdout: "piped",
      stderr: "piped",
    }).output();

    assertEquals(result.code, 1);
    assertEquals(new TextDecoder().decode(result.stdout), "");
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("isolated test environment strips command-scope Git configuration", async () => {
  const hostile = {
    ...Deno.env.toObject(),
    GIT_CONFIG_COUNT: "1",
    GIT_CONFIG_KEY_0: "core.hooksPath",
    GIT_CONFIG_VALUE_0: "/hostile/hooks",
  };

  // Control: the probe must see the injection when nothing sanitizes it.
  const injected = await new Deno.Command("git", {
    args: ["config", "--get", "core.hooksPath"],
    env: hostile,
    stdout: "piped",
    stderr: "piped",
  }).output();
  assertEquals(injected.code, 0);
  assertEquals(new TextDecoder().decode(injected.stdout).trim(), "/hostile/hooks");

  const sanitized = await new Deno.Command("git", {
    args: ["config", "--get", "core.hooksPath"],
    // clearEnv: without it Deno merges the parent environment back in, and a
    // hostile variable inherited by the test process defeats the sanitized map.
    clearEnv: true,
    env: isolatedTestEnv(hostile),
    stdout: "piped",
    stderr: "piped",
  }).output();
  assertEquals(sanitized.code, 1);
  assertEquals(new TextDecoder().decode(sanitized.stdout), "");
});

Deno.test("isolated test environment strips GIT_CONFIG_PARAMETERS injection", async () => {
  const hostile = {
    ...Deno.env.toObject(),
    GIT_CONFIG_PARAMETERS: "'core.hooksPath=/hostile/hooks'",
  };

  // Control: the probe must see the injection when nothing sanitizes it.
  const injected = await new Deno.Command("git", {
    args: ["config", "--get", "core.hooksPath"],
    env: hostile,
    stdout: "piped",
    stderr: "piped",
  }).output();
  assertEquals(injected.code, 0);
  assertEquals(new TextDecoder().decode(injected.stdout).trim(), "/hostile/hooks");

  const sanitized = await new Deno.Command("git", {
    args: ["config", "--get", "core.hooksPath"],
    // clearEnv: without it Deno merges the parent environment back in, and a
    // hostile variable inherited by the test process defeats the sanitized map.
    clearEnv: true,
    env: isolatedTestEnv(hostile),
    stdout: "piped",
    stderr: "piped",
  }).output();
  assertEquals(sanitized.code, 1);
  assertEquals(new TextDecoder().decode(sanitized.stdout), "");
});

Deno.test("isolated test environment drops GIT_TEMPLATE_DIR hook seeding", async () => {
  const root = await Deno.makeTempDir({ prefix: "khive_git_template_" });
  try {
    const template = join(root, "template");
    await Deno.mkdir(join(template, "hooks"), { recursive: true });
    await Deno.writeTextFile(
      join(template, "hooks", "pre-commit"),
      "#!/bin/sh\nexit 0\n",
    );
    const hostile = { ...Deno.env.toObject(), GIT_TEMPLATE_DIR: template };

    // Control: an unsanitized init must seed the hook from the template.
    const seededRepo = join(root, "seeded");
    await new Deno.Command("git", {
      args: ["init", seededRepo],
      env: hostile,
      stdout: "null",
      stderr: "null",
    }).output();
    assertEquals(
      await Deno.stat(join(seededRepo, ".git", "hooks", "pre-commit"))
        .then(() => true, () => false),
      true,
    );

    const cleanRepo = join(root, "clean");
    await new Deno.Command("git", {
      args: ["init", cleanRepo],
      clearEnv: true,
      env: isolatedTestEnv(hostile),
      stdout: "null",
      stderr: "null",
    }).output();
    assertEquals(
      await Deno.stat(join(cleanRepo, ".git", "hooks", "pre-commit"))
        .then(() => true, () => false),
      false,
    );
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});
