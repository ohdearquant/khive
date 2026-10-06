//! Repair of duplicate issue and pull-request notes left under merged anchors.
//!
//! A repository anchor that was merged into a canonical project leaves its notes
//! pointing at the retired anchor through `properties.project_id`. A later
//! ingest looks notes up by the canonical id, misses them, and mints a second
//! note with the same number. This module finds those groups, plans one
//! survivor per group, and merges the rest into it through the runtime's
//! guarded note merge, so a store that changed after planning refuses instead
//! of merging.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Result};
use khive_runtime::curation::{
    ContentMergeStrategy, EntityDedupMergePolicy, MergeAssertion, MergeSummary, NoteMergeGuard,
};
use khive_runtime::{KhiveRuntime, NamespaceToken};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::EdgeRelation;
use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;

const MERGE_REASON: &str = "duplicate git note repair";

#[derive(Clone, Debug)]
pub struct DedupOptions {
    pub project_id: Uuid,
    /// Anchors whose notes are left untouched: they are neither merged away nor
    /// chosen as a survivor.
    pub refused_anchors: BTreeSet<Uuid>,
    pub apply: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyMerge {
    /// The donor's body is appended after the survivor's.
    Append,
    /// The survivor already holds this exact body.
    KeepSurvivor,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlannedNote {
    pub id: Uuid,
    pub version: i64,
    pub project_id: Uuid,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlannedDonor {
    #[serde(flatten)]
    pub note: PlannedNote,
    pub body: BodyMerge,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlannedGroup {
    pub kind: String,
    pub number: i64,
    pub title: String,
    pub survivor: PlannedNote,
    /// The name the survivor takes from a twin, when it has none of its own.
    pub renamed_to: Option<String>,
    /// The retired anchor the survivor leaves for the canonical project.
    pub rehomed_from: Option<Uuid>,
    pub donors: Vec<PlannedDonor>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RefusedNote {
    pub id: Uuid,
    pub project_id: Uuid,
    pub title: Option<String>,
    pub url: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RefusedGroup {
    pub kind: String,
    pub number: i64,
    pub reason: String,
    pub notes: Vec<RefusedNote>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UnchangedCount {
    pub kind: String,
    pub reason: String,
    pub notes: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct UnnamedByAnchor {
    pub kind: String,
    /// The note's stored `project_id`, or `none` when it carries no text value.
    pub project_id: String,
    pub unnamed_notes: usize,
}

/// Per-kind counts. Every candidate note lands in exactly one outcome, so
/// `unnamed` always equals the sum of the four `unnamed_*` outcomes.
#[derive(Clone, Debug, Default, Serialize)]
pub struct KindTotals {
    pub candidates: usize,
    pub unnamed: usize,
    pub groups_found: usize,
    pub groups_planned: usize,
    pub groups_refused: usize,
    pub notes_merged_away: usize,
    pub survivors_renamed: usize,
    pub notes_rehomed: usize,
    pub unnamed_merged_away: usize,
    pub unnamed_survivors: usize,
    pub unnamed_in_refused_groups: usize,
    pub unnamed_unchanged: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct AppliedMerge {
    pub into_id: Uuid,
    pub from_id: Uuid,
    pub kept_version: i64,
    pub summary: MergeSummary,
}

#[derive(Clone, Debug, Serialize)]
pub struct RefusedMerge {
    pub into_id: Uuid,
    pub from_id: Uuid,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DedupReport {
    pub project_id: Uuid,
    pub namespace: String,
    pub refused_anchors: Vec<Uuid>,
    pub lineage: Vec<Uuid>,
    pub totals: BTreeMap<String, KindTotals>,
    pub unnamed_by_anchor: Vec<UnnamedByAnchor>,
    pub planned: Vec<PlannedGroup>,
    pub refused_groups: Vec<RefusedGroup>,
    pub unchanged: Vec<UnchangedCount>,
    pub apply_requested: bool,
    pub applied: Vec<AppliedMerge>,
    pub refused_merges: Vec<RefusedMerge>,
    pub success: bool,
}

impl DedupReport {
    /// One line per figure the stored-population census can be compared with.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "project {} namespace {} lineage {} refused anchors {}",
            self.project_id,
            self.namespace,
            self.lineage.len(),
            self.refused_anchors.len()
        )];
        for (kind, t) in &self.totals {
            lines.push(format!(
                "{kind}: candidates {} groups found {} planned {} refused {} merged away {} \
                 survivors renamed {} re-homed {}",
                t.candidates,
                t.groups_found,
                t.groups_planned,
                t.groups_refused,
                t.notes_merged_away,
                t.survivors_renamed,
                t.notes_rehomed
            ));
            lines.push(format!(
                "{kind}: unnamed {} = merged away {} + survivors {} + refused groups {} \
                 + unchanged {}",
                t.unnamed,
                t.unnamed_merged_away,
                t.unnamed_survivors,
                t.unnamed_in_refused_groups,
                t.unnamed_unchanged
            ));
        }
        for row in &self.unnamed_by_anchor {
            lines.push(format!(
                "{}: unnamed under project_id {} = {}",
                row.kind, row.project_id, row.unnamed_notes
            ));
        }
        for group in &self.refused_groups {
            lines.push(format!(
                "{} #{} refused: {}",
                group.kind, group.number, group.reason
            ));
        }
        lines
    }
}

/// A plan cannot be built from a report: only `plan_dedup` makes one, and
/// `apply_dedup` re-checks every merge on the writer connection.
pub struct DedupPlan {
    report: DedupReport,
}

impl DedupPlan {
    pub fn report(&self) -> &DedupReport {
        &self.report
    }
}

struct Candidate {
    id: Uuid,
    kind: String,
    version: i64,
    created_at: i64,
    name: Option<String>,
    content: String,
    properties: Value,
    canonical_annotation: bool,
    other_project_annotation: bool,
}

impl Candidate {
    fn number(&self) -> Option<i64> {
        self.properties
            .get("number")
            .and_then(Value::as_i64)
            .filter(|number| *number > 0)
    }

    fn title(&self) -> Option<&str> {
        self.properties
            .get("title")
            .and_then(Value::as_str)
            .filter(|title| !title.trim().is_empty())
    }

    /// Null, absent and blank urls are all "no url"; anything else compares as
    /// stored.
    fn url(&self) -> Option<&Value> {
        self.properties.get("url").filter(|url| match url {
            Value::Null => false,
            Value::String(text) => !text.trim().is_empty(),
            _ => true,
        })
    }

    fn project(&self) -> Option<Uuid> {
        self.properties
            .get("project_id")
            .and_then(Value::as_str)
            .and_then(|text| Uuid::parse_str(text).ok())
    }

    fn property_count(&self) -> usize {
        self.properties.as_object().map_or(0, |object| object.len())
    }

    fn planned(&self, project_id: Uuid) -> PlannedNote {
        PlannedNote {
            id: self.id,
            version: self.version,
            project_id,
            name: self.name.clone(),
        }
    }

    fn refused(&self, project_id: Uuid) -> RefusedNote {
        RefusedNote {
            id: self.id,
            project_id,
            title: self.title().map(str::to_owned),
            url: self.url().cloned(),
        }
    }
}

enum Outcome {
    Unchanged,
    MergedAway,
    Survivor,
    RefusedGroup,
}

fn tally(totals: &mut KindTotals, note: &Candidate, outcome: Outcome) {
    if note.name.is_some() {
        return;
    }
    match outcome {
        Outcome::Unchanged => totals.unnamed_unchanged += 1,
        Outcome::MergedAway => totals.unnamed_merged_away += 1,
        Outcome::Survivor => totals.unnamed_survivors += 1,
        Outcome::RefusedGroup => totals.unnamed_in_refused_groups += 1,
    }
}

pub async fn run_dedup(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    options: DedupOptions,
) -> Result<DedupReport> {
    let apply = options.apply;
    let plan = plan_dedup(runtime, token, options).await?;
    if apply {
        apply_dedup(runtime, token, plan).await
    } else {
        Ok(plan.report)
    }
}

pub async fn plan_dedup(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    options: DedupOptions,
) -> Result<DedupPlan> {
    let canonical = options.project_id;
    let namespace = token.namespace().as_str().to_owned();
    let mut reader = runtime.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: crate::sql::sql!("dedup_lineage_select").into(),
            params: vec![
                SqlValue::Text(canonical.to_string()),
                SqlValue::Text(namespace.clone()),
            ],
            label: Some("git_dedup_lineage".into()),
        })
        .await?;
    if rows.is_empty() {
        bail!("project must be a live canonical project in the selected namespace");
    }
    let lineage = rows
        .iter()
        .map(|row| Ok(Uuid::parse_str(row.text("id")?)?))
        .collect::<Result<BTreeSet<Uuid>>>()?;
    let lineage_json = serde_json::to_string(&lineage.iter().collect::<Vec<_>>())?;
    let rows = reader
        .query_all(SqlStatement {
            sql: crate::sql::sql!("dedup_candidates_select").into(),
            params: vec![
                SqlValue::Text(namespace.clone()),
                SqlValue::Text(canonical.to_string()),
                SqlValue::Text(lineage_json),
            ],
            label: Some("git_dedup_candidates".into()),
        })
        .await?;
    drop(reader);

    let mut candidates = Vec::with_capacity(rows.len());
    for row in &rows {
        let text = |key: &str| -> Result<Option<String>> {
            match row.get(key) {
                Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
                Some(SqlValue::Null) => Ok(None),
                _ => Err(anyhow!("invalid stored {key} type")),
            }
        };
        candidates.push(Candidate {
            id: Uuid::parse_str(row.text("id")?)?,
            kind: row.text("kind")?.into(),
            version: row.i64("version")?,
            created_at: row.i64("created_at")?,
            name: text("name")?,
            content: row.text("content")?.into(),
            properties: text("properties")?
                .map(|value| serde_json::from_str(&value))
                .transpose()?
                .unwrap_or(Value::Null),
            canonical_annotation: row.i64("canonical_annotation")? != 0,
            other_project_annotation: row.i64("other_project_annotation")? != 0,
        });
    }

    let mut report = DedupReport {
        project_id: canonical,
        namespace,
        refused_anchors: options.refused_anchors.iter().copied().collect(),
        lineage: lineage.iter().copied().collect(),
        totals: BTreeMap::new(),
        unnamed_by_anchor: Vec::new(),
        planned: Vec::new(),
        refused_groups: Vec::new(),
        unchanged: Vec::new(),
        apply_requested: false,
        applied: Vec::new(),
        refused_merges: Vec::new(),
        success: true,
    };
    let mut unchanged: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut unnamed_by_anchor: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut groups: BTreeMap<(String, i64), Vec<(&Candidate, Uuid)>> = BTreeMap::new();
    for note in &candidates {
        let totals = report.totals.entry(note.kind.clone()).or_default();
        totals.candidates += 1;
        if note.name.is_none() {
            totals.unnamed += 1;
            let anchor = note
                .properties
                .get("project_id")
                .and_then(Value::as_str)
                .unwrap_or("none")
                .to_owned();
            *unnamed_by_anchor
                .entry((note.kind.clone(), anchor))
                .or_default() += 1;
        }
        let eligible = || -> Result<(Uuid, i64), &'static str> {
            let project = note.project();
            if project.is_some_and(|id| options.refused_anchors.contains(&id)) {
                return Err("note belongs to a refused anchor");
            }
            let project = project
                .filter(|id| lineage.contains(id))
                .ok_or("project_id is not in the canonical project's lineage")?;
            if !note.canonical_annotation {
                return Err("no live annotation to the canonical project");
            }
            if note.other_project_annotation {
                return Err("also annotates another live project");
            }
            let number = note.number().ok_or("no positive integer number")?;
            Ok((project, number))
        };
        match eligible() {
            Ok((project, number)) => groups
                .entry((note.kind.clone(), number))
                .or_default()
                .push((note, project)),
            Err(reason) => {
                tally(totals, note, Outcome::Unchanged);
                *unchanged
                    .entry((note.kind.clone(), reason.to_owned()))
                    .or_default() += 1;
            }
        }
    }

    for ((kind, number), mut members) in groups {
        let totals = report.totals.entry(kind.clone()).or_default();
        if members.len() < 2 {
            let (note, _) = members[0];
            tally(totals, note, Outcome::Unchanged);
            *unchanged
                .entry((kind, "no duplicate in the group".to_owned()))
                .or_default() += 1;
            continue;
        }
        totals.groups_found += 1;
        let title = match common_title(&members) {
            Ok(title) => title,
            Err(reason) => {
                totals.groups_refused += 1;
                for (note, _) in &members {
                    tally(totals, note, Outcome::RefusedGroup);
                }
                report.refused_groups.push(RefusedGroup {
                    kind,
                    number,
                    reason,
                    notes: members
                        .iter()
                        .map(|(note, project)| note.refused(*project))
                        .collect(),
                });
                continue;
            }
        };
        members.sort_by(|(a, a_project), (b, b_project)| {
            (*b_project == canonical)
                .cmp(&(*a_project == canonical))
                .then_with(|| b.name.is_some().cmp(&a.name.is_some()))
                .then_with(|| b.property_count().cmp(&a.property_count()))
                .then_with(|| b.content.len().cmp(&a.content.len()))
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        let (survivor, survivor_project) = members.remove(0);
        let mut bodies = BTreeSet::from([survivor.content.as_str()]);
        let donors: Vec<PlannedDonor> = members
            .iter()
            .map(|(donor, project)| PlannedDonor {
                note: donor.planned(*project),
                body: if bodies.insert(donor.content.as_str()) {
                    BodyMerge::Append
                } else {
                    BodyMerge::KeepSurvivor
                },
            })
            .collect();
        let renamed_to = survivor
            .name
            .is_none()
            .then(|| members.iter().find_map(|(donor, _)| donor.name.clone()))
            .flatten();
        let rehomed = survivor_project != canonical;
        totals.groups_planned += 1;
        totals.notes_merged_away += donors.len();
        totals.survivors_renamed += usize::from(renamed_to.is_some());
        totals.notes_rehomed += usize::from(rehomed);
        tally(totals, survivor, Outcome::Survivor);
        for (donor, _) in &members {
            tally(totals, donor, Outcome::MergedAway);
        }
        report.planned.push(PlannedGroup {
            kind,
            number,
            title,
            survivor: survivor.planned(survivor_project),
            renamed_to,
            rehomed_from: rehomed.then_some(survivor_project),
            donors,
        });
    }

    report.unchanged = unchanged
        .into_iter()
        .map(|((kind, reason), notes)| UnchangedCount {
            kind,
            reason,
            notes,
        })
        .collect();
    report.unnamed_by_anchor = unnamed_by_anchor
        .into_iter()
        .map(|((kind, project_id), unnamed_notes)| UnnamedByAnchor {
            kind,
            project_id,
            unnamed_notes,
        })
        .collect();
    Ok(DedupPlan { report })
}

/// The one title a group of same-numbered notes shares, or why they cannot be
/// proven to be one record.
fn common_title(members: &[(&Candidate, Uuid)]) -> Result<String, String> {
    let mut titles = Vec::with_capacity(members.len());
    for (note, _) in members {
        titles.push(note.title().ok_or("a note has no title")?);
    }
    let distinct: BTreeSet<&str> = titles.iter().copied().collect();
    if distinct.len() > 1 {
        let shown: Vec<String> = distinct.iter().map(|title| format!("{title:?}")).collect();
        return Err(format!("titles differ: {}", shown.join(" | ")));
    }
    let first_url = members[0].0.url();
    if members.iter().any(|(note, _)| note.url() != first_url) {
        return Err("urls differ or are missing on some notes".into());
    }
    Ok(titles[0].to_owned())
}

pub async fn apply_dedup(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    mut plan: DedupPlan,
) -> Result<DedupReport> {
    plan.report.apply_requested = true;
    let canonical = plan.report.project_id;
    for group in &plan.report.planned {
        let mut kept_version = group.survivor.version;
        let mut stopped = false;
        for donor in &group.donors {
            if stopped {
                plan.report.refused_merges.push(RefusedMerge {
                    into_id: group.survivor.id,
                    from_id: donor.note.id,
                    reason: "group stopped after an earlier merge refusal".into(),
                });
                continue;
            }
            let mut assertions = Vec::new();
            for note in [&group.survivor, &donor.note] {
                assertions.extend([
                    MergeAssertion::EntityLineageReaches {
                        entity: note.project_id,
                        canonical,
                    },
                    MergeAssertion::NoteEdgeTo {
                        note: note.id,
                        relation: EdgeRelation::Annotates,
                        target: canonical,
                    },
                    MergeAssertion::NoteEdgeTargetsWithin {
                        note: note.id,
                        relation: EdgeRelation::Annotates,
                        target_kind: "project".into(),
                        allowed: canonical,
                    },
                ]);
            }
            let guard = NoteMergeGuard {
                into_version: kept_version,
                from_version: donor.note.version,
                assertions,
                survivor_properties: serde_json::Map::from_iter([(
                    "project_id".to_owned(),
                    json!(canonical.to_string()),
                )]),
                annotation: Some(json!({
                    "repair": "git_note_dedup",
                    "kind": group.kind,
                    "number": group.number,
                    "into_project_id": group.survivor.project_id.to_string(),
                    "from_project_id": donor.note.project_id.to_string(),
                })),
            };
            let content = match donor.body {
                BodyMerge::Append => ContentMergeStrategy::Append,
                BodyMerge::KeepSurvivor => ContentMergeStrategy::PreferInto,
            };
            match runtime
                .merge_note_guarded(
                    token,
                    group.survivor.id,
                    donor.note.id,
                    EntityDedupMergePolicy::PreferInto,
                    content,
                    false,
                    Some(MERGE_REASON.into()),
                    guard,
                )
                .await
            {
                Ok(merged) => {
                    kept_version = merged.kept_version;
                    plan.report.applied.push(AppliedMerge {
                        into_id: group.survivor.id,
                        from_id: donor.note.id,
                        kept_version,
                        summary: merged.summary,
                    });
                }
                Err(error) => {
                    plan.report.refused_merges.push(RefusedMerge {
                        into_id: group.survivor.id,
                        from_id: donor.note.id,
                        reason: error.to_string(),
                    });
                    stopped = true;
                }
            }
        }
    }
    plan.report.success = plan.report.refused_merges.is_empty();
    Ok(plan.report)
}

#[cfg(test)]
#[path = "dedup_tests.rs"]
mod tests;
