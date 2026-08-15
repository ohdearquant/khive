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
  type IconComponent,
  Package,
  ServerCog,
  Signpost,
  Sparkles,
  User,
} from "@/icons";
import type { CSSProperties } from "react";

import {
  DERIVED_EDGE_MARK,
  EDGE_RELATIONS,
  type EdgeLegendEntry,
  edgeLegendFor,
  ENTITY_KINDS,
  entityLegendFor,
  isNoteKind,
  type KindLegendEntry,
  noteLegendFor,
  NOTE_KINDS,
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
} satisfies Record<OntologyIconName, IconComponent>;

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
  // Keep the direction cue in the open edge span. Both graph canvases draw
  // center-to-center beneath opaque node cards, so a cue near the target is
  // hidden on short edges even though the SVG marker is present.
  const x = source.x + (target.x - source.x) * 0.68;
  const y = source.y + (target.y - source.y) * 0.68;
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
      <Icon aria-hidden="true" />
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
      <svg
        aria-hidden="true"
        className="ontology-derived-glyph-icon"
        viewBox="0 0 24 24"
      >
        <polygon points="12,3 21,12 12,21 3,12" />
      </svg>
      <span>{label}</span>
    </span>
  );
}

function dimClass(present: Set<string> | null, value: string): string | undefined {
  return present && !present.has(value) ? "ontology-mark-dim" : undefined;
}

/**
 * Renders the complete closed ontology (9 entity kinds, 17 relations, 5 note
 * kinds, plus the derived-edge mark) per ADR-153 D1/D5 — the legend is a
 * permanent, complete on-canvas affordance, not a per-graph subset. Passing
 * `presentEntityKinds` / `presentNoteKinds` / `presentRelations` highlights
 * which identities occur in the current graph by dimming the rest; it never
 * removes an identity from the render.
 */
export function OntologyLegend({
  presentEntityKinds,
  presentNoteKinds,
  presentRelations,
  className,
}: {
  presentEntityKinds?: readonly string[];
  presentNoteKinds?: readonly string[];
  presentRelations?: readonly string[];
  className?: string;
}) {
  const presentEntitySet = presentEntityKinds
    ? new Set(presentEntityKinds)
    : null;
  const presentNoteSet = presentNoteKinds ? new Set(presentNoteKinds) : null;
  const presentRelationSet = presentRelations
    ? new Set(presentRelations)
    : null;

  return (
    <div
      className={classes("ontology-legend", className)}
      aria-label="Ontology legend"
    >
      {ENTITY_KINDS.map((kind) => (
        <EntityKindMark
          kind={kind}
          key={`entity-${kind}`}
          className={dimClass(presentEntitySet, kind)}
        />
      ))}
      {NOTE_KINDS.map((kind) => (
        <NoteKindMark
          kind={kind}
          key={`note-${kind}`}
          className={dimClass(presentNoteSet, kind)}
        />
      ))}
      {EDGE_RELATIONS.map((relation) => (
        <RelationMark
          relation={relation}
          key={`relation-${relation}`}
          className={dimClass(presentRelationSet, relation)}
        />
      ))}
      <DerivedEdgeMark />
    </div>
  );
}
