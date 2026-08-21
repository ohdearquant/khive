import { readFile } from "node:fs/promises";

import {
  alphaEquivalent,
  composite,
  contrastRatio,
  parseCssColor,
} from "./design-token-contract.mjs";

const css = await readFile(
  new URL("../src/app/tokens.css", import.meta.url),
  "utf8",
);

function declarations(body) {
  return Object.fromEntries(
    Array.from(body.matchAll(/(--khive-[\w-]+)\s*:\s*([^;]+);/gu), (match) => [
      match[1],
      match[2].trim(),
    ]),
  );
}

function themeTokens(theme) {
  const tokens = {};
  for (const block of css.matchAll(/([^{}]+)\{([^{}]*)\}/gu)) {
    const selector = block[1];
    const applies = theme === "dark"
      ? selector.includes(":root") || selector.includes('[data-theme="dark"]')
      : selector.includes('[data-theme="light"]');
    if (applies) Object.assign(tokens, declarations(block[2]));
  }
  return tokens;
}

const surfaces = [
  "--khive-color-surface-base",
  "--khive-color-surface-raised",
  "--khive-color-surface-overlay",
];
const floors = {
  "--khive-color-text-secondary": 0.7,
  "--khive-color-text-muted": 0.5,
};
let failed = false;

for (const theme of ["dark", "light"]) {
  const tokens = themeTokens(theme);
  const primaryValue = tokens["--khive-color-text-primary"];
  if (!primaryValue) {
    throw new Error(`${theme}: missing --khive-color-text-primary`);
  }
  const primary = parseCssColor(primaryValue);

  for (const surfaceName of surfaces) {
    const surfaceValue = tokens[surfaceName];
    if (!surfaceValue) throw new Error(`${theme}: missing ${surfaceName}`);
    const surface = parseCssColor(surfaceValue);
    for (const [textName, floor] of Object.entries(floors)) {
      const textValue = tokens[textName];
      if (!textValue) throw new Error(`${theme}: missing ${textName}`);
      const text = parseCssColor(textValue);
      const equivalent = alphaEquivalent(surface, primary, text);
      const ratio = contrastRatio(surface, composite(surface, text));
      console.log(
        `${theme} ${textName} on ${surfaceName}: alpha-equivalent=${
          equivalent.toFixed(3)
        } contrast=${ratio.toFixed(2)}:1`,
      );
      if (equivalent + Number.EPSILON < floor) {
        console.error(
          `${theme}: ${textName} falls below ${floor} on ${surfaceName}`,
        );
        failed = true;
      }
    }
  }
}

if (failed) process.exitCode = 1;
