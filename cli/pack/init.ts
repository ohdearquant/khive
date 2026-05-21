/**
 * `khive pack init` — scaffold a new declarative pack (ADR-050).
 *
 * Creates a `pack.yaml` template in the current directory with all required
 * fields and commented examples (ADR-050 §4 Authoring). Refuses to overwrite
 * an existing `pack.yaml`.
 */

export async function runPackInit(args: string[]): Promise<number> {
  if (args[0] === "--help" || args[0] === "-h") {
    console.log(`Usage: khive pack init

Creates a pack.yaml template in the current directory with all required fields
and commented examples. Includes the base entity kinds (ADR-001) and edge
relations (ADR-002) as comments to guide the author.

Run 'khive pack check pack.yaml' to validate the result.`);
    return 0;
  }

  const manifestPath = "pack.yaml";

  try {
    await Deno.stat(manifestPath);
    console.error(
      `'${manifestPath}' already exists. Refusing to overwrite.`,
    );
    return 1;
  } catch (err) {
    if (!(err instanceof Deno.errors.NotFound)) throw err;
  }

  const manifest = `# pack.yaml — declarative pack manifest (ADR-050)
#
# Edit this file to declare the vocabulary your pack contributes.
# Run 'khive pack check pack.yaml' to validate before installing.

name: my-pack
version: "0.1.0"
description: "TODO: describe what this pack contributes"
license: Apache-2.0

# entity_kinds: kinds this pack adds beyond the ADR-001 base set.
# Base kinds (do not re-declare): concept, document, dataset, project, person, org
# Use lowercase letters, digits, and underscores; must match ^[a-z][a-z0-9_]{0,62}$.
entity_kinds: []

# note_kinds: optional note kinds for this pack.
# Base kinds (do not re-declare): observation, insight, question, decision, reference
note_kinds: []

# edge_endpoints: new (source_kind, target_kind) pairs for existing relations.
# 'relation' must be one of the 13 ADR-002 base relations — packs cannot
# introduce new relation names.
# Base relations: contains, part_of, instance_of, extends, variant_of,
#   introduced_by, supersedes, depends_on, enables, implements,
#   competes_with, composed_with, annotates
edge_endpoints: []

# properties: per-kind property schemas. 'values' constrains to an enum;
# omit it to accept any string.
properties: {}
`;

  await Deno.writeTextFile(manifestPath, manifest);

  console.log(`Created ${manifestPath}`);
  console.log(`Edit ${manifestPath}, then run:`);
  console.log(`  khive pack check pack.yaml`);
  return 0;
}
