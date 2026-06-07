/**
 * `khive pack check <path>` — validate a declarative pack manifest (ADR-050).
 *
 * Reads pack.yaml from <path> (file or directory containing one) and prints a
 * validation report. Returns 0 on valid, 1 on errors. Warnings are printed
 * but do not change the exit code.
 */

import { loadAndValidatePack } from "../lib/pack.ts";

function printHelp(): void {
  console.log(`Usage: khive pack check <path>

Validate a declarative pack manifest against ADR-050 (pack format spec) and
ADR-001/002 (closed entity-kind and edge-relation taxonomies).

<path>   Path to a pack.yaml file OR a directory containing one.

Exit codes:
  0  manifest is valid (warnings may still be printed)
  1  manifest has structural errors`);
}

export async function runPackCheck(args: string[]): Promise<number> {
  if (args.length === 0 || args[0] === "--help" || args[0] === "-h") {
    printHelp();
    return args.length === 0 ? 1 : 0;
  }
  const targetPath = args[0];
  const res = await loadAndValidatePack(targetPath);

  if (res.errors.length > 0) {
    console.error(`Validation: fail — ${res.errors.length} error(s)`);
    for (const e of res.errors) {
      const where = e.path ? e.path : "(top-level)";
      console.error(`  ERROR  ${where}  ${e.message}`);
    }
  }
  if (res.warnings.length > 0) {
    console.warn(`  ${res.warnings.length} warning(s)`);
    for (const w of res.warnings) {
      const where = w.path ? w.path : "(top-level)";
      console.warn(`  WARN   ${where}  ${w.message}`);
    }
  }
  if (res.valid && res.manifest) {
    console.log(`Validation: pass`);
    console.log(`  name:    ${res.manifest.name}`);
    console.log(`  version: ${res.manifest.version}`);
    if (res.manifest.entity_kinds && res.manifest.entity_kinds.length > 0) {
      console.log(`  entity_kinds: ${res.manifest.entity_kinds.join(", ")}`);
    }
    if (res.manifest.note_kinds && res.manifest.note_kinds.length > 0) {
      console.log(`  note_kinds:   ${res.manifest.note_kinds.join(", ")}`);
    }
    if (res.manifest.edge_endpoints && res.manifest.edge_endpoints.length > 0) {
      const relations = res.manifest.edge_endpoints.map((e) => e.relation).join(", ");
      console.log(`  edge_endpoints: ${relations}`);
    }
  }

  return res.valid ? 0 : 1;
}
