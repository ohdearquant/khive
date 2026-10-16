//! Source-input evidence from the same bytes admitted by the import scan.
//! An observation is lexical and revision-qualified; it never certifies a
//! complete filesystem read graph, cfg selection, or Cargo target role.

use super::*;
use crate::source_reads::{extract_source_reads, SourceRead, SourceReadScan};

/// L1.5 source-input observation counters. Missing scans and unresolved
/// inputs must not be interpreted as evidence that no reader exists.
#[derive(Debug, Default, serde::Serialize)]
pub struct CodeSourceReadReport {
    pub files_observed: u64,
    pub literal_reads: u64,
    pub resolved_reads: u64,
    pub unresolved_reads: u64,
    pub coverage_stamps_missed: u64,
}

#[derive(Debug)]
pub(super) struct SourceReadInput {
    pub source_path: String,
    pub source_revision: String,
    pub content_hash: String,
    pub language: String,
    pub sweep_time: String,
    pub scan: SourceReadScan,
}

impl SourceReadInput {
    fn matches(&self, props: &serde_json::Map<String, Value>) -> bool {
        props.get("source_path").and_then(Value::as_str) == Some(self.source_path.as_str())
            && props.get("source_revision").and_then(Value::as_str)
                == Some(self.source_revision.as_str())
            && props.get("content_hash").and_then(Value::as_str) == Some(self.content_hash.as_str())
            && props.get("language").and_then(Value::as_str) == Some(self.language.as_str())
            && props.get("last_seen_at").and_then(Value::as_str) == Some(self.sweep_time.as_str())
    }

    fn fingerprint(&self) -> Value {
        json!({
            "source_path": self.source_path,
            "source_revision": self.source_revision,
            "content_hash": self.content_hash,
            "sweep_time": self.sweep_time,
        })
    }
}

pub(super) struct SourceReadPlan {
    reader: Uuid,
    input: SourceReadInput,
    pub observation: Value,
    edges: BTreeMap<Uuid, Vec<Value>>,
    unresolved: u64,
}

impl SourceReadPlan {
    pub(super) fn matches(&self, props: &serde_json::Map<String, Value>) -> bool {
        self.input.matches(props)
    }
}

/// Parsing shares the existing Rust ingest safety budget. Use one bounded
/// scanner thread at a time, with the same stack allowance as L2, rather
/// than recursing through syn on a Tokio blocking worker's small stack.
pub(super) fn extract_on_worker(language: &str, content: String) -> SourceReadScan {
    if language != "rust" {
        return extract_source_reads(language, &content);
    }
    if let Err(reason) = check_rust_l2_nesting(&content) {
        return SourceReadScan {
            reads: Vec::new(),
            unresolved: vec![reason],
        };
    }
    static WORKER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _permit = WORKER.lock().unwrap_or_else(|poison| poison.into_inner());
    match std::thread::Builder::new()
        .name("khive-source-read-scanner".into())
        .stack_size(RUST_L2_SCANNER_STACK_BYTES)
        .spawn(move || extract_source_reads("rust", &content))
    {
        Ok(worker) => worker.join().unwrap_or_else(|_| SourceReadScan {
            reads: Vec::new(),
            unresolved: vec!["source-reader worker panicked".into()],
        }),
        Err(_) => SourceReadScan {
            reads: Vec::new(),
            unresolved: vec!["source-reader worker unavailable".into()],
        },
    }
}

/// Normalize lexically inside the repository root. Parent components are
/// valid for file-relative includes, but may never escape that root. This
/// does not open a target or follow symlinks; only another source admitted
/// by this invocation can become an edge endpoint.
fn target_path(reader: &str, read: &SourceRead) -> Option<String> {
    if read.path.is_empty()
        || read.path.starts_with('/')
        || read.path.starts_with('~')
        || read.path.chars().any(|ch| {
            ch.is_control() || matches!(ch, '\\' | ':' | '*' | '?' | '[' | ']' | '{' | '}')
        })
    {
        return None;
    }
    let mut parts: Vec<&str> = match read.base.as_str() {
        "file" => reader
            .rsplit_once('/')
            .map_or(Vec::new(), |(parent, _)| parent.split('/').collect()),
        "repository" => Vec::new(),
        _ => return None,
    };
    for part in read.path.split('/') {
        match part {
            "" => return None,
            "." => {}
            ".." => {
                parts.pop()?;
            }
            value => parts.push(value),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

pub(super) fn prepare(scans: &HashMap<Uuid, ModuleScan>) -> BTreeMap<Uuid, SourceReadPlan> {
    let mut by_path: BTreeMap<&str, Vec<(Uuid, &SourceReadInput)>> = BTreeMap::new();
    for (id, scan) in scans {
        if let [input] = scan.source_reads.as_slice() {
            by_path
                .entry(&input.source_path)
                .or_default()
                .push((*id, input));
        }
    }
    let mut plans = BTreeMap::new();
    for (id, scan) in scans {
        let Some(input) = scan.source_reads.last() else {
            continue;
        };
        let mut residuals = input.scan.unresolved.clone();
        let mut observations = Vec::new();
        let mut edges: BTreeMap<Uuid, Vec<Value>> = BTreeMap::new();
        let mut unresolved = 0;
        let unique_reader = scan.source_reads.len() == 1
            && by_path
                .get(input.source_path.as_str())
                .is_some_and(|rows| rows.len() == 1);
        if !unique_reader {
            residuals.push("source-module ownership is ambiguous".into());
        }
        for read in &input.scan.reads {
            let path = target_path(&input.source_path, read);
            let target = path.as_ref().and_then(|path| by_path.get(path.as_str()));
            let resolved_target = match target.map(Vec::as_slice) {
                Some([(target_id, target_input)])
                    if unique_reader
                        && input.source_revision == target_input.source_revision
                        && input.sweep_time == target_input.sweep_time =>
                {
                    Some((*target_id, *target_input))
                }
                _ => None,
            };
            let mut observation = serde_json::to_value(read).expect("source read serializes");
            observation["target_path"] = json!(path);
            observation["status"] = json!(if resolved_target.is_some() {
                "resolved"
            } else {
                "unresolved"
            });
            if let Some((target_id, target_input)) = resolved_target {
                observation["target_id"] = json!(target_id);
                edges.entry(target_id).or_default().push(json!({
                    "scanner_version": 1,
                    "reader": input.fingerprint(),
                    "input": target_input.fingerprint(),
                    "read": read,
                }));
            } else {
                observation["reason"] = json!(
                    "input is unsafe, absent, ambiguous, non-module, or outside this invocation"
                );
                unresolved += 1;
            }
            observations.push(observation);
        }
        unresolved += residuals.len() as u64;
        // Persist source-derived scan facts. The invocation clock remains in
        // row matching and edge fingerprints, not in the module observation.
        let observation = json!({
            "scanner_version": 1,
            "status": "lexical",
            "source_path": input.source_path,
            "source_revision": input.source_revision,
            "content_hash": input.content_hash,
            "observations": observations,
            "unresolved": residuals,
        });
        plans.insert(
            *id,
            SourceReadPlan {
                reader: *id,
                input: SourceReadInput {
                    source_path: input.source_path.clone(),
                    source_revision: input.source_revision.clone(),
                    content_hash: input.content_hash.clone(),
                    language: input.language.clone(),
                    sweep_time: input.sweep_time.clone(),
                    scan: input.scan.clone(),
                },
                observation,
                edges,
                unresolved,
            },
        );
    }
    plans
}

pub(super) async fn persist_edges(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    plans: &BTreeMap<Uuid, SourceReadPlan>,
    stamped: &BTreeSet<Uuid>,
    now: DateTime<Utc>,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let mut counts = CodeSourceReadReport::default();
    for plan in plans.values() {
        if !stamped.contains(&plan.reader) {
            counts.coverage_stamps_missed += 1;
            continue;
        }
        counts.files_observed += 1;
        counts.literal_reads += plan.input.scan.reads.len() as u64;
        counts.unresolved_reads += plan.unresolved;
        if plan.edges.is_empty() {
            continue;
        }
        // Do not emit edge metadata until the complete module observation
        // has passed the runtime secret gate and is still the current row.
        let Some(reader) = get_entity_opt(rt, token, plan.reader).await? else {
            counts.coverage_stamps_missed += 1;
            continue;
        };
        let Some(props) = reader.properties.as_ref().and_then(Value::as_object) else {
            counts.coverage_stamps_missed += 1;
            continue;
        };
        if !plan.matches(props) || props.get("source_read_scan") != Some(&plan.observation) {
            counts.coverage_stamps_missed += 1;
            continue;
        }
        for (target, observations) in &plan.edges {
            let Some(target_plan) = plans.get(target) else {
                continue;
            };
            let target_current = get_entity_opt(rt, token, *target).await?;
            if !stamped.contains(target)
                || !target_current.as_ref().is_some_and(|row| {
                    row.properties
                        .as_ref()
                        .and_then(Value::as_object)
                        .is_some_and(|props| {
                            target_plan.matches(props)
                                && props.get("source_read_scan") == Some(&target_plan.observation)
                        })
                })
            {
                counts.coverage_stamps_missed += 1;
                continue;
            }
            let edge_id = edge_uuid(EdgeRelation::DependsOn, plan.reader, *target);
            let outcome = mutate_edge(rt, token, edge_id, |current| {
                let mut metadata = merge_dependency_metadata(
                    current.and_then(|edge| edge.metadata.as_ref()),
                    "source_read",
                    "build",
                    &plan.input.language,
                );
                metadata["source_read_observations"] = json!(observations);
                Some(Edge {
                    id: LinkId::from(edge_id),
                    namespace: token.namespace().as_str().to_string(),
                    source_id: plan.reader,
                    target_id: *target,
                    relation: EdgeRelation::DependsOn,
                    weight: 1.0,
                    created_at: current.map(|edge| edge.created_at).unwrap_or(now),
                    updated_at: now,
                    deleted_at: None,
                    metadata: Some(metadata),
                    target_backend: current.and_then(|edge| edge.target_backend.clone()),
                })
            })
            .await?;
            match outcome {
                RowMutationOutcome::Created => report.edges_created += 1,
                RowMutationOutcome::Updated => report.edges_updated += 1,
                RowMutationOutcome::Unchanged | RowMutationOutcome::Blocked => {}
            }
            counts.resolved_reads += observations.len() as u64;
        }
    }
    report.source_reads = Some(counts);
    Ok(())
}

#[cfg(test)]
#[path = "source_reads_tests.rs"]
mod tests;
