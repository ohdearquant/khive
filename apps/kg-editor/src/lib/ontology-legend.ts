export const ENTITY_KINDS = [
  "concept",
  "document",
  "dataset",
  "project",
  "person",
  "org",
  "artifact",
  "service",
  "resource",
] as const;

export const GRAPH_LAYOUT_SEED = 0x4b48_4956;

export const EDGE_RELATIONS = [
  "contains",
  "part_of",
  "instance_of",
  "extends",
  "variant_of",
  "introduced_by",
  "supersedes",
  "derived_from",
  "precedes",
  "depends_on",
  "enables",
  "implements",
  "competes_with",
  "composed_with",
  "annotates",
  "supports",
  "refutes",
] as const;

export const NOTE_KINDS = [
  "observation",
  "insight",
  "question",
  "decision",
  "reference",
] as const;

export const EDGE_RELATION_FAMILY_NAMES = [
  "structure",
  "derivation",
  "provenance",
  "temporal",
  "dependency",
  "implementation",
  "lateral",
  "annotation",
  "epistemic",
] as const;

export type EntityKind = (typeof ENTITY_KINDS)[number];
export type EdgeRelation = (typeof EDGE_RELATIONS)[number];
export type NoteKind = (typeof NOTE_KINDS)[number];
export type EdgeRelationFamily = (typeof EDGE_RELATION_FAMILY_NAMES)[number];

export const EDGE_RELATION_FAMILIES = {
  structure: ["contains", "part_of", "instance_of"],
  derivation: ["extends", "variant_of", "introduced_by", "supersedes"],
  provenance: ["derived_from"],
  temporal: ["precedes"],
  dependency: ["depends_on", "enables"],
  implementation: ["implements"],
  lateral: ["competes_with", "composed_with"],
  annotation: ["annotates"],
  epistemic: ["supports", "refutes"],
} as const satisfies Record<EdgeRelationFamily, readonly EdgeRelation[]>;

export type OntologyIconName =
  | "lightbulb"
  | "file-text"
  | "database"
  | "folder-kanban"
  | "user"
  | "building"
  | "package"
  | "server-cog"
  | "book-open"
  | "eye"
  | "sparkles"
  | "circle-help"
  | "signpost"
  | "bookmark"
  | "circle";

export type KindLegendEntry = Readonly<{
  label: string;
  icon: OntologyIconName;
  hue: string;
}>;

export const ENTITY_KIND_LEGEND = {
  concept: {
    label: "Concept",
    icon: "lightbulb",
    hue: "var(--ontology-concept)",
  },
  document: {
    label: "Document",
    icon: "file-text",
    hue: "var(--ontology-document)",
  },
  dataset: {
    label: "Dataset",
    icon: "database",
    hue: "var(--ontology-dataset)",
  },
  project: {
    label: "Project",
    icon: "folder-kanban",
    hue: "var(--ontology-project)",
  },
  person: { label: "Person", icon: "user", hue: "var(--ontology-person)" },
  org: { label: "Organization", icon: "building", hue: "var(--ontology-org)" },
  artifact: {
    label: "Artifact",
    icon: "package",
    hue: "var(--ontology-artifact)",
  },
  service: {
    label: "Service",
    icon: "server-cog",
    hue: "var(--ontology-service)",
  },
  resource: {
    label: "Resource",
    icon: "book-open",
    hue: "var(--ontology-resource)",
  },
} as const satisfies Record<EntityKind, KindLegendEntry>;

export const NOTE_KIND_LEGEND = {
  observation: {
    label: "Observation",
    icon: "eye",
    hue: "var(--ontology-observation)",
  },
  insight: {
    label: "Insight",
    icon: "sparkles",
    hue: "var(--ontology-insight)",
  },
  question: {
    label: "Question",
    icon: "circle-help",
    hue: "var(--ontology-question)",
  },
  decision: {
    label: "Decision",
    icon: "signpost",
    hue: "var(--ontology-decision)",
  },
  reference: {
    label: "Reference",
    icon: "bookmark",
    hue: "var(--ontology-reference)",
  },
} as const satisfies Record<NoteKind, KindLegendEntry>;

export type EdgeLineTreatment =
  | "quiet-solid"
  | "directional"
  | "dotted"
  | "assertive-solid"
  | "undirected"
  | "recessive-dashed"
  | "epistemic";

export type EdgeLegendEntry = Readonly<{
  label: string;
  family: EdgeRelationFamily;
  glyph: string;
  treatment: EdgeLineTreatment;
  variant: "primary" | "secondary" | "tertiary" | "quaternary";
  hue: string;
  directed: boolean;
}>;

const neutralEdge = "var(--ontology-edge-neutral)";

export const EDGE_RELATION_LEGEND = {
  contains: {
    label: "Contains",
    family: "structure",
    glyph: "C",
    treatment: "quiet-solid",
    variant: "primary",
    hue: neutralEdge,
    directed: true,
  },
  part_of: {
    label: "Part of",
    family: "structure",
    glyph: "P",
    treatment: "quiet-solid",
    variant: "secondary",
    hue: neutralEdge,
    directed: true,
  },
  instance_of: {
    label: "Instance of",
    family: "structure",
    glyph: "I",
    treatment: "quiet-solid",
    variant: "tertiary",
    hue: neutralEdge,
    directed: true,
  },
  extends: {
    label: "Extends",
    family: "derivation",
    glyph: "E",
    treatment: "directional",
    variant: "primary",
    hue: neutralEdge,
    directed: true,
  },
  variant_of: {
    label: "Variant of",
    family: "derivation",
    glyph: "V",
    treatment: "directional",
    variant: "secondary",
    hue: neutralEdge,
    directed: true,
  },
  introduced_by: {
    label: "Introduced by",
    family: "derivation",
    glyph: "IB",
    treatment: "directional",
    variant: "tertiary",
    hue: neutralEdge,
    directed: true,
  },
  supersedes: {
    label: "Supersedes",
    family: "derivation",
    glyph: "S",
    treatment: "directional",
    variant: "quaternary",
    hue: neutralEdge,
    directed: true,
  },
  derived_from: {
    label: "Derived from",
    family: "provenance",
    glyph: "DF",
    treatment: "directional",
    variant: "primary",
    hue: neutralEdge,
    directed: true,
  },
  precedes: {
    label: "Precedes",
    family: "temporal",
    glyph: "T",
    treatment: "dotted",
    variant: "primary",
    hue: neutralEdge,
    directed: true,
  },
  depends_on: {
    label: "Depends on",
    family: "dependency",
    glyph: "DO",
    treatment: "assertive-solid",
    variant: "primary",
    hue: neutralEdge,
    directed: true,
  },
  enables: {
    label: "Enables",
    family: "dependency",
    glyph: "EN",
    treatment: "assertive-solid",
    variant: "secondary",
    hue: neutralEdge,
    directed: true,
  },
  implements: {
    label: "Implements",
    family: "implementation",
    glyph: "IM",
    treatment: "directional",
    variant: "primary",
    hue: neutralEdge,
    directed: true,
  },
  competes_with: {
    label: "Competes with",
    family: "lateral",
    glyph: "CW",
    treatment: "undirected",
    variant: "primary",
    hue: neutralEdge,
    directed: false,
  },
  composed_with: {
    label: "Composed with",
    family: "lateral",
    glyph: "CO",
    treatment: "undirected",
    variant: "secondary",
    hue: neutralEdge,
    directed: false,
  },
  annotates: {
    label: "Annotates",
    family: "annotation",
    glyph: "A",
    treatment: "recessive-dashed",
    variant: "primary",
    hue: neutralEdge,
    directed: true,
  },
  supports: {
    label: "Supports",
    family: "epistemic",
    glyph: "+",
    treatment: "epistemic",
    variant: "primary",
    hue: "var(--ontology-support)",
    directed: true,
  },
  refutes: {
    label: "Refutes",
    family: "epistemic",
    glyph: "−",
    treatment: "epistemic",
    variant: "secondary",
    hue: "var(--ontology-refute)",
    directed: true,
  },
} as const satisfies Record<EdgeRelation, EdgeLegendEntry>;

export const DERIVED_EDGE_MARK = {
  label: "Derived",
  glyph: "◇",
  geometry: "diamond",
  hue: "var(--ontology-derived)",
} as const;

const UNKNOWN_KIND_LEGEND: KindLegendEntry = {
  label: "Unsupported kind",
  icon: "circle",
  hue: "var(--ontology-unknown)",
};

const UNKNOWN_EDGE_LEGEND: EdgeLegendEntry = {
  label: "Unsupported relation",
  family: "annotation",
  glyph: "?",
  treatment: "recessive-dashed",
  variant: "primary",
  hue: "var(--ontology-unknown)",
  directed: true,
};

function hasOwn<T extends object>(value: T, key: PropertyKey): key is keyof T {
  return Object.prototype.hasOwnProperty.call(value, key);
}

export function isEntityKind(value: string): value is EntityKind {
  return hasOwn(ENTITY_KIND_LEGEND, value);
}

export function isNoteKind(value: string): value is NoteKind {
  return hasOwn(NOTE_KIND_LEGEND, value);
}

export function isEdgeRelation(value: string): value is EdgeRelation {
  return hasOwn(EDGE_RELATION_LEGEND, value);
}

export function entityLegendFor(kind: string): KindLegendEntry {
  return isEntityKind(kind) ? ENTITY_KIND_LEGEND[kind] : UNKNOWN_KIND_LEGEND;
}

export function noteLegendFor(kind: string): KindLegendEntry {
  return isNoteKind(kind) ? NOTE_KIND_LEGEND[kind] : UNKNOWN_KIND_LEGEND;
}

export function edgeLegendFor(relation: string): EdgeLegendEntry {
  return isEdgeRelation(relation)
    ? EDGE_RELATION_LEGEND[relation]
    : UNKNOWN_EDGE_LEGEND;
}
