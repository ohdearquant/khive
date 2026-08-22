import { describe, expect, it } from "vitest";

import type { ViewId } from "@/lib/repo-bundle";
import {
  investigationShareUrl,
  parseRepositoryLocation,
  REPOSITORY_VIEW_IDS,
  type RepositoryLocation,
  repositoryLocationUrl,
} from "@/lib/repository-location";

const repository = "https://github.com/ohdearquant/khive";
const snapshotSha = "0123456789abcdef0123456789abcdef01234567";

describe("repository investigation location", () => {
  it.each(REPOSITORY_VIEW_IDS)(
    "round-trips a shareable %s investigation",
    (view: ViewId) => {
      const location: RepositoryLocation = {
        repository,
        snapshotSha,
        modulePath: "crates/space & signals/src/lib.rs",
        view,
      };

      const url = repositoryLocationUrl(
        new URL("https://example.test/?utm_source=demo#analysis"),
        location,
      );
      const parsed = parseRepositoryLocation(url);

      expect(parsed.issues).toEqual([]);
      expect(parsed.location).toEqual(location);
      expect(url.searchParams.has("utm_source")).toBe(false);
      expect(url.hash).toBe("");
    },
  );

  it.each([
    [
      "duplicate repository",
      `repo=${encodeURIComponent(repository)}&repo=${
        encodeURIComponent(repository)
      }`,
      "repo",
    ],
    ["duplicate module", "module=crates%2Fa.rs&module=crates%2Fb.rs", "module"],
    ["malformed snapshot", "at=abc123", "at"],
    ["unknown view", "view=everything", "view"],
    ["absolute module path", "module=%2Fetc%2Fpasswd", "module"],
    ["parent traversal", "module=crates%2F..%2Fsecret.rs", "module"],
    ["empty path segment", "module=crates%2F%2Fsecret.rs", "module"],
    ["empty module", "module=", "module"],
    ["overlong module", `module=${"a".repeat(1025)}`, "module"],
  ])(
    "rejects %s without accepting the ambiguous value",
    (_name, search, parameter) => {
      const parsed = parseRepositoryLocation(
        new URL(`https://example.test/?${search}`),
      );

      expect(parsed.issues).toContainEqual(
        expect.objectContaining({ parameter }),
      );
      const property = parameter === "repo"
        ? "repository"
        : parameter === "at"
        ? "snapshotSha"
        : parameter === "module"
        ? "modulePath"
        : "view";
      expect(parsed.location[property]).toBeNull();
    },
  );

  it.each([
    ["query string", `${repository}?tab=readme`],
    ["fragment", `${repository}#readme`],
  ])(
    // normalizeRepositoryUrl now returns the CANONICAL value (no query, no
    // fragment) rather than the raw input — parseRepository stores that
    // canonical value on the parsed location. A repo value carrying its
    // own query string or fragment is still accepted, but the extras are
    // dropped from the stored/round-tripped value; they never reach a
    // rebuilt URL. This is an intended consequence of the round-trip
    // invariant, not a regression: the canonical value is what
    // `repositoryLocationUrl`/`investigationShareUrl` will emit anyway.
    "accepts a curated repository URL carrying a %s, but canonicalizes away the extras",
    (_name, repositoryWithExtras) => {
      const parsed = parseRepositoryLocation(
        new URL(
          `https://example.test/?repo=${encodeURIComponent(repositoryWithExtras)}`,
        ),
      );

      expect(parsed.issues).toEqual([]);
      expect(parsed.location.repository).toBe(repository);
    },
  );

  it("canonicalizes to only the closed location parameters in stable order, dropping every other query parameter", () => {
    const url = repositoryLocationUrl(
      new URL(
        "https://example.test/?view=scorecard&module=old.rs&at=old&repo=old&keep=1",
      ),
      {
        repository,
        snapshotSha,
        modulePath: "crates/khive-db/src/pool.rs",
        view: "dependency_topology",
      },
    );

    expect(url.search).toBe(
      `?repo=${
        encodeURIComponent(repository)
      }&at=${snapshotSha}&module=crates%2Fkhive-db%2Fsrc%2Fpool.rs&view=dependency_topology`,
    );
    expect(url.searchParams.has("keep")).toBe(false);
  });

  it("drops credential-bearing query parameters instead of preserving them", () => {
    const url = repositoryLocationUrl(
      new URL(
        "https://example.test/?access_token=super-secret&id_token=another-secret",
      ),
      {
        repository,
        snapshotSha,
        modulePath: null,
        view: "scorecard",
      },
    );

    expect(url.searchParams.has("access_token")).toBe(false);
    expect(url.searchParams.has("id_token")).toBe(false);
    expect(url.href).not.toContain("super-secret");
    expect(url.href).not.toContain("another-secret");
  });

  it("drops a fragment-borne credential instead of preserving it", () => {
    const url = repositoryLocationUrl(
      new URL(
        "https://example.test/#access_token=super-secret",
      ),
      {
        repository,
        snapshotSha,
        modulePath: null,
        view: "scorecard",
      },
    );

    expect(url.hash).toBe("");
    expect(url.href).not.toContain("super-secret");
  });

  it("share form carries only investigation parameters and drops the fragment", () => {
    const url = investigationShareUrl(
      new URL(
        "https://example.test/app?repo=old&access_token=not-a-real-secret&utm_source=x#token-fragment",
      ),
      {
        repository,
        snapshotSha,
        modulePath: "crates/khive-db/src/pool.rs",
        view: "dependency_topology",
      },
    );

    expect(url.pathname).toBe("/app");
    expect(url.hash).toBe("");
    expect(url.searchParams.get("access_token")).toBeNull();
    expect(url.searchParams.get("utm_source")).toBeNull();
    expect(url.search).toBe(
      `?repo=${
        encodeURIComponent(repository)
      }&at=${snapshotSha}&module=crates%2Fkhive-db%2Fsrc%2Fpool.rs&view=dependency_topology`,
    );
  });

  it("share form strips a repository value's own query and fragment", () => {
    const url = investigationShareUrl(new URL("https://example.test/app"), {
      repository:
        "https://forge.example/group/repo?access_token=not-a-real-secret#token-fragment",
      snapshotSha,
      modulePath: "crates/khive-db/src/pool.rs",
      view: "dependency_topology",
    });

    expect(url.searchParams.get("repo")).toBe(
      "https://forge.example/group/repo",
    );
    expect(url.href).not.toContain("access_token");
    expect(url.href).not.toContain("token-fragment");
  });

  it("share form omits a module path carrying a query or fragment delimiter", () => {
    for (
      const modulePath of [
        "src/x?access_token=not-a-real-secret",
        "src/x#token-fragment",
      ]
    ) {
      const url = investigationShareUrl(new URL("https://example.test/app"), {
        repository,
        snapshotSha,
        modulePath,
        view: "dependency_topology",
      });

      expect(url.searchParams.get("module")).toBeNull();
      expect(url.href).not.toContain("access_token");
      expect(url.href).not.toContain("token-fragment");
      expect(url.searchParams.get("repo")).toBe(repository);
      expect(url.searchParams.get("at")).toBe(snapshotSha);
    }
  });
});
