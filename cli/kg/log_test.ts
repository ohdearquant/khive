/**
 * Tests for `khive kg log`.
 */

import { assertEquals } from "@std/assert";
import { computeLog } from "./log.ts";
import { exec } from "../lib/git.ts";

// ─── Test harness ─────────────────────────────────────────────────────────────

async function makeGitRepo(): Promise<string> {
  const dir = await Deno.makeTempDir({ prefix: "khive_log_test_" });
  await exec(["git", "-C", dir, "init"]);
  await exec(["git", "-C", dir, "config", "user.email", "test@test.com"]);
  await exec(["git", "-C", dir, "config", "user.name", "Test"]);
  return dir;
}

async function removeDir(path: string): Promise<void> {
  await Deno.remove(path, { recursive: true });
}

// ─── Tests ────────────────────────────────────────────────────────────────────

Deno.test("computeLog: no commits returns empty array", async () => {
  const dir = await makeGitRepo();
  try {
    const entries = await computeLog(dir, []);
    assertEquals(entries.length, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeLog: KG commit appears in output", async () => {
  const dir = await makeGitRepo();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    await Deno.writeTextFile(`${dir}/.khive/kg/entities.ndjson`, "");
    await exec(["git", "-C", dir, "add", "."]);
    await exec(["git", "-C", dir, "commit", "-m", "init kg"]);

    const entries = await computeLog(dir, []);
    assertEquals(entries.length, 1);
    assertEquals(entries[0].subject, "init kg");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeLog: non-KG commit does not appear", async () => {
  const dir = await makeGitRepo();
  try {
    await Deno.writeTextFile(`${dir}/readme.txt`, "hello");
    await exec(["git", "-C", dir, "add", "."]);
    await exec(["git", "-C", dir, "commit", "-m", "add readme"]);

    const entries = await computeLog(dir, []);
    assertEquals(entries.length, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeLog: only KG commits shown when mixed with non-KG", async () => {
  const dir = await makeGitRepo();
  try {
    // Commit 1: non-KG
    await Deno.writeTextFile(`${dir}/readme.txt`, "hello");
    await exec(["git", "-C", dir, "add", "."]);
    await exec(["git", "-C", dir, "commit", "-m", "readme"]);

    // Commit 2: KG
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    await Deno.writeTextFile(`${dir}/.khive/kg/entities.ndjson`, "");
    await exec(["git", "-C", dir, "add", "."]);
    await exec(["git", "-C", dir, "commit", "-m", "kg init"]);

    // Commit 3: non-KG
    await Deno.writeTextFile(`${dir}/readme.txt`, "world");
    await exec(["git", "-C", dir, "add", "."]);
    await exec(["git", "-C", dir, "commit", "-m", "update readme"]);

    const entries = await computeLog(dir, []);
    assertEquals(entries.length, 1);
    assertEquals(entries[0].subject, "kg init");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeLog: -n limit respected", async () => {
  const dir = await makeGitRepo();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    for (let i = 0; i < 5; i++) {
      await Deno.writeTextFile(
        `${dir}/.khive/kg/entities.ndjson`,
        `{"id":"commit-${i}"}\n`,
      );
      await exec(["git", "-C", dir, "add", "."]);
      await exec(["git", "-C", dir, "commit", `-m`, `commit-${i}`]);
    }
    const entries = await computeLog(dir, ["-n", "3"]);
    assertEquals(entries.length, 3);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeLog: --limit flag respected", async () => {
  const dir = await makeGitRepo();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    for (let i = 0; i < 4; i++) {
      await Deno.writeTextFile(
        `${dir}/.khive/kg/entities.ndjson`,
        `{"id":"commit-${i}"}\n`,
      );
      await exec(["git", "-C", dir, "add", "."]);
      await exec(["git", "-C", dir, "commit", "-m", `commit-${i}`]);
    }
    const entries = await computeLog(dir, ["--limit", "2"]);
    assertEquals(entries.length, 2);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeLog: entries have expected fields", async () => {
  const dir = await makeGitRepo();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    await Deno.writeTextFile(`${dir}/.khive/kg/entities.ndjson`, "");
    await exec(["git", "-C", dir, "add", "."]);
    await exec(["git", "-C", dir, "commit", "-m", "first kg commit"]);

    const entries = await computeLog(dir, []);
    assertEquals(entries.length, 1);
    const e = entries[0];
    assertEquals(typeof e.sha, "string");
    assertEquals(e.sha.length > 0, true);
    assertEquals(e.subject, "first kg commit");
    assertEquals(typeof e.author, "string");
    assertEquals(typeof e.date, "string");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeLog: returns array (json flag passthrough)", async () => {
  const dir = await makeGitRepo();
  try {
    const entries = await computeLog(dir, ["--json"]);
    assertEquals(Array.isArray(entries), true);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeLog: edges.ndjson changes are included", async () => {
  const dir = await makeGitRepo();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    await Deno.writeTextFile(`${dir}/.khive/kg/edges.ndjson`, "");
    await exec(["git", "-C", dir, "add", "."]);
    await exec(["git", "-C", dir, "commit", "-m", "edge commit"]);

    const entries = await computeLog(dir, []);
    assertEquals(entries.length, 1);
    assertEquals(entries[0].subject, "edge commit");
  } finally {
    await removeDir(dir);
  }
});
