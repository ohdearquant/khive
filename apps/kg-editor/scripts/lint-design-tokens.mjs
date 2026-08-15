import { readdir, readFile } from "node:fs/promises";
import path from "node:path";

import { findLiteralColorViolations } from "./design-token-contract.mjs";

const sourceRoot = new URL("../src/", import.meta.url);
const supported = new Set([".css", ".js", ".jsx", ".ts", ".tsx"]);

async function collect(directory, prefix = "src") {
  const sources = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const entryUrl = new URL(
      `${entry.name}${entry.isDirectory() ? "/" : ""}`,
      directory,
    );
    const entryPath = `${prefix}/${entry.name}`;
    if (entry.isDirectory()) {
      sources.push(...await collect(entryUrl, entryPath));
    } else if (supported.has(path.extname(entry.name))) {
      sources.push({
        path: entryPath,
        content: await readFile(entryUrl, "utf8"),
      });
    }
  }
  return sources;
}

const violations = findLiteralColorViolations(await collect(sourceRoot));
if (violations.length > 0) {
  for (const violation of violations) {
    console.error(
      `${violation.path}:${violation.line}: literal color ${violation.literal}`,
    );
  }
  console.error(
    `literal-color gate failed with ${violations.length} violation(s)`,
  );
  process.exitCode = 1;
} else {
  console.log("literal-color gate passed");
}
