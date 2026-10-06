# Repair duplicate issue and pull-request notes

`kkernel git-dedup` previews, and on request applies, merges of duplicate `issue`
and `pull_request` notes under one live canonical project. It changes no ingest
behavior and adds no MCP verb.

```sh
kkernel git-dedup --project <full-project-uuid> --namespace local --db /path/to/khive.db
kkernel git-dedup --project <full-project-uuid> --namespace local --db /path/to/khive.db \
  --refuse-anchor <full-anchor-uuid> --apply
```

The default is a read-only plan. Normal runtime startup can still run schema
migrations. `--apply` plans again in the same process and applies that plan; a
saved report is never accepted as authorization. The plan summary goes to stderr
and the full JSON report to stdout.

## What counts as a duplicate

A repository anchor that was merged into a canonical project leaves its notes
pointing at the retired anchor through `properties.project_id`, because an entity
merge rewires edges, not note properties. A later ingest looks a note up by the
canonical id, misses it, and creates a second note with the same number.

A note is a candidate when its `project_id` is the canonical project or an anchor
whose retained `merged_into` chain reaches it, and a live `annotates` edge links
it to the canonical project. A note that also annotates another live project in
the namespace is left alone. Notes are grouped by kind and number; issues and
pull requests never join.

A group is merged only when every member carries the same non-blank
`properties.title` and the same stored `url`. A missing or differing title
refuses the group and the report prints each title. A `url` that is present on
one note and absent or blank on another is not proof of identity and refuses the
group too. A refused group changes nothing.

## Survivor, rename and body

The survivor is the note already homed at the canonical project, then a note that
has a name, then the one with the most properties, the longest body, the earliest
creation time and the lowest id. A survivor with no name takes its twin's name,
which is allowed only because number and title are equal. A survivor that still
points at a retired anchor takes the canonical project as its `project_id`; the
anchor it left is recorded in the merge history entry.

A donor body is appended after the survivor's, separated by a rule. A body the
survivor already holds, byte for byte, is not appended again.

## Refused anchors

`--refuse-anchor` (repeatable, full UUID) leaves every note whose `project_id`
is that anchor untouched: such a note is neither merged away nor chosen as a
survivor. Other notes in the same group still merge.

## Guarded merges

Every merge goes through the runtime's guarded note merge. It carries the exact
version of the survivor and the donor that the plan read, and read-only
assertions that run on the writer connection before the first write: each note's
anchor still reaches the canonical project through live `merged_into` records,
each note still has a live `annotates` edge to the canonical project, and each
note's annotated projects are all the canonical project. A store that changed
between planning and applying refuses that merge and writes nothing. A refusal
stops the rest of its group; other groups continue, and the report lists every
refusal.

The merge embeds the survivor the way ingest does, so its vector follows the
merged body, and the donor's vector rows are removed. The donor is tombstoned
with its original content and properties, and the normal edge rewiring and merge
event apply.

## Reading the plan

The report carries counts per kind that can be set against a stored-population
census:

- `candidates`, `unnamed` (notes with no stored name), `groups_found`,
  `groups_planned`, `groups_refused`, `notes_merged_away`, `survivors_renamed`
  and `notes_rehomed`.
- Every note lands in one outcome, so `unnamed` equals `unnamed_merged_away` +
  `unnamed_survivors` + `unnamed_in_refused_groups` + `unnamed_unchanged`.
- `unnamed_by_anchor` counts unnamed notes by their stored `project_id`, which
  compares directly with a census that groups by that property.
- `unchanged` counts notes left alone, by reason.

A repeated successful apply with no intervening change finds nothing to merge.
