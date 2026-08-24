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
