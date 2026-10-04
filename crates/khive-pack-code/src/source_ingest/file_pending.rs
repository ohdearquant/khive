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
pub(super) struct FilePending {
    pub(super) content_hash: String,
    pub(super) declaration_ids: Vec<Uuid>,
    pub(super) references: Vec<FileReference>,
}
pub(super) fn read_file(properties: &Value, file: &str) -> Option<FilePending> {
    let entry: FilePending =
        serde_json::from_value(properties.get("l2_file_pending")?.get(file)?.clone()).ok()?;
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
    content_hash: &str,
    file_label: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<bool, CodeSourceIngestError> {
    let mut seen = HashSet::new();
    let references: Vec<_> = references
        .iter()
        .filter(|reference| seen.insert((*reference).clone()))
        .cloned()
        .collect();
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
            file_label.to_owned(),
            json!(FilePending {
                content_hash: content_hash.to_owned(),
                declaration_ids: declaration_ids.to_vec(),
                references: references.to_vec()
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
        if remaining == entry.references {
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
            props
                .get_mut("l2_file_pending")?
                .as_object_mut()?
                .insert(file.clone(), json!(fresh));
            module.properties = Some(Value::Object(props));
            Some(module)
        })
        .await?;
    }
    Ok(())
}
