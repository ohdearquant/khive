import {
  Bookmark,
  BookOpen,
  Building2,
  Circle,
  CircleHelp,
  Database,
  Eye,
  FileText,
  FolderKanban,
  Lightbulb,
  type LucideIcon,
  Package,
  ServerCog,
  Signpost,
  Sparkles,
  User,
} from "lucide-react";
import type { CSSProperties } from "react";

import {
  DERIVED_EDGE_MARK,
  type EdgeLegendEntry,
  edgeLegendFor,
  entityLegendFor,
  isNoteKind,
  type KindLegendEntry,
  noteLegendFor,
  type OntologyIconName,
} from "@/lib/ontology-legend";

const iconComponents = {
  lightbulb: Lightbulb,
  "file-text": FileText,
  database: Database,
  "folder-kanban": FolderKanban,
  user: User,
  building: Building2,
  package: Package,
  "server-cog": ServerCog,
  "book-open": BookOpen,
  eye: Eye,
  sparkles: Sparkles,
  "circle-help": CircleHelp,
  signpost: Signpost,
  bookmark: Bookmark,
  circle: Circle,
} satisfies Record<OntologyIconName, LucideIcon>;

type OntologyStyle = CSSProperties & {
  "--ontology-kind-hue"?: string;
  "--ontology-edge-hue"?: string;
};

function classes(...values: Array<string | false | undefined>): string {
  return values.filter(Boolean).join(" ");
}

export function kindHueStyle(entry: KindLegendEntry): OntologyStyle {
  return { "--ontology-kind-hue": entry.hue };
}

export function edgeHueStyle(entry: EdgeLegendEntry): OntologyStyle {
  return { "--ontology-edge-hue": entry.hue };
}

export function edgeDirectionMark(
  entry: EdgeLegendEntry,
  source: { x: number; y: number },
  target: { x: number; y: number },
): { x: number; y: number; transform: string } | null {
  if (!entry.directed || (source.x === target.x && source.y === target.y)) {
    return null;
  }
  const x = source.x + (target.x - source.x) * 0.82;
  const y = source.y + (target.y - source.y) * 0.82;
  const angle = Math.atan2(target.y - source.y, target.x - source.x) * 180 /
    Math.PI;
  return { x, y, transform: `rotate(${angle} ${x} ${y})` };
}

function KindMark({
  entry,
  rawKind,
  className,
  showLabel,
}: {
  entry: KindLegendEntry;
  rawKind: string;
  className?: string;
  showLabel: boolean;
}) {
  const Icon = iconComponents[entry.icon];
  return (
    <span
      aria-label={showLabel ? undefined : entry.label}
      className={classes("ontology-kind-mark", className)}
      data-kind={rawKind}
      style={kindHueStyle(entry)}
      title={entry.label === "Unsupported kind"
        ? `${entry.label}: ${rawKind}`
        : undefined}
    >
      <Icon aria-hidden="true" strokeWidth={1.5} />
      {showLabel && <span>{entry.label}</span>}
    </span>
  );
}

export function EntityKindMark({
  kind,
  className,
  showLabel = true,
}: {
  kind: string;
  className?: string;
  showLabel?: boolean;
}) {
  return (
    <KindMark
      entry={entityLegendFor(kind)}
      rawKind={kind}
      className={className}
      showLabel={showLabel}
    />
  );
}

export function NoteKindMark({
  kind,
  className,
  showLabel = true,
}: {
  kind: string;
  className?: string;
  showLabel?: boolean;
}) {
  return (
    <KindMark
      entry={noteLegendFor(kind)}
      rawKind={kind}
      className={className}
      showLabel={showLabel}
    />
  );
}

export function OntologyKindMark({
  kind,
  className,
  showLabel = true,
}: {
  kind: string;
  className?: string;
  showLabel?: boolean;
}) {
  return isNoteKind(kind)
    ? <NoteKindMark kind={kind} className={className} showLabel={showLabel} />
    : (
      <EntityKindMark
        kind={kind}
        className={className}
        showLabel={showLabel}
      />
    );
}

export function RelationMark({
  relation,
  className,
  showLabel = true,
}: {
  relation: string;
  className?: string;
  showLabel?: boolean;
}) {
  const entry = edgeLegendFor(relation);
  return (
    <span
      aria-label={showLabel ? undefined : entry.label}
      className={classes("ontology-relation-mark", className)}
      data-edge-family={entry.family}
      data-relation={relation}
      data-edge-treatment={entry.treatment}
      data-edge-variant={entry.variant}
      style={edgeHueStyle(entry)}
      title={entry.label === "Unsupported relation"
        ? `${entry.label}: ${relation}`
        : `${entry.family} · ${entry.label}`}
    >
      <b aria-hidden="true">{entry.glyph}</b>
      {showLabel && <span>{entry.label}</span>}
    </span>
  );
}

export function DerivedEdgeMark({
  className,
  label = DERIVED_EDGE_MARK.label,
}: {
  className?: string;
  label?: string;
}) {
  return (
    <span
      className={classes("ontology-derived-mark", className)}
      title={DERIVED_EDGE_MARK.geometry}
    >
      <b aria-hidden="true">{DERIVED_EDGE_MARK.glyph}</b>
      <span>{label}</span>
    </span>
  );
}

function unique(values: readonly string[]): string[] {
  return [...new Set(values)];
}

export function OntologyLegend({
  entityKinds = [],
  noteKinds = [],
  relations = [],
  includeDerived = false,
  className,
}: {
  entityKinds?: readonly string[];
  noteKinds?: readonly string[];
  relations?: readonly string[];
  includeDerived?: boolean;
  className?: string;
}) {
  return (
    <div
      className={classes("ontology-legend", className)}
      aria-label="Ontology legend"
    >
      {unique(entityKinds).map((kind) => (
        <EntityKindMark kind={kind} key={`entity-${kind}`} />
      ))}
      {unique(noteKinds).map((kind) => (
        <NoteKindMark kind={kind} key={`note-${kind}`} />
      ))}
      {unique(relations).map((relation) => (
        <RelationMark relation={relation} key={`relation-${relation}`} />
      ))}
      {includeDerived && <DerivedEdgeMark />}
    </div>
  );
}
