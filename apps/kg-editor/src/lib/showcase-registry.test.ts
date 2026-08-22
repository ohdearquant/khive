import { describe, expect, it } from "vitest";

import {
  isAllowedShowcaseAsset,
  normalizeRepositoryUrl,
  resolveShowcaseRepository,
} from "@/lib/showcase-registry";

describe("showcase registry", () => {
  it.each([
    "https://github.com/ohdearquant/khive",
    "https://github.com/ohdearquant/khive/",
    "https://github.com/ohdearquant/khive.git",
    "http://github.com/ohdearquant/khive.git",
    "https://www.github.com/ohdearquant/khive?tab=readme#readme",
  ])("resolves a curated alias without fetching the submitted URL: %s", (input) => {
    const result = resolveShowcaseRepository(input);

    expect(result.status).toBe("hit");
    if (result.status === "hit") {
      expect(result.entry.assetPath).toBe("/showcase/khive-repo-v1-khive.json");
      expect(result.normalizedUrl).toBe("https://github.com/ohdearquant/khive");
    }
  });

  it("distinguishes a valid URL outside the curated set from invalid input", () => {
    expect(resolveShowcaseRepository("https://github.com/example/not-curated")).toEqual({
      status: "miss",
      normalizedUrl: "https://github.com/example/not-curated",
    });
    expect(resolveShowcaseRepository("https://github.com:444/ohdearquant/khive")).toMatchObject({
      status: "miss",
    });
    expect(resolveShowcaseRepository("github.com/example/repo")).toEqual({
      status: "invalid",
      reason: "Enter a complete http or https repository URL.",
    });
  });

  it("rejects credentials, non-http protocols, and path traversal", () => {
    expect(normalizeRepositoryUrl("https://token@github.com/example/repo").ok).toBe(false);
    expect(normalizeRepositoryUrl("file:///tmp/repo").ok).toBe(false);
    expect(normalizeRepositoryUrl("https://github.com/example/%2E%2E/repo").ok).toBe(false);
  });

  // These six fixtures are the individual character classes a prior,
  // narrower attempt rejected one at a time by enumerating characters.
  // They are kept here as named regression fixtures to demonstrate that
  // the round-trip invariant in normalizeRepositoryUrl (build the
  // canonical value, re-parse it, and require the segments to come back
  // unchanged) subsumes that whole class of input without enumerating a
  // single character. The credentials case is the one exception: it is
  // refused by the pre-existing `url.username || url.password` check,
  // which runs before the invariant is ever reached — its test says so.
  describe("subsumes the individual structural-character classes", () => {
    it("refuses a decoded slash inside a path segment (invariant: segment count changes on re-parse)", () => {
      const result = normalizeRepositoryUrl("https://forge.example/owner/%2Fsecret");
      expect(result.ok).toBe(false);
    });

    it("refuses a single-encoded query delimiter (invariant: re-parse splits off a query string)", () => {
      const result = normalizeRepositoryUrl("https://github.com/owner/repo%3Faccess_token");
      expect(result.ok).toBe(false);
    });

    it("refuses a double-encoded query delimiter (invariant: re-parse decodes an extra layer)", () => {
      const result = normalizeRepositoryUrl(
        "https://github.com/owner/repo%253Faccess_token%253DSECRET",
      );
      expect(result.ok).toBe(false);
    });

    it("refuses a triple-encoded query delimiter (invariant: re-parse decodes an extra layer)", () => {
      const result = normalizeRepositoryUrl(
        "https://github.com/owner/repo%25253Faccess_token%25253DSECRET",
      );
      expect(result.ok).toBe(false);
    });

    it("refuses a decoded backslash (invariant: special-scheme URLs treat \\ as a path separator, so segment count changes on re-parse)", () => {
      const result = normalizeRepositoryUrl("https://github.com/owner/repo%5Cevil");
      expect(result.ok).toBe(false);
    });

    it("refuses credentials in the authority — via the pre-existing username/password check, NOT the round-trip invariant", () => {
      const result = normalizeRepositoryUrl(
        "https://user%40name:SECRET@github.com/owner/repo",
      );
      expect(result.ok).toBe(false);
      if (!result.ok) {
        expect(result.reason).toBe("Repository URLs cannot contain credentials.");
      }
    });

    it("refuses a literal percent in a decoded repository name as deliberate policy, not a structural-character rejection", () => {
      // This is NOT one of the six structural-character classes above: "%"
      // followed by two hex digits decodes cleanly, so it never trips the
      // re-parse round-trip on its own. It is refused because the fixed-point
      // check below cannot be satisfied while the join stays unencoded — see
      // the comment at the join site in showcase-registry.ts.
      const result = normalizeRepositoryUrl("https://forge.example/owner/repo%25");
      expect(result.ok).toBe(false);
    });
  });

  it("refuses a doubled repository-name suffix that strips to a segment-level round-trip match but is not a fixed point of normalization", () => {
    // normalizeRepositoryUrl("https://forge.example/owner/repo.git") is ok
    // with value ".../owner/repo" — a single ".git" strip is itself a fixed
    // point. Stripping a SECOND ".git" is not: it would land on
    // ".../owner/repo.git", and normalizing that again strips again to
    // ".../owner/repo". The runtime fixed-point check catches that and
    // refuses the input outright, rather than accepting a non-canonical value.
    // The reason is asserted, not just the refusal: this input's encoding is
    // valid, so it must NOT report an encoding error. Without pinning the
    // string, the message could revert to the shared encoding reason and no
    // test would notice.
    const result = normalizeRepositoryUrl("https://forge.example/owner/repo.git.git");
    expect(result).toEqual({
      ok: false,
      reason: "The repository URL cannot be normalized to a stable canonical URL.",
    });
  });

  it("is a fixed point for every value produced by the curated-alias fixtures", () => {
    const acceptFixtures = [
      "https://github.com/ohdearquant/khive",
      "https://github.com/ohdearquant/khive/",
      "https://github.com/ohdearquant/khive.git",
      "http://github.com/ohdearquant/khive.git",
      "https://www.github.com/ohdearquant/khive?tab=readme#readme",
    ];
    for (const input of acceptFixtures) {
      const once = normalizeRepositoryUrl(input);
      expect(once.ok).toBe(true);
      if (!once.ok) continue;
      const twice = normalizeRepositoryUrl(once.value);
      expect(twice).toEqual(once);
    }
  });

  it("is idempotent: normalizing a canonical value again returns the same value", () => {
    const inputs = [
      "https://github.com/ohdearquant/khive",
      "https://github.com/ohdearquant/khive.git",
      "https://www.github.com/ohdearquant/khive?tab=readme#readme",
      "https://forge.example/group/sub-group/repo",
    ];
    for (const input of inputs) {
      const once = normalizeRepositoryUrl(input);
      expect(once.ok).toBe(true);
      if (!once.ok) continue;
      const twice = normalizeRepositoryUrl(once.value);
      expect(twice).toEqual(once);
    }
  });

  // A seeded linear congruential generator: no property-testing package
  // (e.g. fast-check) is in this package's devDependencies, so this is a
  // small deterministic PRNG local to this test file, seeded for
  // reproducibility across runs.
  describe("round-trip idempotence over generated inputs", () => {
    function makeRng(seed: number) {
      let state = seed >>> 0;
      return () => {
        state = (state * 1_664_525 + 1_013_904_223) >>> 0;
        return state / 0xffffffff;
      };
    }

    const STRUCTURAL_CHARS = ["%", "/", "?", "#", "\\", "@", ".", "é", "漢", "🙂"];
    const SAFE_CHARS = "abcdefghijklmnopqrstuvwxyz0123456789-_";
    const HOSTS = ["github.com", "forge.example", "www.github.com", "example.co.uk"];

    function randomSegmentContent(rng: () => number): string {
      const length = 1 + Math.floor(rng() * 6);
      let out = "";
      for (let i = 0; i < length; i++) {
        if (rng() < 0.35) {
          out += STRUCTURAL_CHARS[Math.floor(rng() * STRUCTURAL_CHARS.length)];
        } else {
          out += SAFE_CHARS[Math.floor(rng() * SAFE_CHARS.length)];
        }
      }
      return out;
    }

    // Forced onto the terminal segment: the character-by-character sampler
    // above will essentially never assemble the exact literal ".git" run
    // needed to exercise the doubled-suffix fixed-point defect (Fix 1), so
    // without this the generator's property arm could never reach that
    // class of input no matter how many iterations it runs.
    const TERMINAL_GIT_SUFFIXES = [".git", ".git.git"];

    function randomCandidate(rng: () => number): { url: string; hasTerminalGitSuffix: boolean } {
      const host = HOSTS[Math.floor(rng() * HOSTS.length)];
      const segmentCount = 2 + Math.floor(rng() * 3);
      const segments: string[] = [];
      let hasTerminalGitSuffix = false;
      for (let i = 0; i < segmentCount; i++) {
        let raw = randomSegmentContent(rng);
        if (i === segmentCount - 1 && rng() < 0.3) {
          if (rng() < 0.34) {
            raw = ".git";
          } else {
            const suffix = TERMINAL_GIT_SUFFIXES[Math.floor(rng() * TERMINAL_GIT_SUFFIXES.length)];
            raw = `${raw}${suffix}`;
          }
          hasTerminalGitSuffix = true;
        }
        // encodeURIComponent so the generated segment is well-formed
        // percent-encoding input (the whole point is to exercise
        // decode/re-encode boundaries, not to feed the parser garbage
        // it would reject before it ever reaches the invariant).
        segments.push(encodeURIComponent(raw));
      }
      return { url: `https://${host}/${segments.join("/")}`, hasTerminalGitSuffix };
    }

    it("either refuses the input, or normalizing twice equals normalizing once — for 500 generated inputs", () => {
      const rng = makeRng(0xc0ffee);
      let refused = 0;
      let accepted = 0;
      let sawTerminalGitSuffix = false;
      for (let i = 0; i < 500; i++) {
        const { url: candidate, hasTerminalGitSuffix } = randomCandidate(rng);
        if (hasTerminalGitSuffix) sawTerminalGitSuffix = true;
        const once = normalizeRepositoryUrl(candidate);
        if (!once.ok) {
          refused++;
          continue;
        }
        accepted++;
        const twice = normalizeRepositoryUrl(once.value);
        expect(twice).toEqual(once);
      }
      // Sanity check that the generator actually exercises both branches
      // rather than trivially refusing or trivially accepting everything.
      expect(refused).toBeGreaterThan(0);
      expect(accepted).toBeGreaterThan(0);
      // Coverage check, not a correctness check: this asserts the
      // generator itself reaches the terminal-".git" shapes that Fix 1's
      // defect lived in, so a future edit to the generator that stops
      // producing them fails loudly here instead of silently weakening
      // this test's ability to catch that defect class again.
      expect(sawTerminalGitSuffix).toBe(true);
      console.log(`round-trip idempotence generator: refused=${refused} accepted=${accepted}`);
    });
  });

  it("allows only registry-owned same-origin static assets", () => {
    expect(isAllowedShowcaseAsset("/showcase/khive-repo-v1-khive.json")).toBe(true);
    expect(isAllowedShowcaseAsset("https://example.com/showcase.json")).toBe(false);
    expect(isAllowedShowcaseAsset("/showcase/../../secrets.json")).toBe(false);
    expect(isAllowedShowcaseAsset("/showcase/unknown.json")).toBe(false);
  });
});
