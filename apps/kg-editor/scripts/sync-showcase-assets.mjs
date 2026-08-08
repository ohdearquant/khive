import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const appRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const source = resolve(appRoot, "../../docs/schemas/examples/khive-repo-v1-khive.json");
const target = resolve(appRoot, "public/showcase/khive-repo-v1-khive.json");
const expected = await readFile(source);

if (process.argv.includes("--check")) {
  const actual = await readFile(target).catch(() => null);
  if (!actual || !actual.equals(expected)) {
    throw new Error("The browser showcase asset is not byte-identical to the canonical golden bundle.");
  }
} else {
  await mkdir(dirname(target), { recursive: true });
  await writeFile(target, expected);
}
