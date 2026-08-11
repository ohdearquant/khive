import assert from "node:assert/strict";
import test from "node:test";

import {
  alphaEquivalent,
  findLiteralColorViolations,
  parseCssColor,
} from "./design-token-contract.mjs";

test("literal-color lint rejects component CSS and Tailwind palette classes", () => {
  const violations = findLiteralColorViolations([
    {
      path: "src/app/card.css",
      content: ".card { color: #fff; background: rgb(1 2 3 / 50%); }",
    },
    {
      path: "src/components/card.tsx",
      content: '<div className="bg-red-500 text-[#ffffff]" />',
    },
  ]);

  assert.deepEqual(
    violations.map(({ path, line }) => [path, line]),
    [
      ["src/app/card.css", 1],
      ["src/app/card.css", 1],
      ["src/components/card.tsx", 1],
      ["src/components/card.tsx", 1],
    ],
  );
});

test("literal-color lint permits values only in the token layer", () => {
  assert.deepEqual(
    findLiteralColorViolations([
      {
        path: "src/app/tokens.css",
        content: ":root { --khive-color-surface-base: #171512; }",
      },
      {
        path: "src/app/card.css",
        content:
          ".card { color: var(--khive-color-text-primary); background: var(--khive-color-transparent); }",
      },
    ]),
    [],
  );
});

test("literal-color lint sees conditional and helper class strings", () => {
  const violations = findLiteralColorViolations([
    {
      path: "src/components/card.tsx",
      content:
        '<div className={active ? "border-amber-500" : cn("text-slate-400")} />',
    },
  ]);

  assert.deepEqual(
    violations.map(({ literal }) => literal),
    ["border-amber-500", "text-slate-400"],
  );
});

test("alpha-equivalent contrast is computed after compositing over a surface", () => {
  const surface = parseCssColor("#171512");
  const primary = parseCssColor("#f6f1e7");
  const secondary = parseCssColor("rgb(246 241 231 / 70%)");
  const muted = parseCssColor("rgb(246 241 231 / 0.5)");

  assert.ok(
    Math.abs(alphaEquivalent(surface, primary, secondary) - 0.7) < 1e-9,
  );
  assert.ok(Math.abs(alphaEquivalent(surface, primary, muted) - 0.5) < 1e-9);
});
