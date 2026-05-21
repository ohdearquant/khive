/**
 * Tests for validateRulesFile() — ADR-056 §8 schema validation.
 *
 * These tests cover the blocker (exit code 2) and the closed-taxonomy
 * high-severity findings from codex round-1 review of PR #134.
 */

import { assertEquals, assertStringIncludes } from "@std/assert";
import { validateRulesFile } from "./rules.ts";

// ─── Top-level structure ──────────────────────────────────────────────────────

Deno.test("validateRulesFile — null / empty is valid", () => {
  assertEquals(validateRulesFile(null), []);
  assertEquals(validateRulesFile(undefined), []);
  assertEquals(validateRulesFile({}), []);
});

Deno.test("validateRulesFile — scalar root is invalid", () => {
  const errors = validateRulesFile("not-a-map");
  assertEquals(errors.length, 1);
  assertStringIncludes(errors[0].message, "mapping");
});

Deno.test("validateRulesFile — array root is invalid", () => {
  const errors = validateRulesFile([]);
  assertEquals(errors.length, 1);
  assertStringIncludes(errors[0].message, "mapping");
});

Deno.test("validateRulesFile — unknown top-level key is rejected", () => {
  const errors = validateRulesFile({ rules: {}, unknown_key: true });
  assertEquals(errors.length, 1);
  assertStringIncludes(errors[0].message, "unknown top-level key 'unknown_key'");
});

// ─── Severity validation ──────────────────────────────────────────────────────

Deno.test("validateRulesFile — invalid severity 'fatal' is rejected (blocker repro)", () => {
  // This is the exact case that caused exit=0 in codex's blocker repro.
  const parsed = {
    rules: {
      "no-self-loops": { severity: "fatal", enabled: true },
    },
  };
  const errors = validateRulesFile(parsed);
  assertEquals(errors.length, 1);
  assertStringIncludes(errors[0].message, "invalid severity 'fatal'");
  assertStringIncludes(errors[0].message, "error|warning|info");
});

Deno.test("validateRulesFile — valid severities are accepted", () => {
  for (const sev of ["error", "warning", "info"]) {
    const errors = validateRulesFile({
      rules: { "no-self-loops": { severity: sev } },
    });
    assertEquals(errors, [], `severity '${sev}' should be valid`);
  }
});

// ─── enabled type ─────────────────────────────────────────────────────────────

Deno.test("validateRulesFile — non-boolean 'enabled' is rejected", () => {
  const errors = validateRulesFile({
    rules: { "no-self-loops": { enabled: "yes" } },
  });
  assertEquals(errors.length, 1);
  assertStringIncludes(errors[0].message, "'enabled' must be a boolean");
});

// ─── Unknown entry keys ───────────────────────────────────────────────────────

Deno.test("validateRulesFile — unknown entry-level key is rejected", () => {
  const errors = validateRulesFile({
    rules: { "no-self-loops": { severity: "error", typo_key: true } },
  });
  assertEquals(errors.length, 1);
  assertStringIncludes(errors[0].message, "unknown entry key 'typo_key'");
});

// ─── module key (Phase E2 deferred) ──────────────────────────────────────────

Deno.test("validateRulesFile — 'module' key rejected as Phase E2 (deferred)", () => {
  const errors = validateRulesFile({
    rules: {
      "my-custom-rule": {
        severity: "error",
        enabled: true,
        module: "rules/my-rule.ts",
      },
    },
  });
  // Should surface as a schema error — not a KG violation.
  assertEquals(errors.length >= 1, true);
  const moduleErr = errors.find((e) => e.message.includes("module"));
  if (!moduleErr) throw new Error("expected 'module' schema error");
  assertStringIncludes(moduleErr.message, "Phase E2");
});

// ─── Unknown config keys for built-in rules ───────────────────────────────────

Deno.test("validateRulesFile — unknown config key for no-orphan-entities is rejected", () => {
  const errors = validateRulesFile({
    rules: {
      "no-orphan-entities": {
        severity: "warning",
        config: { min_edges_per_node: 2 }, // typo: correct key is min_edges
      },
    },
  });
  assertEquals(errors.length >= 1, true);
  const cfgErr = errors.find((e) => e.message.includes("min_edges_per_node"));
  if (!cfgErr) throw new Error("expected unknown config key error");
  assertStringIncludes(cfgErr.message, "unknown config key");
});

Deno.test("validateRulesFile — unknown config key for min-edge-density is rejected", () => {
  const errors = validateRulesFile({
    rules: {
      "min-edge-density": {
        config: { min_edges_per_entity: 3, bad_key: true },
      },
    },
  });
  const cfgErr = errors.find((e) => e.message.includes("bad_key"));
  if (!cfgErr) throw new Error("expected unknown config key error");
  assertStringIncludes(cfgErr.message, "unknown config key 'bad_key'");
});

Deno.test("validateRulesFile — valid config keys are accepted", () => {
  const errors = validateRulesFile({
    rules: {
      "min-edge-density": {
        config: { min_edges_per_entity: 3, exclude_kinds: ["person"] },
      },
    },
  });
  assertEquals(errors, []);
});

// ─── Closed-taxonomy enforcement (entity kinds) ───────────────────────────────

Deno.test("validateRulesFile — required-properties config with non-ADR-001 kind is rejected", () => {
  // 'paper' and 'model' are not in ADR-001 — must produce schema error.
  const errors = validateRulesFile({
    rules: {
      "required-properties": {
        severity: "error",
        config: {
          concept: ["description"],
          paper: ["title"], // invalid kind
        },
      },
    },
  });
  const kindErr = errors.find((e) => e.message.includes("paper"));
  if (!kindErr) throw new Error("expected invalid entity kind error for 'paper'");
  assertStringIncludes(kindErr.message, "not a valid entity kind");
  assertStringIncludes(kindErr.message, "ADR-001");
});

Deno.test("validateRulesFile — required-properties config with all ADR-001 kinds is valid", () => {
  const errors = validateRulesFile({
    rules: {
      "required-properties": {
        config: {
          concept: ["description"],
          document: ["title"],
          dataset: ["source"],
          project: ["repo"],
          person: ["affiliation"],
          org: ["website"],
        },
      },
    },
  });
  assertEquals(errors, []);
});

Deno.test("validateRulesFile — min-edge-density exclude_kinds with invalid kind is rejected", () => {
  const errors = validateRulesFile({
    rules: {
      "min-edge-density": {
        config: {
          min_edges_per_entity: 3,
          exclude_kinds: ["person", "model"], // 'model' is not in ADR-001
        },
      },
    },
  });
  const kindErr = errors.find((e) => e.message.includes("model"));
  if (!kindErr) throw new Error("expected invalid entity kind error for 'model'");
  assertStringIncludes(kindErr.message, "not a valid entity kind");
});

Deno.test("validateRulesFile — min-edge-density exclude_kinds with all valid kinds is accepted", () => {
  const errors = validateRulesFile({
    rules: {
      "min-edge-density": {
        config: { min_edges_per_entity: 2, exclude_kinds: ["person", "org"] },
      },
    },
  });
  assertEquals(errors, []);
});

Deno.test("validateRulesFile — min-edge-density exclude_kinds must be an array", () => {
  const errors = validateRulesFile({
    rules: {
      "min-edge-density": {
        config: { exclude_kinds: "person" }, // string instead of array
      },
    },
  });
  assertEquals(errors.length >= 1, true);
  const arrErr = errors.find((e) => e.message.includes("exclude_kinds"));
  if (!arrErr) throw new Error("expected array type error for exclude_kinds");
});
