import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  alphaEquivalent,
  composite,
  contrastRatio,
  findLiteralColorViolations,
  parseCssColor,
} from "./design-token-contract.mjs";

const tokenCss = await readFile(
  new URL("../src/app/tokens.css", import.meta.url),
  "utf8",
);

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

test("literal-color lint rejects named CSS colors and JSX style colors", () => {
  const violations = findLiteralColorViolations([
    {
      path: "src/app/card.css",
      content:
        ".a { color: aliceblue; } .b { border-color: rebeccapurple; } .c { fill: yellowgreen; }",
    },
    {
      path: "src/components/card.tsx",
      content:
        '<div style={{ color: "red", backgroundColor: "#fff", fill: "rgb(1 2 3)", stroke: "hsl(0 0% 0%)" }}><svg fill={"goldenrod"} stroke="currentColor" /></div>',
    },
  ]);

  assert.deepEqual(
    violations.map(({ literal }) => literal),
    [
      "aliceblue",
      "rebeccapurple",
      "yellowgreen",
      "red",
      "#fff",
      "rgb(1 2 3)",
      "hsl(0 0% 0%)",
      "goldenrod",
    ],
  );
});

test("literal-color lint follows JSX color expressions without scanning prose", () => {
  const violations = findLiteralColorViolations([
    {
      path: "src/components/card.tsx",
      content: `
        // style={{ color: "red" }}
        const copy = 'color: "red"';
        export function Card({ active }) {
          return <>
            {/* fill={"red"} */}
            <p>color: "red"</p>
            <svg fill={active ? "red" : "blue"} stroke={active ? \`rebeccapurple\` : "currentColor"} />
            <div style={{ color: active ? "red" : "blue", "fill": active ? \`goldenrod\` : "var(--khive-color-accent)" }} />
          </>;
        }
      `,
    },
  ]);

  assert.deepEqual(
    violations.map(({ literal }) => literal),
    ["red", "blue", "rebeccapurple", "red", "blue", "goldenrod"],
  );
});

test("Tailwind color grammar rejects arbitrary values and extended prefixes", () => {
  const violations = findLiteralColorViolations([
    {
      path: "src/components/card.tsx",
      content: `
        <div className={active
          ? "bg-[red] text-[color:rebeccapurple] [border-color:#fff]"
          : "decoration-red-500 placeholder-blue-500 caret-green-500 accent-purple-500 divide-orange-500 border-x-red-500 border-x-2 text-sm"}
        />
      `,
    },
  ]);

  assert.deepEqual(
    violations.map(({ literal }) => literal),
    [
      "bg-[red]",
      "text-[color:rebeccapurple]",
      "[border-color:#fff]",
      "decoration-red-500",
      "placeholder-blue-500",
      "caret-green-500",
      "accent-purple-500",
      "divide-orange-500",
      "border-x-red-500",
    ],
  );
});

test("literal-color lint ignores selectors, comments, and string content", () => {
  assert.deepEqual(
    findLiteralColorViolations([
      {
        path: "src/app/card.css",
        content:
          '#ace, #bad { content: "red #fff"; color: var(--khive-color-text-primary); } /* color: blue; fill: #123; */',
      },
    ]),
    [],
  );
});

test("literal-color lint ignores named words in non-color CSS declarations", () => {
  assert.deepEqual(
    findLiteralColorViolations([
      {
        path: "src/app/card.css",
        content:
          ".card { animation-name: red; grid-area: blue; color: var(--khive-color-text-primary); }",
      },
    ]),
    [],
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

test("CSS color parsing clamps valid ranges and rejects non-numeric input", () => {
  assert.deepEqual(parseCssColor("rgb(2000 -4 300 / 2)"), {
    red: 255,
    green: 0,
    blue: 255,
    alpha: 1,
  });
  assert.throws(() => parseCssColor("rgb(nope 0 0)"), /invalid/u);
  assert.throws(() => parseCssColor("rgb(0 0 0 / nope)"), /invalid/u);
  assert.throws(() => parseCssColor("#nope"), /invalid|unsupported/u);
});

test("alpha-equivalent contrast requires opaque surface and primary tokens", () => {
  const opaque = parseCssColor("#ffffff");
  const translucent = parseCssColor("rgb(255 255 255 / 0.9)");
  assert.throws(
    () => alphaEquivalent(translucent, opaque, opaque),
    /surface.*opaque/u,
  );
  assert.throws(
    () => alphaEquivalent(parseCssColor("#000000"), translucent, opaque),
    /primary.*opaque/u,
  );
});

test("Tailwind color mappings cover the public token taxonomy from source", () => {
  const publicTokens = new Set(
    Array.from(
      tokenCss.matchAll(/(--khive-color-[a-z0-9-]+)\s*:/gu),
      (match) => match[1],
    ),
  );
  const theme = tokenCss.match(/@theme\s+inline\s*\{([\s\S]*?)\}/u)?.[1] ?? "";
  const mappedTokens = new Set(
    Array.from(
      theme.matchAll(
        /--color-[a-z0-9-]+\s*:\s*var\((--khive-color-[a-z0-9-]+)\)/gu,
      ),
      (match) => match[1],
    ),
  );

  assert.deepEqual([...mappedTokens].sort(), [...publicTokens].sort());
});

test("brand glyph has non-text contrast against its real surfaces in both themes", () => {
  const blocks = Array.from(tokenCss.matchAll(/([^{}]+)\{([^{}]*)\}/gu));
  for (const theme of ["dark", "light"]) {
    const tokens = {};
    for (const block of blocks) {
      const applies = theme === "dark"
        ? block[1].includes(":root") || block[1].includes('[data-theme="dark"]')
        : block[1].includes('[data-theme="light"]');
      if (!applies) continue;
      for (
        const declaration of block[2].matchAll(
          /(--khive-[\w-]+)\s*:\s*([^;]+);/gu,
        )
      ) {
        tokens[declaration[1]] = declaration[2].trim();
      }
    }
    const glyph = parseCssColor(tokens["--khive-color-brand-glyph"]);
    for (
      const backgroundName of [
        "--khive-color-text-primary",
        "--khive-color-surface-raised",
      ]
    ) {
      const background = parseCssColor(tokens[backgroundName]);
      assert.ok(
        contrastRatio(background, composite(background, glyph)) >= 3,
        `${theme} brand glyph must reach 3:1 against ${backgroundName}`,
      );
    }
  }
});
