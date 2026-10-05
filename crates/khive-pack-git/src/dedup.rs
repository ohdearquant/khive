//! Bounded, explicit repair of duplicate issue and pull-request notes.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Result};
use khive_runtime::curation::git_note_dedup::{valid_git_note_forge_url, GitNoteMergeGuard};
use khive_runtime::curation::MergeSummary;
use khive_runtime::{KhiveRuntime, NamespaceToken};
use khive_storage::types::{SqlStatement, SqlValue};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

const MAX_ROWS: usize = 10_000;
const MAX_LINEAGE: usize = 4096;
const MAX_HOPS: i64 = 64;
const MAX_ROW_BYTES: i64 = 256 * 1024;
const MAX_TOTAL_BYTES: i64 = 16 * 1024 * 1024;

type GroupKey = (String, i64);

#[derive(Clone, Debug)]
pub struct DedupOptions {
    pub project_id: Uuid,
    pub apply: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlannedMerge {
    pub into_id: Uuid,
    pub from_id: Uuid,
    pub into_version: i64,
    pub from_version: i64,
    pub kind: String,
    pub number: i64,
    pub placeholder_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UnchangedNote {
    pub id: Uuid,
    pub reason: String,
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
    pub preview_id: String,
    pub complete_census: bool,
    pub census_rows: usize,
    pub lineage: Vec<Uuid>,
    pub diagnostics: Vec<String>,
    pub planned: Vec<PlannedMerge>,
    pub unchanged: Vec<UnchangedNote>,
    pub applied: Vec<AppliedMerge>,
    pub refused: Vec<RefusedMerge>,
    pub apply_requested: bool,
    pub success: bool,
}

/// A plan cannot be constructed from an untrusted report or incomplete page.
/// Apply revalidates every pair in the runtime's merge transaction.
pub struct DedupPlan {
    report: DedupReport,
    groups: Vec<Vec<PlannedMerge>>,
}

impl DedupPlan {
    pub fn report(&self) -> &DedupReport {
        &self.report
    }
}

#[derive(Serialize)]
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
            .filter(|n| *n > 0)
    }
    fn url(&self) -> Option<&str> {
        self.properties
            .get("url")
            .and_then(Value::as_str)
            .filter(|s| valid_git_note_forge_url(s))
    }
    fn real_title(&self) -> bool {
        self.name
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty() && name != format!("[{}]", self.kind))
    }
    fn property_count(&self) -> usize {
        self.properties
            .as_object()
            .map_or(0, |properties| properties.len())
    }
}

pub async fn run_dedup(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    options: DedupOptions,
) -> Result<DedupReport> {
    let plan = plan_dedup(runtime, token, options.project_id).await?;
    if options.apply {
        apply_dedup(runtime, token, plan).await
    } else {
        Ok(plan.report)
    }
}

pub async fn plan_dedup(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project_id: Uuid,
) -> Result<DedupPlan> {
    let namespace = token.namespace().as_str().to_owned();
    let mut reader = runtime.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: crate::sql::sql!("dedup_lineage_select").into(),
            params: vec![
                SqlValue::Text(project_id.to_string()),
                SqlValue::Text(namespace.clone()),
                SqlValue::Integer(MAX_HOPS),
                SqlValue::Integer((MAX_LINEAGE + 1) as i64),
            ],
            label: Some("git_dedup_lineage".into()),
        })
        .await?;
    if rows.is_empty() {
        bail!("project must be a live canonical project in the selected namespace");
    }
    let mut report = DedupReport {
        project_id,
        namespace: namespace.clone(),
        preview_id: String::new(),
        complete_census: true,
        census_rows: 0,
        lineage: Vec::new(),
        diagnostics: Vec::new(),
        planned: Vec::new(),
        unchanged: Vec::new(),
        applied: Vec::new(),
        refused: Vec::new(),
        apply_requested: false,
        success: true,
    };
    if rows.len() > MAX_LINEAGE {
        report.complete_census = false;
        report
            .diagnostics
            .push("project lineage exceeds 4096 entities".into());
    }
    for row in rows.iter().take(MAX_LINEAGE) {
        report.lineage.push(Uuid::parse_str(row.text("id")?)?);
        if row.i64("depth")? == MAX_HOPS && row.i64("has_children")? != 0 {
            report.complete_census = false;
            report
                .diagnostics
                .push("project lineage exceeds 64 hops".into());
        }
    }
    report.lineage.sort_unstable();
    let lineage: BTreeSet<_> = report.lineage.iter().copied().collect();
    let lineage_json = serde_json::to_string(
        &report
            .lineage
            .iter()
            .map(|id| id.simple().to_string())
            .collect::<Vec<_>>(),
    )?;
    let rows = reader
        .query_all(SqlStatement {
            sql: crate::sql::sql!("dedup_candidates_select").into(),
            params: vec![
                SqlValue::Text(namespace),
                SqlValue::Text(project_id.to_string()),
                SqlValue::Text(lineage_json),
                SqlValue::Integer((MAX_ROWS + 1) as i64),
                SqlValue::Integer(MAX_ROW_BYTES),
                SqlValue::Integer(MAX_TOTAL_BYTES),
            ],
            label: Some("git_dedup_candidates".into()),
        })
        .await?;
    report.census_rows = rows.len();
    if rows.len() > MAX_ROWS {
        report.complete_census = false;
        report
            .diagnostics
            .push("candidate census exceeds 10000 rows".into());
    }
    let mut candidates = Vec::new();
    for row in rows.iter().take(MAX_ROWS) {
        let id = Uuid::parse_str(row.text("id")?)?;
        if row.i64("within_budget")? == 0 {
            report.complete_census = false;
            report.unchanged.push(UnchangedNote {
                id,
                reason: "candidate payload budget exceeded".into(),
            });
            continue;
        }
        let text = |key| -> Result<Option<String>> {
            match row.get(key) {
                Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
                Some(SqlValue::Null) => Ok(None),
                _ => Err(anyhow!("invalid stored {key} type")),
            }
        };
        candidates.push(Candidate {
            id,
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
    drop(reader);
    let source_bytes = serde_json::to_vec(&(&report, &candidates))?;
    let mut groups: BTreeMap<GroupKey, Vec<&Candidate>> = BTreeMap::new();
    let mut placeholders = Vec::new();
    let mut urls: BTreeMap<&str, BTreeSet<GroupKey>> = BTreeMap::new();
    for note in &candidates {
        let project = note
            .properties
            .get("project_id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok());
        let reason = if !project.is_some_and(|id| lineage.contains(&id)) {
            Some("missing or unproven project lineage")
        } else if !note.canonical_annotation {
            Some("missing live canonical project annotation")
        } else if note.other_project_annotation {
            Some("ambiguous live project annotations")
        } else {
            None
        };
        if let Some(reason) = reason {
            report.unchanged.push(UnchangedNote {
                id: note.id,
                reason: reason.into(),
            });
            continue;
        }
        if let (Some(number), Some(url)) = (note.number(), note.url()) {
            // An unmergeable history still supplies contradictory numbered URL
            // evidence; the writer's uniqueness check sees that live note too.
            urls.entry(url)
                .or_default()
                .insert((note.kind.clone(), number));
        }
        if note
            .properties
            .get("_merge_history")
            .is_some_and(|history| !history.is_array())
        {
            report.unchanged.push(UnchangedNote {
                id: note.id,
                reason: "malformed merge history".into(),
            });
        } else if let Some(number) = note.number() {
            groups
                .entry((note.kind.clone(), number))
                .or_default()
                .push(note);
        } else if note
            .properties
            .get("number")
            .is_some_and(|number| !number.is_null())
        {
            report.unchanged.push(UnchangedNote {
                id: note.id,
                reason: "malformed or nonpositive number".into(),
            });
        } else if note.name.as_deref() == Some(format!("[{}]", note.kind).as_str())
            && note.url().is_some()
        {
            placeholders.push(note);
        } else {
            report.unchanged.push(UnchangedNote {
                id: note.id,
                reason: "numberless note lacks exact placeholder URL evidence".into(),
            });
        }
    }
    let mut adopted: BTreeMap<GroupKey, Vec<&Candidate>> = BTreeMap::new();
    for note in placeholders {
        let keys = urls.get(note.url().expect("validated placeholder"));
        if let Some(keys) = keys.filter(|keys| keys.len() == 1) {
            let key = keys.first().expect("one key");
            if key.0 == note.kind && groups.contains_key(key) {
                adopted.entry(key.clone()).or_default().push(note);
                continue;
            }
        }
        report.unchanged.push(UnchangedNote {
            id: note.id,
            reason: "placeholder URL does not identify one same-kind numbered group".into(),
        });
    }
    let mut plans = Vec::new();
    for (key, mut notes) in groups {
        notes.sort_by(|a, b| {
            b.real_title()
                .cmp(&a.real_title())
                .then_with(|| b.content.len().cmp(&a.content.len()))
                .then_with(|| b.property_count().cmp(&a.property_count()))
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        let into = notes.remove(0);
        let placeholders = adopted.remove(&key).unwrap_or_default();
        let mut accepted_url = into.url();
        if !into
            .properties
            .as_object()
            .is_some_and(|properties| properties.contains_key("url"))
        {
            // PreferInto only fills an absent key. Pick one exact donor before any
            // placeholder; a null/conflicting survivor URL is never overwritten.
            if let Some(index) = notes.iter().position(|note| {
                note.url()
                    .is_some_and(|url| placeholders.iter().any(|p| p.url() == Some(url)))
            }) {
                let donor = notes.remove(index);
                accepted_url = donor.url();
                notes.insert(0, donor);
            } else {
                accepted_url = notes.iter().find_map(|note| note.url());
            }
        }
        let mut merges: Vec<_> = notes
            .iter()
            .map(|from| planned(into, from, &key, None))
            .collect();
        for from in placeholders {
            if accepted_url == from.url() {
                merges.push(planned(into, from, &key, from.url().map(str::to_owned)));
            } else {
                report.unchanged.push(UnchangedNote {
                    id: from.id,
                    reason: "survivor cannot retain this placeholder's exact URL".into(),
                });
            }
        }
        if !merges.is_empty() {
            report.planned.extend(merges.iter().cloned());
            plans.push(merges);
        } else {
            report.unchanged.push(UnchangedNote {
                id: into.id,
                reason: "no proven duplicate".into(),
            });
        }
    }
    report.unchanged.sort_by_key(|note| note.id);
    report.diagnostics.sort();
    report.diagnostics.dedup();
    report.success = report.complete_census;
    let mut hash = blake3::Hasher::new();
    hash.update(&source_bytes);
    hash.update(&serde_json::to_vec(&(&report.planned, &report.unchanged))?);
    report.preview_id = hash.finalize().to_hex().to_string();
    Ok(DedupPlan {
        report,
        groups: plans,
    })
}

fn planned(
    into: &Candidate,
    from: &Candidate,
    key: &GroupKey,
    url: Option<String>,
) -> PlannedMerge {
    PlannedMerge {
        into_id: into.id,
        from_id: from.id,
        into_version: into.version,
        from_version: from.version,
        kind: key.0.clone(),
        number: key.1,
        placeholder_url: url,
    }
}

pub async fn apply_dedup(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    mut plan: DedupPlan,
) -> Result<DedupReport> {
    plan.report.apply_requested = true;
    if !plan.report.complete_census || plan.report.namespace != token.namespace().as_str() {
        plan.report.success = false;
        plan.report
            .diagnostics
            .push("apply refused: incomplete census or changed namespace".into());
        return Ok(plan.report);
    }
    for group in plan.groups {
        let mut kept_version = group[0].into_version;
        let mut stopped = false;
        for pair in group {
            if stopped {
                plan.report.refused.push(RefusedMerge {
                    into_id: pair.into_id,
                    from_id: pair.from_id,
                    reason: "group stopped after an earlier merge refusal".into(),
                });
                continue;
            }
            let guard = GitNoteMergeGuard {
                project_id: plan.report.project_id,
                kind: pair.kind,
                number: pair.number,
                into_version: kept_version,
                from_version: pair.from_version,
                placeholder_url: pair.placeholder_url,
            };
            match runtime
                .merge_git_note_guarded(token, pair.into_id, pair.from_id, guard, false)
                .await
            {
                Ok(result) => {
                    kept_version = result.kept_version;
                    plan.report.applied.push(AppliedMerge {
                        into_id: pair.into_id,
                        from_id: pair.from_id,
                        kept_version,
                        summary: result.summary,
                    });
                }
                Err(error) => {
                    plan.report.refused.push(RefusedMerge {
                        into_id: pair.into_id,
                        from_id: pair.from_id,
                        reason: error.to_string(),
                    });
                    stopped = true;
                }
            }
        }
    }
    plan.report.success = plan.report.refused.is_empty();
    Ok(plan.report)
}

#[cfg(test)]
#[path = "dedup_tests.rs"]
mod tests;
