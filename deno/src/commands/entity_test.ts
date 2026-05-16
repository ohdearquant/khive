// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Tests for the entity command argument-parsing logic and MCP result helpers.
 *
 * Pure unit tests — no live khive-mcp binary required.
 * The parse helpers below mirror the logic in entity.ts so we can verify
 * argument handling in isolation.
 */

import { assertEquals } from "@std/assert";

// ---------------------------------------------------------------------------
// Inline parse helpers (mirror entity.ts logic)
// ---------------------------------------------------------------------------

function parseEntityCreateFlags(args: string[]): {
  kind?: string;
  name?: string;
  description?: string;
  namespace?: string;
  tags?: string[];
  json: boolean;
  ok: boolean;
  error?: string;
} {
  let kind: string | undefined;
  let name: string | undefined;
  let description: string | undefined;
  let namespace: string | undefined;
  let tags: string[] | undefined;
  let json = false;

  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if ((a === "--kind" || a === "-k") && i + 1 < args.length) kind = args[++i];
    else if ((a === "--name" || a === "-n") && i + 1 < args.length) name = args[++i];
    else if ((a === "--description" || a === "-d") && i + 1 < args.length) {
      description = args[++i];
    } else if ((a === "--namespace" || a === "--ns") && i + 1 < args.length) {
      namespace = args[++i];
    } else if (a === "--tags" && i + 1 < args.length) {
      tags = args[++i].split(",").map((t) => t.trim());
    } else if (a === "--json") json = true;
  }

  if (!kind) return { json, ok: false, error: "--kind is required" };
  if (!name) return { json, ok: false, error: "--name is required" };
  return { kind, name, description, namespace, tags, json, ok: true };
}

function parseEntityGetArgs(
  args: string[],
): { id?: string; namespace?: string; ok: boolean } {
  let namespace: string | undefined;
  const positional: string[] = [];

  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if ((a === "--namespace" || a === "--ns") && i + 1 < args.length) {
      namespace = args[++i];
    } else if (!a.startsWith("-")) positional.push(a);
  }
  const id = positional[0];
  return { id, namespace, ok: !!id };
}

function parseEntityListArgs(args: string[]): {
  kind?: string;
  namespace?: string;
  limit: number;
} {
  let kind: string | undefined;
  let namespace: string | undefined;
  let limit = 50;

  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if ((a === "--kind" || a === "-k") && i + 1 < args.length) kind = args[++i];
    else if ((a === "--namespace" || a === "--ns") && i + 1 < args.length) {
      namespace = args[++i];
    } else if ((a === "--limit" || a === "-l") && i + 1 < args.length) {
      limit = Number(args[++i]);
    }
  }
  return { kind, namespace, limit };
}

function parseEntityDeleteArgs(args: string[]): {
  id?: string;
  hard: boolean;
  namespace?: string;
  ok: boolean;
} {
  let hard = false;
  let namespace: string | undefined;
  const positional: string[] = [];

  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === "--hard") hard = true;
    else if ((a === "--namespace" || a === "--ns") && i + 1 < args.length) {
      namespace = args[++i];
    } else if (!a.startsWith("-")) positional.push(a);
  }
  const id = positional[0];
  return { id, hard, namespace, ok: !!id };
}

// ---------------------------------------------------------------------------
// MCP result extractor (mirrors entity.ts)
// ---------------------------------------------------------------------------
function extractText(result: unknown): string {
  const r = result as {
    content?: Array<{ type: string; text?: string }>;
    isError?: boolean;
  };
  if (r.isError) {
    const errMsg = r.content?.find((c) => c.type === "text")?.text ?? "Unknown error";
    throw new Error(errMsg);
  }
  return r.content?.find((c) => c.type === "text")?.text ?? "";
}

// ---------------------------------------------------------------------------
// entity create tests
// ---------------------------------------------------------------------------

Deno.test("entity create: requires --kind", () => {
  const r = parseEntityCreateFlags(["--name", "MyEntity"]);
  assertEquals(r.ok, false);
  assertEquals(r.error, "--kind is required");
});

Deno.test("entity create: requires --name", () => {
  const r = parseEntityCreateFlags(["--kind", "concept"]);
  assertEquals(r.ok, false);
  assertEquals(r.error, "--name is required");
});

Deno.test("entity create: parses all flags", () => {
  const r = parseEntityCreateFlags([
    "--kind",
    "concept",
    "--name",
    "FlashAttention",
    "--description",
    "Fast attention algorithm",
    "--namespace",
    "papers",
    "--tags",
    "attention,ml",
    "--json",
  ]);
  assertEquals(r.ok, true);
  assertEquals(r.kind, "concept");
  assertEquals(r.name, "FlashAttention");
  assertEquals(r.description, "Fast attention algorithm");
  assertEquals(r.namespace, "papers");
  assertEquals(r.tags, ["attention", "ml"]);
  assertEquals(r.json, true);
});

Deno.test("entity create: tags split and trimmed", () => {
  const r = parseEntityCreateFlags([
    "--kind",
    "project",
    "--name",
    "PyTorch",
    "--tags",
    " ml , python ",
  ]);
  assertEquals(r.ok, true);
  assertEquals(r.tags, ["ml", "python"]);
});

// ---------------------------------------------------------------------------
// entity get tests
// ---------------------------------------------------------------------------

Deno.test("entity get: requires id", () => {
  const r = parseEntityGetArgs([]);
  assertEquals(r.ok, false);
});

Deno.test("entity get: parses id", () => {
  const r = parseEntityGetArgs(["abc12345"]);
  assertEquals(r.ok, true);
  assertEquals(r.id, "abc12345");
});

Deno.test("entity get: parses id and namespace", () => {
  const r = parseEntityGetArgs(["abc12345", "--namespace", "papers"]);
  assertEquals(r.ok, true);
  assertEquals(r.id, "abc12345");
  assertEquals(r.namespace, "papers");
});

// ---------------------------------------------------------------------------
// entity list tests
// ---------------------------------------------------------------------------

Deno.test("entity list: defaults to limit 50", () => {
  const r = parseEntityListArgs([]);
  assertEquals(r.limit, 50);
  assertEquals(r.kind, undefined);
});

Deno.test("entity list: parses kind and limit", () => {
  const r = parseEntityListArgs(["--kind", "concept", "--limit", "10"]);
  assertEquals(r.kind, "concept");
  assertEquals(r.limit, 10);
});

Deno.test("entity list: parses namespace", () => {
  const r = parseEntityListArgs(["--namespace", "papers"]);
  assertEquals(r.namespace, "papers");
});

// ---------------------------------------------------------------------------
// entity delete tests
// ---------------------------------------------------------------------------

Deno.test("entity delete: requires id", () => {
  const r = parseEntityDeleteArgs(["--hard"]);
  assertEquals(r.ok, false);
});

Deno.test("entity delete: soft by default", () => {
  const r = parseEntityDeleteArgs(["some-uuid"]);
  assertEquals(r.hard, false);
  assertEquals(r.id, "some-uuid");
  assertEquals(r.ok, true);
});

Deno.test("entity delete: --hard flag", () => {
  const r = parseEntityDeleteArgs(["some-uuid", "--hard"]);
  assertEquals(r.hard, true);
  assertEquals(r.ok, true);
});

Deno.test("entity delete: parses namespace", () => {
  const r = parseEntityDeleteArgs(["some-uuid", "--namespace", "ns1"]);
  assertEquals(r.namespace, "ns1");
});

// ---------------------------------------------------------------------------
// MCP result extraction tests
// ---------------------------------------------------------------------------

Deno.test("extractText: returns text from content array", () => {
  const result = { content: [{ type: "text", text: '{"id":"abc"}' }] };
  assertEquals(extractText(result), '{"id":"abc"}');
});

Deno.test("extractText: throws on isError result", () => {
  const result = {
    isError: true,
    content: [{ type: "text", text: "entity not found" }],
  };
  let threw = false;
  try {
    extractText(result);
  } catch (e) {
    threw = true;
    assertEquals((e as Error).message, "entity not found");
  }
  assertEquals(threw, true);
});

Deno.test("extractText: returns empty string for empty content", () => {
  const result = { content: [] };
  assertEquals(extractText(result), "");
});

Deno.test("extractText: falls through non-text content items", () => {
  const result = {
    content: [
      { type: "image", data: "base64..." },
      { type: "text", text: "hello" },
    ],
  };
  assertEquals(extractText(result), "hello");
});
