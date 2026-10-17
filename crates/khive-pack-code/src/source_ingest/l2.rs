use super::*;

// ===== L2: symbol/call-edge persistence =====
//
// Declaration/impl shapes (`DeclKind`, `CallRef`, `TypeRef`,
// `ExtractedDeclaration`, `ExtractedImpl`, `ExtractedFile`) live in
// `crate::extractor`; this module consumes that language-neutral contract
// rather than defining a parallel copy.

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(super) struct L2OwnerKey {
    pub(super) source_project: String,
    pub(super) language: String,
}

#[derive(Debug)]
pub(super) struct L2OwnerCoverage {
    pub(super) whole: bool,
    pub(super) fallback: bool,
    pub(super) file_label: String,
}

#[derive(Debug)]
pub(super) struct L2SweepState {
    pub(super) run_id: Uuid,
    pub(super) owners: HashMap<L2OwnerKey, L2OwnerCoverage>,
    /// Declarations proven current by this L2 invocation, never ambient
    /// ownership left by a prior sweep or an earlier tier in this call.
    pub(super) current_declarations: HashMap<Uuid, L2OwnerKey>,
    /// File modules whose L2 ownership was successfully refreshed/stamped.
    pub(super) current_modules: HashMap<Uuid, L2OwnerKey>,
    pub(super) current_files: BTreeMap<(Uuid, String), String>,
    /// Current declarations reused without parsing. Their outgoing edges are
    /// refreshed only after every target file's ownership is finalized.
    pub(super) unchanged_declarations: BTreeSet<Uuid>,
    /// Natural L2 dependency/implementation edges successfully stamped this
    /// sweep; this set is the authority for `symbol_edges_stamped`.
    pub(super) stamped_edge_ids: BTreeSet<Uuid>,
    /// Successful natural writer observations, including repeated shared IDs.
    pub(super) observed_natural_edge_ids: Vec<Uuid>,
    /// Accepted natural relations belonging to reused physical producers.
    pub(super) current_natural_edge_ids: BTreeSet<Uuid>,
}

impl L2SweepState {
    pub(super) fn new(run_id: Uuid) -> Self {
        Self {
            run_id,
            owners: HashMap::new(),
            current_declarations: HashMap::new(),
            current_modules: HashMap::new(),
            current_files: BTreeMap::new(),
            unchanged_declarations: BTreeSet::new(),
            stamped_edge_ids: BTreeSet::new(),
            observed_natural_edge_ids: Vec::new(),
            current_natural_edge_ids: BTreeSet::new(),
        }
    }
}

#[cfg(test)]
impl Default for L2SweepState {
    fn default() -> Self {
        Self::new(Uuid::new_v4())
    }
}

pub(super) async fn complete_l2_sweeps(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    sweep_time: DateTime<Utc>,
    previous_stamps: &PreviousL2SweepStamps,
    state: &L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let run_id = previous_stamps.run_id.to_string();
    for (owner, coverage) in &state.owners {
        if !coverage.whole || coverage.fallback {
            continue;
        }
        let outcome = mutate_entity(
            rt,
            token,
            project_uuid(&owner.source_project),
            &coverage.file_label,
            report,
            |current| {
                let mut entity = current?.clone();
                let props = entity.properties.as_mut()?.as_object_mut()?;
                let runs = props.get_mut("l2_sweep_runs")?.as_object_mut()?;
                let entry = runs.get_mut(&owner.language)?;
                if !valid_l2_sweep_entry(entry)
                    || entry["attempted"]["run_id"].as_str() != Some(run_id.as_str())
                {
                    return None;
                }
                entry["completed"] = json!({
                    "run_id": run_id,
                    "sweep_time": sweep_time.to_rfc3339(),
                });
                entity.updated_at = ts(sweep_time);
                Some(entity)
            },
        )
        .await?;
        if outcome.wrote() {
            report.projects_updated += 1;
        }
    }
    Ok(())
}

impl L2SweepState {
    pub(super) fn mark_current_declarations(
        &mut self,
        ids: &[Uuid],
        source_project: &str,
        language: &str,
    ) {
        let owner = L2OwnerKey {
            source_project: source_project.to_string(),
            language: language.to_string(),
        };
        for id in ids {
            self.current_declarations.insert(*id, owner.clone());
        }
    }

    pub(super) fn mark_current_module(&mut self, id: Uuid, source_project: &str, language: &str) {
        self.current_modules.insert(
            id,
            L2OwnerKey {
                source_project: source_project.to_string(),
                language: language.to_string(),
            },
        );
    }

    pub(super) fn is_current_declaration(
        &self,
        id: Uuid,
        source_project: &str,
        language: &str,
        current_file_ids: &BTreeSet<Uuid>,
    ) -> bool {
        current_file_ids.contains(&id)
            || self.current_declarations.get(&id).is_some_and(|owner| {
                owner.source_project == source_project && owner.language == language
            })
    }
}

/// L2 Rust source parsing: the real syn-based scan (`scanner_rust`) adapted
/// into the language-neutral extractor shape (`extractor::from_rust_scan`).
/// A `syn::Error` (i.e. content that does not parse as a Rust file) surfaces
/// through the parse-failure channel: retain source
/// metadata, no `declaration_ids` stamp, increment `symbol_parse_failures`,
/// warn, retry next sweep) instead of aborting the sweep.
pub(super) fn parse_rust_file(content: &str) -> Result<ExtractedFile, String> {
    #[cfg(test)]
    if content.contains("l2_worker_probe_3292") {
        if let Some(observer) = scanner_thread_observer()
            .lock()
            .expect("scanner observer lock")
            .take()
        {
            let current = std::thread::current();
            let _ = observer.send((current.id(), current.name().map(str::to_string)));
        }
    }
    crate::scanner_rust::scan_rust_source(content)
        .map(crate::extractor::from_rust_scan)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
pub(super) type ScannerThreadObservation = (std::thread::ThreadId, Option<String>);

#[cfg(test)]
pub(super) type ScannerThreadObserver =
    std::sync::Mutex<Option<std::sync::mpsc::Sender<ScannerThreadObservation>>>;

#[cfg(test)]
pub(super) fn scanner_thread_observer() -> &'static ScannerThreadObserver {
    static OBSERVER: OnceLock<ScannerThreadObserver> = OnceLock::new();
    OBSERVER.get_or_init(|| std::sync::Mutex::new(None))
}

/// Parsing and adaptation may recurse inside syn and the scanner. Keep that
/// work off the async executor on a known stack, with a process-wide cap on
/// concurrent scanner threads. The caller has already checked source size
/// and delimiter depth before this function is reached.
pub(super) async fn parse_rust_file_on_worker(content: String) -> Result<ExtractedFile, String> {
    static WORKERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let workers = WORKERS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(RUST_L2_SCANNER_WORKERS)))
        .clone();
    let permit = workers
        .acquire_owned()
        .await
        .map_err(|_| "scanner worker pool unavailable".to_string())?;
    let (send, receive) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("khive-rust-l2-scanner".to_string())
        .stack_size(RUST_L2_SCANNER_STACK_BYTES)
        .spawn(move || {
            let _permit = permit;
            let result = std::panic::catch_unwind(|| parse_rust_file(&content))
                .unwrap_or_else(|_| Err("scanner worker panicked".to_string()));
            let _ = send.send(result);
        })
        .map_err(|error| format!("scanner worker unavailable: {error}"))?;
    receive
        .await
        .map_err(|_| "scanner worker terminated".to_string())?
}

/// Join a file module's own path with a declaration's in-file nesting
/// segments into the absolute module path it lives in. Rust-only, so the
/// separator is always `::` (`module_path_separator("rust")`).
pub(super) fn resolve_module_path(file_module_path: &str, module_segments: &[String]) -> String {
    if module_segments.is_empty() {
        file_module_path.to_string()
    } else {
        format!("{file_module_path}::{}", module_segments.join("::"))
    }
}

/// Resolve the immediate containment owner for an extracted declaration.
/// Top-level declarations belong to the file module; nested declarations
/// belong to the inline-module declaration named by the last segment.
pub(super) fn declaration_owner_id(
    source_project: &str,
    language: &str,
    file_module_path: &str,
    file_module_id: Uuid,
    module_segments: &[String],
) -> Uuid {
    match module_segments.split_last() {
        None => file_module_id,
        Some((name, parent_segments)) => symbol_uuid(
            source_project,
            language,
            &resolve_module_path(file_module_path, parent_segments),
            name,
            "module",
        ),
    }
}

/// Unchanged content reuses its ownership stamp only when that stamp was
/// produced by this Rust scanner identity version. Older generic method IDs
/// must be recomputed even when the source bytes have not changed.
pub(super) fn l2_needs_reparse(
    existing_content_hash: Option<&str>,
    existing_declaration_ids: Option<&Value>,
    existing_identity_version: Option<u64>,
    new_content_hash: &str,
) -> bool {
    existing_content_hash != Some(new_content_hash)
        || existing_identity_version != Some(RUST_L2_SCANNER_IDENTITY_VERSION)
        || existing_declaration_ids
            .and_then(read_declaration_ids)
            .is_none()
}

pub(super) fn read_declaration_ids(value: &Value) -> Option<Vec<Uuid>> {
    value
        .as_array()?
        .iter()
        .map(|value| value.as_str().and_then(|id| Uuid::parse_str(id).ok()))
        .collect()
}

/// Candidate target symbol ids for a call/type-reference path, tried in
/// priority order: same-declaring-module bare name first (a sibling
/// function/type reached without a path prefix), then the path's own
/// module-prefix-as-declared. There is no real name resolver here — L2 is a
/// documented syntax coverage floor — so this is a
/// best-effort heuristic, not exhaustive Rust name resolution. Because every
/// candidate is built from the caller's own `source_project`/`language`,
/// resolution can never cross a project or language boundary: a reference
/// that would only resolve elsewhere simply stays unresolved rather than
/// producing a cross-project edge (same-source-project enforcement is
/// structural here, not a separate rejection check).
pub(super) fn symbol_candidate_ids(
    source_project: &str,
    language: &str,
    declaring_module_path: &str,
    segments: &[String],
    evidence: &str,
) -> Vec<Uuid> {
    let kinds: &[&str] = match evidence {
        "call" => &["function", "datatype", "interface"],
        "type_reference" => &["datatype", "interface"],
        _ => return Vec::new(),
    };
    symbol_candidate_ids_for_kinds(
        source_project,
        language,
        declaring_module_path,
        segments,
        kinds,
    )
}

pub(super) fn symbol_candidate_ids_for_kinds(
    source_project: &str,
    language: &str,
    declaring_module_path: &str,
    segments: &[String],
    kinds: &[&str],
) -> Vec<Uuid> {
    let Some((name, prefix_segments)) = segments.split_last() else {
        return Vec::new();
    };
    let module_paths = candidate_module_paths(declaring_module_path, prefix_segments);
    let canonical_kinds: Vec<&str> = kinds
        .iter()
        .filter_map(|kind| DeclKind::from_code_token(kind))
        .map(DeclKind::code_token)
        .collect();
    let mut candidates = Vec::with_capacity(canonical_kinds.len() * module_paths.len());
    for module_path in module_paths {
        for kind in &canonical_kinds {
            candidates.push(symbol_uuid(
                source_project,
                language,
                &module_path,
                name,
                kind,
            ));
        }
    }
    candidates
}

pub(super) fn candidate_module_paths(
    declaring_module_path: &str,
    prefix: &[String],
) -> Vec<String> {
    if prefix.is_empty() {
        return vec![declaring_module_path.to_string()];
    }

    let mut paths = Vec::new();
    match prefix[0].as_str() {
        "crate" => push_module_path_variants(&mut paths, prefix.join("::")),
        "self" => {
            let suffix = prefix[1..].join("::");
            let path = if suffix.is_empty() {
                declaring_module_path.to_string()
            } else {
                format!("{declaring_module_path}::{suffix}")
            };
            push_module_path_variants(&mut paths, path);
        }
        "super" => {
            let mut base: Vec<String> = declaring_module_path
                .split("::")
                .map(str::to_string)
                .collect();
            let count = prefix
                .iter()
                .take_while(|segment| segment.as_str() == "super")
                .count();
            let max_ascents = if base.first().is_some_and(|segment| segment == "crate") {
                base.len().saturating_sub(1)
            } else {
                base.len()
            };
            if count > max_ascents {
                return Vec::new();
            }
            for _ in 0..count {
                base.pop();
            }
            if base.is_empty() {
                base.push("crate".to_string());
            }
            let suffix = prefix[count..].join("::");
            let base = base.join("::");
            let path = if suffix.is_empty() {
                base
            } else {
                format!("{base}::{suffix}")
            };
            push_module_path_variants(&mut paths, path);
        }
        _ => return Vec::new(),
    }
    paths
}

pub(super) fn push_module_path_variants(paths: &mut Vec<String>, path: String) {
    let mut variants = vec![path.clone()];
    if let Some(stripped) = path.strip_prefix("crate::") {
        variants.push(stripped.to_string());
    }
    for variant in variants {
        if !variant.is_empty() && !paths.contains(&variant) {
            paths.push(variant);
        }
    }
}

/// Unresolved call/type tuple. Declaration rows retain historical unions;
/// accepted per-file entries alone authorize late materialization.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub(super) struct L2UnresolvedRef {
    pub(super) segments: Vec<String>,
    pub(super) evidence: String,
}

#[cfg(test)]
pub(super) fn read_l2_unresolved(properties: &Value) -> Vec<L2UnresolvedRef> {
    properties
        .get("l2_unresolved_references")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// Attempt immediate same-project resolution of one call/type reference
/// declared by `declaring_id`; on success upserts (or refreshes) a
/// `depends_on` edge with the given evidence, on failure stages a pending
/// reference for the phase-local flush and reresolve pass. Nonfatal either way.
#[allow(clippy::too_many_arguments)]
pub(super) async fn resolve_l2_reference(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    declaring_module_path: &str,
    declaring_id: Uuid,
    current_file_ids: &BTreeSet<Uuid>,
    segments: &[String],
    evidence: &str,
    sweep_time: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<L2UnresolvedRef>, CodeSourceIngestError> {
    if segments.is_empty() {
        return Ok(None);
    }
    let mut target = None;
    let mut suppressed_self_type = false;
    for candidate in symbol_candidate_ids(
        source_project,
        language,
        declaring_module_path,
        segments,
        evidence,
    ) {
        if candidate == declaring_id && evidence == "type_reference" {
            suppressed_self_type = true;
            break;
        }
        if state.is_current_declaration(candidate, source_project, language, current_file_ids) {
            target = Some(candidate);
            break;
        }
    }
    match target {
        Some(target_id) => {
            upsert_l2_depends_on(
                rt,
                token,
                declaring_id,
                target_id,
                evidence,
                language,
                sweep_time,
                state,
                report,
            )
            .await?;
        }
        None if !suppressed_self_type => {
            let reference = L2UnresolvedRef {
                segments: segments.to_vec(),
                evidence: evidence.to_string(),
            };
            return Ok(Some(reference));
        }
        None => {}
    }
    Ok(None)
}

/// Attempt immediate same-project resolution of one positive `impl Trait for
/// Type`. Unlike a call/type reference, an impl has no declaring storage
/// entity of its own, so a failed
/// resolution is staged as a pending impl on the *file module* instead,
/// for the reresolve pass to retry.
#[allow(clippy::too_many_arguments)]
pub(super) async fn resolve_l2_implements(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    containing_module_path: &str,
    current_file_ids: &BTreeSet<Uuid>,
    type_path: &[String],
    trait_path: &[String],
    sweep_time: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<L2PendingImpl>, CodeSourceIngestError> {
    if type_path.is_empty() || trait_path.is_empty() {
        return Ok(None);
    }
    let type_id = find_first_current(
        state,
        source_project,
        language,
        symbol_candidate_ids_for_kinds(
            source_project,
            language,
            containing_module_path,
            type_path,
            &["datatype"],
        ),
        current_file_ids,
    );
    let trait_id = find_first_current(
        state,
        source_project,
        language,
        symbol_candidate_ids_for_kinds(
            source_project,
            language,
            containing_module_path,
            trait_path,
            &["interface"],
        ),
        current_file_ids,
    );
    match (type_id, trait_id) {
        (Some(type_id), Some(trait_id)) => {
            upsert_l2_implements(
                rt, token, type_id, trait_id, language, sweep_time, state, report,
            )
            .await?;
        }
        _ => {
            return Ok(Some(L2PendingImpl {
                type_path: type_path.to_vec(),
                trait_path: trait_path.to_vec(),
            }));
        }
    }
    Ok(None)
}

pub(super) fn find_first_current(
    state: &L2SweepState,
    source_project: &str,
    language: &str,
    candidates: Vec<Uuid>,
    current_file_ids: &BTreeSet<Uuid>,
) -> Option<Uuid> {
    candidates.into_iter().find(|candidate| {
        state.is_current_declaration(*candidate, source_project, language, current_file_ids)
    })
}

/// A `uuid5`-recomputable unresolved positive impl, recorded on the *file
/// module* entity that declared it (mirrors [`L2UnresolvedRef`] on symbol
/// entities — see [`resolve_l2_implements`]'s doc comment for why the
/// attachment point differs).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub(super) struct L2PendingImpl {
    pub(super) type_path: Vec<String>,
    pub(super) trait_path: Vec<String>,
}

pub(super) fn read_l2_pending_impls(properties: &Value) -> Vec<L2PendingImpl> {
    properties
        .get("l2_pending_impls")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

#[derive(Clone)]
pub(super) struct PendingL2<T> {
    pub(super) value: T,
    pub(super) file: String,
}

pub(super) fn append_l2_pending<T: Clone + Eq + Hash>(list: &mut Vec<T>, additions: &[T]) -> usize {
    let mut seen: HashSet<T> = list.iter().cloned().collect();
    let mut appended = 0;
    for addition in additions {
        if seen.insert(addition.clone()) {
            list.push(addition.clone());
            appended += 1;
        }
    }
    appended
}

pub(super) fn rebase_l2_pending<T: Clone + Eq + Hash>(
    current: &mut Vec<T>,
    original: &HashSet<T>,
    remaining: &[T],
) {
    current.retain(|value| !original.contains(value));
    append_l2_pending(current, remaining);
}

pub(super) async fn record_l2_pending_batch<T>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    key: &str,
    pending: &[PendingL2<T>],
    report: &mut CodeSourceIngestReport,
) -> Result<bool, CodeSourceIngestError>
where
    T: Clone + Eq + Hash + serde::Serialize + serde::de::DeserializeOwned,
{
    if pending.is_empty() {
        return Ok(false);
    }
    let read_list = |entity: &Entity| -> Vec<T> {
        entity
            .properties
            .as_ref()
            .and_then(|props| props.get(key))
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    };
    let Some(current) = rt
        .entities(token)?
        .get_entity_including_deleted(id)
        .await
        .map_err(|error| CodeSourceIngestError::Storage(error.to_string()))?
    else {
        return Ok(false);
    };
    #[cfg(test)]
    l2_batch_tests::observe_row_read(id);
    let existing: HashSet<T> = read_list(&current).into_iter().collect();
    if pending.iter().all(|item| existing.contains(&item.value)) {
        return Ok(false);
    }
    advancing_entity_revision(current.updated_at, current.updated_at)?;
    let context = candidate_context(token.namespace().as_str());
    if let Err(error) = gate_check_with_context(&context, &current) {
        match error {
            RuntimeError::SecretDetected(secret) => {
                for item in pending
                    .iter()
                    .filter(|item| !existing.contains(&item.value))
                {
                    report.blocked_count += 1;
                    report.blocked.push(BlockedWrite {
                        file: item.file.clone(),
                        detector: secret.detector.to_string(),
                        masked_excerpt: secret.masked.clone(),
                    });
                }
                return Ok(false);
            }
            other => return Err(other.into()),
        }
    }
    let mut allowed = Vec::new();
    let mut occurrences = Vec::new();
    let mut seen = existing.clone();
    for item in pending {
        if existing.contains(&item.value) {
            continue;
        }
        if seen.contains(&item.value) {
            occurrences.push(item);
            continue;
        }
        match context.check_properties(Some(
            &serde_json::to_value(&item.value).expect("serializes"),
        )) {
            Ok(()) => {
                seen.insert(item.value.clone());
                allowed.push(item.value.clone());
                occurrences.push(item);
            }
            Err(RuntimeError::SecretDetected(secret)) => {
                report.blocked_count += 1;
                report.blocked.push(BlockedWrite {
                    file: item.file.clone(),
                    detector: secret.detector.to_string(),
                    masked_excerpt: secret.masked,
                });
            }
            Err(other) => return Err(other.into()),
        }
    }
    if allowed.is_empty() {
        return Ok(false);
    }
    #[cfg(test)]
    l2_batch_tests::pause_before_batch().await;
    let mut attempted_files = Vec::new();
    let outcome = mutate_entity_with_context(
        rt,
        token,
        &context,
        id,
        &pending[0].file,
        report,
        |current| {
            attempted_files.clear();
            let mut entity = current?.clone();
            let mut list = read_list(&entity);
            let present: HashSet<T> = list.iter().cloned().collect();
            attempted_files.extend(
                occurrences
                    .iter()
                    .filter(|item| !present.contains(&item.value))
                    .map(|item| item.file.clone()),
            );
            if append_l2_pending(&mut list, &allowed) == 0 {
                return None;
            }
            let mut props = entity
                .properties
                .clone()
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            props.insert(key.into(), serde_json::to_value(list).expect("serializes"));
            entity.properties = Some(Value::Object(props));
            Some(entity)
        },
    )
    .await?;
    if outcome == RowMutationOutcome::Blocked {
        let refusal = report
            .blocked
            .pop()
            .expect("blocked mutation records refusal");
        report.blocked_count -= 1;
        for file in attempted_files {
            report.blocked_count += 1;
            report.blocked.push(BlockedWrite {
                file,
                detector: refusal.detector.clone(),
                masked_excerpt: refusal.masked_excerpt.clone(),
            });
        }
    }
    Ok(outcome.wrote())
}

/// Upsert (create or refresh) the `contains` edge from a declaration's
/// owning module to the declaration itself.
#[derive(Clone, Copy)]
pub(super) struct L2ContainmentStamp<'a> {
    pub(super) language: &'a str,
    pub(super) sweep_time: DateTime<Utc>,
    pub(super) run_id: Uuid,
}

pub(super) async fn stamp_containment_edge(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    owner_id: Uuid,
    child_id: Uuid,
    stamp: L2ContainmentStamp<'_>,
    preserve_non_l2_metadata: bool,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let edge_id = edge_uuid(EdgeRelation::Contains, owner_id, child_id);
    let outcome = mutate_edge(rt, token, edge_id, |current| {
        let metadata = match current.and_then(|edge| edge.metadata.as_ref()) {
            Some(metadata)
                if preserve_non_l2_metadata
                    && !metadata
                        .get("l2_derived")
                        .and_then(Value::as_bool)
                        .unwrap_or(false) =>
            {
                metadata.clone()
            }
            _ => json!({
                "l2_derived": true,
                "language": stamp.language,
                "last_seen_at": stamp.sweep_time.to_rfc3339(),
                "l2_observed_run_id": stamp.run_id.to_string(),
            }),
        };
        Some(Edge {
            id: LinkId::from(edge_id),
            namespace: token.namespace().as_str().to_string(),
            source_id: owner_id,
            target_id: child_id,
            relation: EdgeRelation::Contains,
            weight: 1.0,
            created_at: current
                .map(|edge| edge.created_at)
                .unwrap_or(stamp.sweep_time),
            updated_at: stamp.sweep_time,
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
    Ok(())
}

/// Upsert one declaration's `concept` entity. Returns `Ok(None)` when the
/// runtime secret gate refuses the write (recorded in `report.blocked`,
/// keyed by `file_label`) — callers must treat this declaration as absent
/// from this sweep. Also returns the declaration's own absolute module path
/// (identical to `containing_module_path` for non-`Module` kinds; the
/// nested path for an inline `Module` declaration).
#[allow(clippy::too_many_arguments)]
pub(super) async fn upsert_declaration(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    containing_module_path: &str,
    decl: &ExtractedDeclaration,
    source_path: &str,
    source_revision: &str,
    sweep_time: DateTime<Utc>,
    run_id: Uuid,
    file_label: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<(Uuid, String)>, CodeSourceIngestError> {
    let canonical_kind = decl.kind.code_token();
    let id = symbol_uuid(
        source_project,
        language,
        containing_module_path,
        &decl.name,
        canonical_kind,
    );
    let own_module_path = if decl.kind == DeclKind::Module {
        format!("{containing_module_path}::{}", decl.name)
    } else {
        containing_module_path.to_string()
    };
    let now = ts(sweep_time);
    let outcome = mutate_entity(rt, token, id, file_label, report, |current| {
        let mut entity = current.cloned().unwrap_or_else(|| {
            Entity::new(token.namespace().as_str(), "concept", decl.name.clone())
                .with_entity_type(Some(canonical_kind))
        });
        let mut props = entity
            .properties
            .clone()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        props.insert("source_project".into(), json!(source_project));
        props.insert("language".into(), json!(language));
        // `module_path` is the containing module used in the UUID preimage for
        // every declaration, including inline-module declarations. The returned
        // `own_module_path` is only traversal context for that module's children.
        props.insert("module_path".into(), json!(containing_module_path));
        props.insert("source_path".into(), json!(source_path));
        props.insert("source_revision".into(), json!(source_revision));
        props.insert("content_hash".into(), json!(decl.content_hash));
        props.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
        props.insert("l2_observed_run_id".into(), json!(run_id.to_string()));
        entity.id = id;
        entity.namespace = token.namespace().as_str().to_string();
        entity.kind = "concept".to_string();
        entity.entity_type = Some(canonical_kind.to_string());
        entity.name = decl.name.clone();
        entity.description = decl.description.clone();
        entity.properties = Some(Value::Object(props));
        entity.updated_at = now;
        Some(entity)
    })
    .await?;
    if outcome == RowMutationOutcome::Blocked {
        return Ok(None);
    }
    if let Some(l2) = report.l2.as_mut() {
        match outcome {
            RowMutationOutcome::Created => l2.symbols_created += 1,
            RowMutationOutcome::Updated => l2.symbols_updated += 1,
            RowMutationOutcome::Unchanged | RowMutationOutcome::Blocked => {}
        }
    }
    Ok(Some((id, own_module_path)))
}

/// Remove the module's `declaration_ids` ownership stamp (a failed parse
/// leaves the module with source metadata but no current coverage stamp)
/// without touching any other property, including prior symbol
/// rows, which remain as history rather than being exported as current.
pub(super) async fn clear_l2_ownership(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    module_id: Uuid,
    file_label: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    mutate_entity(rt, token, module_id, file_label, report, |current| {
        let mut module = current?.clone();
        let mut props = module
            .properties
            .clone()
            .and_then(|value| value.as_object().cloned())?;
        let changed = [
            "declaration_ids",
            "l2_pending_impls",
            "l2_content_hash",
            "l2_scanner_identity_version",
        ]
        .into_iter()
        .any(|key| props.remove(key).is_some());
        if !changed {
            return None;
        }
        module.properties = Some(Value::Object(props));
        Some(module)
    })
    .await?;
    Ok(())
}

/// Persist one L2-selected Rust file's parse outcome. On failure, invalidate
/// current ownership without touching history; on success, upsert every declaration, its
/// containment edge, its same-project call/type-reference resolution, every
/// positive impl, then stamp the module's `declaration_ids` coverage.
#[allow(clippy::too_many_arguments)]
pub(super) async fn persist_l2_file(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    module_id: Uuid,
    module_path: &str,
    source_path: &str,
    source_revision: &str,
    content_hash: &str,
    parse: Result<&ExtractedFile, &str>,
    sweep_time: DateTime<Utc>,
    file_label: &str,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<Option<Vec<Uuid>>, CodeSourceIngestError> {
    let parsed = match parse {
        Err(message) => {
            if let Some(l2) = report.l2.as_mut() {
                l2.symbol_parse_failures += 1;
            }
            report
                .warnings
                .push(format!("L2 parse failed for {file_label}: {message}"));
            clear_l2_ownership(rt, token, module_id, file_label, report).await?;
            return Ok(None);
        }
        Ok(parsed) => parsed,
    };

    // Phase A: upsert every declaration's entity first, independent of
    // resolution order, so phase B's containment/dependency edges can
    // target any declaration in this file regardless of list position.
    let mut declaration_ids: Vec<Uuid> = Vec::new();
    let mut declared: Vec<(Uuid, String, &ExtractedDeclaration)> = Vec::new();
    let mut refused_inline_modules = BTreeSet::new();
    for decl in &parsed.declarations {
        let containing_module_path = resolve_module_path(module_path, &decl.module_segments);
        if refused_inline_modules.iter().any(|refused: &String| {
            containing_module_path == *refused
                || containing_module_path.starts_with(&format!("{refused}::"))
        }) {
            continue;
        }
        let Some((id, _own_path)) = upsert_declaration(
            rt,
            token,
            source_project,
            language,
            &containing_module_path,
            decl,
            source_path,
            source_revision,
            sweep_time,
            state.run_id,
            file_label,
            report,
        )
        .await?
        else {
            if decl.kind == DeclKind::Module {
                refused_inline_modules.insert(format!("{containing_module_path}::{}", decl.name));
            }
            continue; // gate-refused, already recorded in report.blocked
        };
        declaration_ids.push(id);
        declared.push((id, containing_module_path, decl));
    }
    let current_file_ids: BTreeSet<Uuid> = declaration_ids.iter().copied().collect();

    let observation_start = state.observed_natural_edge_ids.len();
    let mut file_references = Vec::new();
    let mut file_implementations = Vec::new();
    // Phase B: containment + same-project call/type-reference resolution.
    for (id, containing_module_path, decl) in &declared {
        let owner_id = declaration_owner_id(
            source_project,
            language,
            module_path,
            module_id,
            &decl.module_segments,
        );
        stamp_containment_edge(
            rt,
            token,
            owner_id,
            *id,
            L2ContainmentStamp {
                language,
                sweep_time,
                run_id: state.run_id,
            },
            false,
            report,
        )
        .await?;

        let mut pending_references = Vec::new();
        for call in &decl.calls {
            if let Some(reference) = resolve_l2_reference(
                rt,
                token,
                source_project,
                language,
                containing_module_path,
                *id,
                &current_file_ids,
                &call.segments,
                "call",
                sweep_time,
                state,
                report,
            )
            .await?
            {
                pending_references.push(PendingL2 {
                    value: reference,
                    file: file_label.to_owned(),
                });
            }
        }
        for type_ref in &decl.type_refs {
            if let Some(reference) = resolve_l2_reference(
                rt,
                token,
                source_project,
                language,
                containing_module_path,
                *id,
                &current_file_ids,
                &type_ref.segments,
                "type_reference",
                sweep_time,
                state,
                report,
            )
            .await?
            {
                pending_references.push(PendingL2 {
                    value: reference,
                    file: file_label.to_owned(),
                });
            }
        }
        file_references.extend(pending_references.iter().map(|pending| FileReference {
            declaration_id: *id,
            module_path: containing_module_path.clone(),
            reference: pending.value.clone(),
        }));
        // Preserve declaration history without granting it late edge authority.
        record_l2_pending_batch(
            rt,
            token,
            *id,
            "l2_unresolved_references",
            &pending_references,
            report,
        )
        .await?;
    }

    // Phase C: positive trait implementations.
    let mut pending_impls = Vec::new();
    for imp in &parsed.impls {
        let containing_module_path = resolve_module_path(module_path, &imp.module_segments);
        if !imp.type_path.is_empty() && !imp.trait_path.is_empty() {
            file_implementations.push(file_pending::FileImplementation {
                module_path: containing_module_path.clone(),
                implementation: L2PendingImpl {
                    type_path: imp.type_path.clone(),
                    trait_path: imp.trait_path.clone(),
                },
            });
        }
        if let Some(entry) = resolve_l2_implements(
            rt,
            token,
            source_project,
            language,
            &containing_module_path,
            &current_file_ids,
            &imp.type_path,
            &imp.trait_path,
            sweep_time,
            state,
            report,
        )
        .await?
        {
            pending_impls.push(PendingL2 {
                value: entry,
                file: file_label.to_owned(),
            });
        }
    }
    record_l2_pending_batch(
        rt,
        token,
        module_id,
        "l2_pending_impls",
        &pending_impls,
        report,
    )
    .await?;

    declaration_ids.sort();
    declaration_ids.dedup();
    if !stamp_l2_declarations(
        rt,
        token,
        module_id,
        &declaration_ids,
        &file_references,
        &state.observed_natural_edge_ids[observation_start..],
        &file_implementations,
        content_hash,
        file_label,
        report,
    )
    .await?
    {
        return Ok(None);
    }
    Ok(Some(declaration_ids))
}

pub(super) struct L2SweepInputs<'a> {
    pub(super) ingest_root: &'a Path,
    pub(super) snapshot: &'a SourceSnapshot,
    pub(super) sweep_time: DateTime<Utc>,
}

/// Walk every `.rs` file under `ingest_root`, ensure its project/module L2
/// ownership scaffolding exists, and (re)parse it when needed
/// Rust-only; other-language selections
/// never reach this function (`run_code_ingest` only calls it when L2 is
/// enabled, and L2 itself scans Rust exclusively).
pub(super) async fn run_l2_sweep(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    inputs: L2SweepInputs<'_>,
    project_ids: &mut HashMap<(String, String), Uuid>,
    previous_l2_sweep_stamps: &mut PreviousL2SweepStamps,
    report: &mut CodeSourceIngestReport,
) -> Result<L2SweepState, CodeSourceIngestError> {
    let L2SweepInputs {
        ingest_root,
        snapshot,
        sweep_time,
    } = inputs;
    const LANGUAGE: &str = "rust";
    let mut state = L2SweepState::new(previous_l2_sweep_stamps.run_id);
    let Some(ext) = imports::extension_for_language(LANGUAGE) else {
        return Ok(state);
    };
    let walk = match walk_source_files_on_worker(ingest_root, ext).await? {
        Ok(walk) => walk,
        Err(error) => {
            report
                .warnings
                .push(format!("walking {}: {error}", ingest_root.display()));
            return Ok(state);
        }
    };
    let SourceWalkResult {
        canonical_root: canonical_ingest_root,
        files,
        skipped_outside_root,
        skipped_non_regular,
        skipped_non_source,
    } = walk;
    for skipped in skipped_outside_root {
        report.warnings.push(format!(
            "L2 skipped source outside the canonical ingest root: {}",
            skipped.display()
        ));
        report.files_dropped_without_source_path += 1;
    }
    for skipped in skipped_non_regular {
        report.warnings.push(format!(
            "L2 skipped non-regular source: {}",
            skipped.display()
        ));
        report.files_dropped_without_source_path += 1;
    }
    for skipped in skipped_non_source {
        report.warnings.push(format!(
            "L2 skipped non-source traversal entry: {}",
            skipped.display()
        ));
    }
    if !files.is_empty() {
        record_observed_language(report, LANGUAGE);
    }

    // L2-only never requests the L1 manifest walk. Read only ancestors of
    // canonical source paths and reuse that parsed snapshot for every file.
    let files_for_manifests = files.clone();
    let root_for_manifests = canonical_ingest_root.clone();
    let (manifests, failures) = blocking_io(move || {
        manifest::discover_rust_manifests_for_sources(&files_for_manifests, &root_for_manifests)
    })
    .await?;
    record_manifest_failures(report, failures);
    let manifest_index = manifest::ManifestIndex::new(&manifests);

    // Resolve every encountered owner before any unchanged-file decision.
    // A later fallback file disqualifies the owner for the whole invocation,
    // including files encountered earlier through a governing manifest.
    let files: Vec<_> = files
        .into_iter()
        .filter_map(|file| {
            let governing =
                manifest_index.governing(file.parent()?, &canonical_ingest_root, LANGUAGE);
            let fallback = governing.is_none();
            let (proj_root, proj_name) = governing.unwrap_or_else(|| {
                (
                    canonical_ingest_root.clone(),
                    basename_project_name(ingest_root, &canonical_ingest_root),
                )
            });
            let owner = L2OwnerKey {
                source_project: proj_name.clone(),
                language: LANGUAGE.to_string(),
            };
            let whole = proj_root.starts_with(&canonical_ingest_root);
            state
                .owners
                .entry(owner.clone())
                .and_modify(|coverage| {
                    coverage.whole &= whole;
                    coverage.fallback |= fallback;
                })
                .or_insert_with(|| L2OwnerCoverage {
                    whole,
                    fallback,
                    file_label: file.display().to_string(),
                });
            if fallback {
                // This also discards authority captured by an earlier L1 or
                // L1.5 upsert. Completion retention is a separate decision.
                previous_l2_sweep_stamps.stamps.insert(owner, None);
            }
            Some((file, proj_root, proj_name))
        })
        .collect();

    for (file, proj_root, proj_name) in files {
        let Some(module_path) = imports::module_path_for_file(&file, &proj_root, LANGUAGE) else {
            report.files_skipped_without_module_path += 1;
            continue;
        };
        let (source_path, warnings, dropped) =
            derive_source_path_on_worker(&file, &canonical_ingest_root, &snapshot.root).await?;
        report.warnings.extend(warnings);
        report.files_dropped_without_source_path += dropped;
        let Some(source_path) = source_path else {
            continue;
        };
        let file_label = file.display().to_string();

        let Some(proj_id) = ensure_project_id(
            rt,
            token,
            project_ids,
            &proj_name,
            &file_label,
            LANGUAGE,
            true,
            sweep_time,
            previous_l2_sweep_stamps,
            report,
        )
        .await?
        else {
            continue;
        };

        let file_for_read = file.clone();
        let root_for_read = canonical_ingest_root.clone();
        #[cfg(test)]
        l2_recovery_tests::before_source_read(&file).await;
        let source =
            match blocking_io(move || read_l2_source(&root_for_read, &file_for_read)).await? {
                Ok(source) => source,
                Err(error) => {
                    record_source_read_failure(report, "L2", &file, error);
                    continue;
                }
            };
        let hash = source.hash().to_string();
        let refused = matches!(&source, L2Source::Refused { .. });

        let precomputed_module_id = module_uuid(&proj_name, LANGUAGE, &module_path);
        let existing_module = get_entity_opt(rt, token, precomputed_module_id).await?;
        let owner = L2OwnerKey {
            source_project: proj_name.clone(),
            language: LANGUAGE.to_string(),
        };
        let recovering = previous_l2_sweep_stamps
            .get(&owner)
            .and_then(Option::as_ref)
            .is_none();
        let needs_reparse = recovering
            || refused
            || existing_module
                .as_ref()
                .and_then(|entity| entity.properties.as_ref())
                .and_then(|properties| file_pending::read_file(properties, &file_label))
                .filter(|entry| entry.content_hash == hash && entry.natural_edge_ids.is_some())
                .is_none()
            || l2_needs_reparse(
                existing_module
                    .as_ref()
                    .and_then(|e| e.properties.as_ref())
                    .and_then(|p| p.get("l2_content_hash"))
                    .and_then(Value::as_str),
                existing_module
                    .as_ref()
                    .and_then(|e| e.properties.as_ref())
                    .and_then(|p| p.get("declaration_ids")),
                existing_module
                    .as_ref()
                    .and_then(|e| e.properties.as_ref())
                    .and_then(|p| p.get("l2_scanner_identity_version"))
                    .and_then(Value::as_u64),
                &hash,
            );

        let Some(module_id) = upsert_module(
            rt,
            token,
            &proj_name,
            LANGUAGE,
            &module_path,
            &source_path,
            &snapshot.revision,
            &hash,
            !needs_reparse,
            sweep_time,
            &file_label,
            report,
        )
        .await?
        else {
            continue;
        };

        stamp_containment_edge(
            rt,
            token,
            proj_id,
            module_id,
            L2ContainmentStamp {
                language: LANGUAGE,
                sweep_time,
                run_id: state.run_id,
            },
            true,
            report,
        )
        .await?;

        if !needs_reparse {
            let accepted = existing_module
                .as_ref()
                .and_then(|entity| entity.properties.as_ref())
                .and_then(|properties| file_pending::read_file(properties, &file_label))
                .expect("reuse requires accepted producer coverage");
            let declaration_ids = accepted.declaration_ids;
            let natural_edge_ids = accepted.natural_edge_ids.expect("checked inventory");
            if refresh_l2_declarations(
                rt,
                token,
                &proj_name,
                LANGUAGE,
                &source_path,
                &snapshot.revision,
                sweep_time,
                state.run_id,
                &file_label,
                &declaration_ids,
                &natural_edge_ids,
                previous_l2_sweep_stamps
                    .get(&owner)
                    .and_then(Option::as_ref),
                report,
            )
            .await?
            {
                state
                    .current_files
                    .insert((module_id, file_label.clone()), hash.clone());
                state.mark_current_module(module_id, &proj_name, LANGUAGE);
                state.mark_current_declarations(&declaration_ids, &proj_name, LANGUAGE);
                state
                    .unchanged_declarations
                    .extend(declaration_ids.iter().copied());
                state.current_natural_edge_ids.extend(natural_edge_ids);
                continue;
            }
        }

        clear_l2_ownership(rt, token, module_id, &file_label, report).await?;
        let parse_result = match source {
            L2Source::Ready { content, .. } => {
                let result = parse_rust_file_on_worker(content).await;
                #[cfg(test)]
                l2_recovery_tests::observe_parse(&file);
                result
            }
            L2Source::Refused { reason, .. } => Err(reason),
        };
        if let Some(declaration_ids) = persist_l2_file(
            rt,
            token,
            &proj_name,
            LANGUAGE,
            module_id,
            &module_path,
            &source_path,
            &snapshot.revision,
            &hash,
            parse_result.as_ref().map_err(String::as_str),
            sweep_time,
            &file_label,
            &mut state,
            report,
        )
        .await?
        {
            state
                .current_files
                .insert((module_id, file_label.clone()), hash.clone());
            state.mark_current_module(module_id, &proj_name, LANGUAGE);
            state.mark_current_declarations(&declaration_ids, &proj_name, LANGUAGE);
        }
    }
    Ok(state)
}

/// L2 synchronous re-resolve pass, run once after the whole L2 file walk
/// completes (mirrors L1.5's `reresolve_pass`): revisits every symbol
/// carrying accepted per-file call/type references and every module carrying pending
/// impls, and retries resolution against the now-fully-populated set. This
/// is what makes edge convergence independent of file-visit order within one
/// sweep, and lets a later sweep pick up targets that did not exist yet.
pub(super) async fn l2_reresolve_pass(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    sweep_time: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let sql = rt.sql();
    let no_current_file_ids = BTreeSet::new();
    if let Some(l2) = report.l2.as_mut() {
        l2.symbol_dependencies_unresolved = 0;
    }

    file_pending::reresolve(rt, token, sweep_time, state, report).await?;

    // Pass 2: pending positive impls on module entities.
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT id FROM entities WHERE deleted_at IS NULL \
                  AND json_extract(properties,'$.l2_pending_impls') IS NOT NULL"
                .into(),
            params: vec![],
            label: Some("code_ingest_l2_reresolve_impls".into()),
        })
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    let mut resolved = HashMap::new();
    for row in rows {
        let Some(module_id) = row_uuid(&row) else {
            continue;
        };
        if !state.current_modules.contains_key(&module_id) {
            continue;
        }
        let Some(module) = get_entity_opt(rt, token, module_id).await? else {
            continue;
        };
        let Some(source_project) = module
            .properties
            .as_ref()
            .and_then(|p| p.get("source_project"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let Some(language) = module
            .properties
            .as_ref()
            .and_then(|p| p.get("language"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let Some(module_path) = module
            .properties
            .as_ref()
            .and_then(|p| p.get("module_path"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let pending = module
            .properties
            .as_ref()
            .map(read_l2_pending_impls)
            .unwrap_or_default();
        if pending.is_empty() {
            continue;
        }
        let original_pending: HashSet<_> = pending.iter().cloned().collect();
        let mut still_pending = Vec::new();
        let mut resolved_any = false;
        for entry in pending {
            let type_id = find_first_current(
                state,
                &source_project,
                &language,
                symbol_candidate_ids_for_kinds(
                    &source_project,
                    &language,
                    &module_path,
                    &entry.type_path,
                    &["datatype"],
                ),
                &no_current_file_ids,
            );
            let trait_id = find_first_current(
                state,
                &source_project,
                &language,
                symbol_candidate_ids_for_kinds(
                    &source_project,
                    &language,
                    &module_path,
                    &entry.trait_path,
                    &["interface"],
                ),
                &no_current_file_ids,
            );
            match (type_id, trait_id) {
                (Some(type_id), Some(trait_id)) => {
                    upsert_l2_implements(
                        rt, token, type_id, trait_id, &language, sweep_time, state, report,
                    )
                    .await?;
                    resolved_any = true;
                    resolved.insert(
                        (module_id, entry),
                        edge_uuid(EdgeRelation::Implements, type_id, trait_id),
                    );
                }
                _ => still_pending.push(entry),
            }
        }
        if let Some(l2) = report.l2.as_mut() {
            l2.symbol_dependencies_unresolved += still_pending.len() as u64;
        }
        if resolved_any {
            let label = module_id.to_string();
            #[cfg(test)]
            l2_batch_tests::pause_before_rebase().await;
            mutate_entity(rt, token, module_id, &label, report, |current| {
                let mut module = current?.clone();
                let mut rebased = module
                    .properties
                    .as_ref()
                    .map(read_l2_pending_impls)
                    .unwrap_or_default();
                rebase_l2_pending(&mut rebased, &original_pending, &still_pending);
                let mut props = module
                    .properties
                    .clone()
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                if rebased.is_empty() {
                    props.remove("l2_pending_impls");
                } else {
                    props.insert(
                        "l2_pending_impls".into(),
                        serde_json::to_value(&rebased).expect("serializes"),
                    );
                }
                module.properties = Some(Value::Object(props));
                Some(module)
            })
            .await?;
        }
    }

    file_pending::record_resolved_implementations(rt, token, state, &resolved, report).await?;
    Ok(())
}

pub(super) fn row_uuid(row: &khive_storage::types::SqlRow) -> Option<Uuid> {
    use khive_storage::types::SqlValue;
    match row.get("id") {
        Some(SqlValue::Uuid(u)) => Some(*u),
        Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
        _ => None,
    }
}
