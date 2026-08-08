import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { loadStaticShowcaseBundle } from "@/lib/adapters/static-showcase-source";
import { parseRepoBundle } from "@/lib/repo-bundle";

vi.mock("@/lib/adapters/static-showcase-source", () => ({
  loadStaticShowcaseBundle: vi.fn(),
}));

import { Showcase } from "@/components/showcase/showcase";

const goldenPath = resolve(process.cwd(), "../../docs/schemas/examples/khive-repo-v1-khive.json");
const goldenText = readFileSync(goldenPath, "utf8");
const bundle = parseRepoBundle(JSON.parse(goldenText));
const mockedLoad = vi.mocked(loadStaticShowcaseBundle);

function topLevelObjectBytes(key: string, nextKey: string) {
  const marker = `,"${key}":`;
  const nextMarker = `,"${nextKey}":`;
  const start = goldenText.indexOf(marker);
  const end = goldenText.indexOf(nextMarker, start + marker.length);
  expect(start).toBeGreaterThanOrEqual(0);
  expect(end).toBeGreaterThan(start);
  return goldenText.slice(start + marker.length, end);
}

function sha256(value: string) {
  return createHash("sha256").update(value).digest("hex");
}

describe("static repository lookup", () => {
  beforeEach(() => {
    window.history.replaceState(null, "", "/");
    mockedLoad.mockResolvedValue(bundle);
  });

  it("normalizes a curated alias and performs no bundle load for a later miss", async () => {
    const user = userEvent.setup();
    const { container } = render(<Showcase />);

    await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());
    expect(mockedLoad).toHaveBeenCalledTimes(1);
    expect(container.querySelector(".repo-overview")).toHaveAttribute(
      "data-head-sha",
      "c2979d2443738a075e55a170c772d1dc86cf0f91",
    );

    const input = screen.getByLabelText("Public repository URL");
    await user.clear(input);
    await user.type(input, "http://github.com/ohdearquant/khive.git");
    await user.click(screen.getByRole("button", { name: bundle.capability.labels.lookup_action }));

    await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());
    expect(window.location.search).toContain(
      "repo=https%3A%2F%2Fgithub.com%2Fohdearquant%2Fkhive",
    );
    expect(mockedLoad).toHaveBeenCalledTimes(1);

    await user.clear(input);
    await user.type(input, "https://github.com/example/not-curated");
    await user.click(screen.getByRole("button", { name: bundle.capability.labels.lookup_action }));

    expect(await screen.findByText(bundle.capability.labels.miss_title)).toBeVisible();
    expect(screen.getByText(new RegExp(bundle.capability.labels.miss_body))).toBeVisible();
    expect(window.location.search).toBe("");
    expect(mockedLoad).toHaveBeenCalledTimes(1);
  }, 15_000);

  it("loads populated symbol pages from the pinned code snapshot", () => {
    expect(bundle.meta.snapshot.head_sha).toBe("c2979d2443738a075e55a170c772d1dc86cf0f91");
    expect(bundle.meta.snapshot.ingested_at).toBe("2026-08-07T18:00:00Z");

    const codeIngest = bundle.meta.ingest.code_ingest;
    expect(codeIngest.status).toBe("available");
    if (codeIngest.status !== "available") {
      throw new Error("the checked-in bundle must carry code-ingest provenance");
    }
    expect(codeIngest.value.l2?.source_revision).toBe(bundle.meta.snapshot.head_sha);

    const symbolPages = [
      bundle.graph.functions,
      bundle.graph.datatypes,
      bundle.graph.interfaces,
    ];
    for (const page of symbolPages) {
      expect(page.items.length).toBeGreaterThan(0);
      expect(page.total_count.status).toBe("available");
      expect(page.bound.max_items).toBe(2_000);
      expect(page.bound.order).toBe("module_path,name,symbol_id");
      expect(["complete", "truncated"]).toContain(page.disclosure.status);
    }

    const symbolIds = new Set(symbolPages.flatMap((page) => page.items.map((symbol) => symbol.id)));
    expect(bundle.graph.structure_edges.items.every(
      (edge) => !symbolIds.has(edge.source) && !symbolIds.has(edge.target),
    )).toBe(true);
  });

  it("preserves the aggregate and module-edge golden bytes", () => {
    const aggregates = topLevelObjectBytes("aggregates", "capability");
    expect(Buffer.byteLength(aggregates)).toBe(1_123_968);
    expect(sha256(aggregates)).toBe(
      "8248048bc08a01d215923a0f944c89c02b016252b73c650776dc969900f58d70",
    );

    const structureEdges = topLevelObjectBytes("structure_edges", "history_edges");
    expect(Buffer.byteLength(structureEdges)).toBe(521_608);
    expect(sha256(structureEdges)).toBe(
      "89d0b69efae3d9e2301eb9541473e561735413a16e001e6a512ecd0cc65cae92",
    );

    expect(bundle.capability.views.structure_graph.granularity).toBe("module_symbol_deferred");
    const symbolCount = bundle.aggregates.scorecard.fields.find(
      (field) => field.key === "symbol_count",
    );
    expect(symbolCount?.granularity).toBe("module_symbol_deferred");
    expect(symbolCount?.value).toEqual({
      status: "unavailable",
      reason: "symbol-tier ingest is deferred",
    });
  });
});
