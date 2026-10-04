//! Accepted unresolved references belong to a physical producing file.
use super::*;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub(super) struct FileReference {
    pub(super) declaration_id: Uuid,
    pub(super) module_path: String,
    #[serde(flatten)]
    pub(super) reference: L2UnresolvedRef,
}
#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(super) struct FileImplementation {
    pub(super) module_path: String,
    #[serde(flatten)]
    pub(super) implementation: L2PendingImpl,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(super) struct FilePending {
    pub(super) content_hash: String,
    pub(super) declaration_ids: Vec<Uuid>,
    pub(super) references: Vec<FileReference>,
    #[serde(default)]
    pub(super) scanner_version: u64,
    // Missing inventory predates producer-owned edge coverage and requires parsing.
    pub(super) natural_edge_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    pub(super) implementations: Vec<FileImplementation>,
}
/// The object key of a file's entry: a digest of its label, so the host path
/// the label spells is never stored in module properties.
pub(super) fn file_key(file: &str) -> String {
    content_hash(file)
}
pub(super) fn read_file(properties: &Value, file: &str) -> Option<FilePending> {
    let stored = properties.get("l2_file_pending")?.get(file_key(file))?;
    let entry: FilePending = serde_json::from_value(stored.clone()).ok()?;
    if entry.scanner_version != RUST_L2_SCANNER_IDENTITY_VERSION {
        return None;
    }
    entry
        .references
        .iter()
        .all(|reference| entry.declaration_ids.contains(&reference.declaration_id))
        .then_some(entry)
}

/// Stamp the module's current-coverage `declaration_ids` (sorted, deduped)
/// and scanner identity version after a successful parse.
#[allow(clippy::too_many_arguments)]
pub(super) async fn stamp_l2_declarations(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    module_id: Uuid,
    declaration_ids: &[Uuid],
    references: &[FileReference],
    natural_edge_ids: &[Uuid],
    implementations: &[FileImplementation],
    content_hash: &str,
    file_label: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<bool, CodeSourceIngestError> {
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    for reference in references {
        if !seen.insert(reference.clone()) {
            continue;
        }
        // The per-item screen record_l2_pending_batch applies. A refused
        // reference was already reported there; it is only left out here, so
        // one refused reference cannot make the module row refuse the stamp.
        match secret_gate::check_json_at(
            &serde_json::to_value(&reference.reference).expect("serializes"),
            "entity",
            "properties",
        ) {
            Ok(()) => kept.push(reference.clone()),
            Err(RuntimeError::SecretDetected(_)) => {}
            Err(other) => return Err(other.into()),
        }
    }
    let references = kept;
    let mut kept = Vec::new();
    for implementation in implementations {
        // The existing pending-impl writer already reports refused items.
        // Keep their producer inventory out of the same stamped row as well.
        match secret_gate::check_json_at(
            &serde_json::to_value(&implementation.implementation).expect("serializes"),
            "entity",
            "properties",
        ) {
            Ok(()) => kept.push(implementation.clone()),
            Err(RuntimeError::SecretDetected(_)) => {}
            Err(other) => return Err(other.into()),
        }
    }
    let implementations = kept;
    let mut row_missing = false;
    let mut invalid_properties = false;
    let outcome = mutate_entity(rt, token, module_id, file_label, report, |current| {
        row_missing = current.is_none();
        let mut module = current?.clone();
        let Some(Value::Object(mut props)) = module.properties.clone() else {
            invalid_properties = true;
            return None;
        };
        props.insert(
            "declaration_ids".into(),
            json!(declaration_ids
                .iter()
                .map(Uuid::to_string)
                .collect::<Vec<_>>()),
        );
        let entries = props.entry("l2_file_pending").or_insert_with(|| json!({}));
        if !entries.is_object() {
            *entries = json!({});
        }
        entries.as_object_mut().expect("object").insert(
            file_key(file_label),
            json!(FilePending {
                content_hash: content_hash.to_owned(),
                declaration_ids: declaration_ids.to_vec(),
                references: references.to_vec(),
                scanner_version: RUST_L2_SCANNER_IDENTITY_VERSION,
                natural_edge_ids: Some(
                    natural_edge_ids
                        .iter()
                        .copied()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect()
                ),
                implementations: implementations.to_vec(),
            }),
        );
        props.insert("l2_content_hash".into(), json!(content_hash));
        props.insert(
            "l2_scanner_identity_version".into(),
            json!(RUST_L2_SCANNER_IDENTITY_VERSION),
        );
        module.properties = Some(Value::Object(props));
        Some(module)
    })
    .await?;
    if row_missing {
        report.warnings.push(format!(
            "L2 module {module_id} from this sweep was missing at stamp time; \
             declaration_ids not recorded"
        ));
    } else if invalid_properties {
        report.warnings.push(format!(
            "L2 module {module_id} has missing or non-object properties at stamp time; \
             declaration_ids not recorded"
        ));
    }
    Ok(!row_missing && !invalid_properties && outcome != RowMutationOutcome::Blocked)
}

pub(super) async fn reresolve(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    sweep_time: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let no_current_file_ids = BTreeSet::new();
    for ((module_id, file), observed_hash) in state.current_files.clone() {
        let Some(owner) = state.current_modules.get(&module_id).cloned() else {
            continue;
        };
        let Some(module) = get_entity_opt(rt, token, module_id).await? else {
            continue;
        };
        let Some(properties) = module.properties.as_ref() else {
            continue;
        };
        if properties["source_project"].as_str() != Some(owner.source_project.as_str())
            || properties["language"].as_str() != Some(owner.language.as_str())
        {
            continue;
        }
        let Some(entry) = read_file(properties, &file) else {
            continue;
        };
        if entry.content_hash != observed_hash {
            continue;
        }
        let observation_start = state.observed_natural_edge_ids.len();
        let mut remaining = Vec::new();
        for pending in &entry.references {
            let id = pending.declaration_id;
            if !state.is_current_declaration(
                id,
                &owner.source_project,
                &owner.language,
                &no_current_file_ids,
            ) {
                remaining.push(pending.clone());
                continue;
            }
            let reference = &pending.reference;
            let mut target = None;
            let mut suppressed_self_type = false;
            for candidate in symbol_candidate_ids(
                &owner.source_project,
                &owner.language,
                &pending.module_path,
                &reference.segments,
                &reference.evidence,
            ) {
                if candidate == id && reference.evidence == "type_reference" {
                    suppressed_self_type = true;
                    break;
                }
                if state.is_current_declaration(
                    candidate,
                    &owner.source_project,
                    &owner.language,
                    &no_current_file_ids,
                ) {
                    target = Some(candidate);
                    break;
                }
            }
            match target {
                Some(target_id) => {
                    upsert_l2_depends_on(
                        rt,
                        token,
                        id,
                        target_id,
                        &reference.evidence,
                        &owner.language,
                        sweep_time,
                        state,
                        report,
                    )
                    .await?;
                }
                None if suppressed_self_type => {}
                None => remaining.push(pending.clone()),
            }
        }
        if let Some(l2) = report.l2.as_mut() {
            l2.symbol_dependencies_unresolved += remaining.len() as u64;
        }
        let observed = &state.observed_natural_edge_ids[observation_start..];
        if remaining == entry.references && observed.is_empty() {
            continue;
        }
        let original: HashSet<_> = entry.references.iter().cloned().collect();
        #[cfg(test)]
        l2_batch_tests::pause_before_rebase().await;
        mutate_entity(rt, token, module_id, &file, report, |current| {
            let mut module = current?.clone();
            let mut props = module.properties.clone()?.as_object()?.clone();
            let mut fresh = read_file(&Value::Object(props.clone()), &file)?;
            if fresh.content_hash != entry.content_hash {
                return None;
            }
            let retained: Vec<_> = remaining
                .iter()
                .filter(|pending| fresh.references.contains(pending))
                .cloned()
                .collect();
            rebase_l2_pending(&mut fresh.references, &original, &retained);
            if let Some(ids) = fresh.natural_edge_ids.as_mut() {
                ids.extend(observed.iter().copied());
                ids.sort();
                ids.dedup();
            }
            props
                .get_mut("l2_file_pending")?
                .as_object_mut()?
                .insert(file_key(&file), json!(fresh));
            module.properties = Some(Value::Object(props));
            Some(module)
        })
        .await?;
    }
    Ok(())
}

/// Record only impl relations accepted from this producer's parse and actually
/// observed by the completed walk's resolution pass. Ambient module history
/// cannot become this file's edge inventory.
pub(super) async fn record_resolved_implementations(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    state: &L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let no_file_ids = BTreeSet::new();
    let observed: BTreeSet<_> = state.observed_natural_edge_ids.iter().copied().collect();
    for ((module_id, file), hash) in &state.current_files {
        let Some(owner) = state.current_modules.get(module_id) else {
            continue;
        };
        let Some(module) = get_entity_opt(rt, token, *module_id).await? else {
            continue;
        };
        let Some(properties) = module.properties.as_ref() else {
            continue;
        };
        if properties["source_project"].as_str() != Some(owner.source_project.as_str())
            || properties["language"].as_str() != Some(owner.language.as_str())
        {
            continue;
        }
        let Some(entry) = read_file(properties, file) else {
            continue;
        };
        if &entry.content_hash != hash {
            continue;
        }
        let mut accepted = BTreeSet::new();
        for item in &entry.implementations {
            let implementation = &item.implementation;
            let type_id = find_first_current(
                state,
                &owner.source_project,
                &owner.language,
                symbol_candidate_ids_for_kinds(
                    &owner.source_project,
                    &owner.language,
                    &item.module_path,
                    &implementation.type_path,
                    &["datatype"],
                ),
                &no_file_ids,
            );
            let trait_id = find_first_current(
                state,
                &owner.source_project,
                &owner.language,
                symbol_candidate_ids_for_kinds(
                    &owner.source_project,
                    &owner.language,
                    &item.module_path,
                    &implementation.trait_path,
                    &["interface"],
                ),
                &no_file_ids,
            );
            if let (Some(type_id), Some(trait_id)) = (type_id, trait_id) {
                let id = edge_uuid(EdgeRelation::Implements, type_id, trait_id);
                if observed.contains(&id) {
                    accepted.insert(id);
                }
            }
        }
        if accepted.is_empty()
            || entry
                .natural_edge_ids
                .as_ref()
                .is_some_and(|ids| accepted.iter().all(|id| ids.contains(id)))
        {
            continue;
        }
        mutate_entity(rt, token, *module_id, file, report, |current| {
            let mut module = current?.clone();
            let mut properties = module.properties.clone()?.as_object()?.clone();
            if properties.get("source_project").and_then(Value::as_str)
                != Some(owner.source_project.as_str())
                || properties.get("language").and_then(Value::as_str)
                    != Some(owner.language.as_str())
            {
                return None;
            }
            let mut fresh = read_file(&Value::Object(properties.clone()), file)?;
            if fresh != entry {
                return None;
            }
            let ids = fresh.natural_edge_ids.as_mut()?;
            ids.extend(accepted.iter().copied());
            ids.sort();
            ids.dedup();
            properties
                .get_mut("l2_file_pending")?
                .as_object_mut()?
                .insert(file_key(&file), json!(fresh));
            module.properties = Some(Value::Object(properties));
            Some(module)
        })
        .await?;
    }
    Ok(())
}
