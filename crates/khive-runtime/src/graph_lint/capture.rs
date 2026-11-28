use crate::{KhiveRuntime, NamespaceToken, RuntimeResult, VerbRegistry};
use khive_storage::{Edge, Entity, Note};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Already-visible edge and authorized endpoint observations. No backend handles escape.
#[derive(Clone, Debug)]
pub struct InspectedLintEdge {
    pub edge: Edge,
    pub source_live: bool,
    pub target_live: bool,
    pub source_soft_deleted: bool,
    pub target_soft_deleted: bool,
    pub contract_valid: Option<bool>,
}
/// Read-only observation owned by one invocation, retaining private mutation provenance.
pub struct LintCapture {
    pub(super) token: NamespaceToken,
    pub(super) entities: Vec<Entity>,
    pub(super) notes: Vec<Note>,
    pub(super) edges: Vec<InspectedLintEdge>,
    pub(super) owners: Vec<KhiveRuntime>,
    pub(super) record_owners: HashMap<Uuid, Vec<usize>>,
    pub(super) all_entities: HashMap<Uuid, Entity>,
    pub(super) all_notes: HashMap<Uuid, Note>,
    pub(super) all_edges: HashMap<Uuid, Edge>,
    pub(super) edge_guards: HashMap<Uuid, khive_storage::SqlStatement>,
}
impl LintCapture {
    pub fn entities(&self) -> &[Entity] {
        &self.entities
    }
    pub fn notes(&self) -> &[Note] {
        &self.notes
    }
    pub fn edges(&self) -> &[InspectedLintEdge] {
        &self.edges
    }
}

const PAGE: u32 = 256;
fn active() -> RuntimeResult<()> {
    khive_storage::ensure_request_read_active("knowledge.lint capture").map_err(Into::into)
}
fn visible(token: &NamespaceToken, namespace: &str) -> bool {
    token
        .visible_namespaces()
        .iter()
        .any(|ns| ns.as_str() == namespace)
}
/// A digest of the serialized record. Identity checks compare digests, so the capture does
/// not hold a third copy of every visible record (note bodies included) for them.
fn canonical<T: serde::Serialize>(value: &T) -> RuntimeResult<[u8; 32]> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| crate::RuntimeError::Internal(error.to_string()))?;
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&Sha256::digest(bytes));
    Ok(digest)
}
fn register<T: serde::Serialize>(
    identities: &mut HashMap<Uuid, (&'static str, [u8; 32])>,
    owners: &mut HashMap<Uuid, Vec<usize>>,
    id: Uuid,
    substrate: &'static str,
    value: &T,
    owner: usize,
) -> RuntimeResult<bool> {
    let value = canonical(value)?;
    if let Some((prior_substrate, prior)) = identities.get(&id) {
        if *prior_substrate != substrate || *prior != value {
            return Err(crate::RuntimeError::InvalidInput(format!(
                "contradictory visible graph records for {id}"
            )));
        }
        let ids = owners.entry(id).or_default();
        if !ids.contains(&owner) {
            ids.push(owner);
        }
        return Ok(false);
    }
    identities.insert(id, (substrate, value));
    owners.insert(id, vec![owner]);
    Ok(true)
}
impl VerbRegistry {
    /// Capture all registered physical graph owners under the caller's token.
    /// No snapshot or mutation ownership is inferred from read deduplication.
    pub async fn capture_graph_lint(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
    ) -> RuntimeResult<LintCapture> {
        khive_storage::await_request_read_phase(
            "knowledge.lint capture",
            self.capture_graph_lint_inner(runtime, token),
        )
        .await?
    }
    async fn capture_graph_lint_inner(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
    ) -> RuntimeResult<LintCapture> {
        let registered = self.graph_lint_runtimes(runtime);
        let backend_aliases: HashSet<_> = registered
            .iter()
            .map(|owner| owner.backend_id().as_str().to_owned())
            .collect();
        let mut owners: Vec<KhiveRuntime> = Vec::new();
        for owner in registered {
            if !owners
                .iter()
                .any(|existing| existing.shares_backend_storage_with(&owner))
            {
                owners.push(owner);
            }
        }
        let rules = self.all_edge_rules();
        let mut capture = LintCapture {
            token: token.clone(),
            entities: Vec::new(),
            notes: Vec::new(),
            edges: Vec::new(),
            owners,
            record_owners: HashMap::new(),
            all_entities: HashMap::new(),
            all_notes: HashMap::new(),
            all_edges: HashMap::new(),
            edge_guards: HashMap::new(),
        };
        let mut identities = HashMap::new();
        let mut all_edges = HashMap::new();
        let mut hidden = HashSet::new();
        let mailbox = crate::MailboxView {
            actor_id: token.actor().id.clone(),
            delegated: false,
            maintainer_actor: runtime
                .config()
                .actor_id
                .clone()
                .filter(|id| !id.is_empty()),
        };
        for (owner, runtime) in capture.owners.iter().enumerate() {
            let mut after = None;
            loop {
                active()?;
                let filter = khive_storage::EntityFilter {
                    tombstones: khive_storage::entity::EntityTombstones::All,
                    ..Default::default()
                };
                let (entities, next) = runtime
                    .list_entities_after_filtered(token, filter, after, PAGE)
                    .await?;
                for entity in entities {
                    if register(
                        &mut identities,
                        &mut capture.record_owners,
                        entity.id,
                        "entity",
                        &entity,
                        owner,
                    )? {
                        if entity.deleted_at.is_none() {
                            capture.entities.push(entity.clone());
                        }
                        capture.all_entities.insert(entity.id, entity);
                    }
                }
                match next {
                    None => break,
                    Some(next) if Some(next) == after => {
                        return Err(crate::RuntimeError::Internal(
                            "entity lint cursor did not advance".into(),
                        ))
                    }
                    Some(next) => after = Some(next),
                }
            }
            let store = runtime.notes(token)?;
            let filter = khive_storage::note::NoteFilter {
                namespaces: token
                    .visible_namespaces()
                    .iter()
                    .map(|ns| ns.as_str().to_owned())
                    .collect(),
                include_deleted: true,
                ..Default::default()
            };
            let mut after = None;
            loop {
                active()?;
                let page = store
                    .query_notes_filtered_after(token.namespace().as_str(), &filter, after, PAGE)
                    .await?;
                for note in page.items {
                    if !mailbox.permits_message_note(token, &note) {
                        hidden.insert(note.id);
                        continue;
                    }
                    if note.salience.is_some_and(|v| !v.is_finite())
                        || note.decay_factor.is_some_and(|v| !v.is_finite())
                    {
                        return Err(crate::RuntimeError::InvalidInput(
                            "nonfinite visible note value".into(),
                        ));
                    }
                    if register(
                        &mut identities,
                        &mut capture.record_owners,
                        note.id,
                        "note",
                        &note,
                        owner,
                    )? {
                        if note.deleted_at.is_none() {
                            capture.notes.push(note.clone());
                        }
                        capture.all_notes.insert(note.id, note);
                    }
                }
                match page.next_after {
                    None => break,
                    Some(next) if Some(next) == after => {
                        return Err(crate::RuntimeError::Internal(
                            "note lint cursor did not advance".into(),
                        ))
                    }
                    Some(next) => after = Some(next),
                }
            }
            for ns in token.visible_namespaces() {
                let mut after: Option<String> = None;
                loop {
                    active()?;
                    let rows = runtime
                        .sql()
                        .reader()
                        .await?
                        .query_all(khive_db::graph_lint::edge_page(
                            ns.as_str(),
                            after.as_deref(),
                            PAGE,
                        ))
                        .await?;
                    if rows.is_empty() {
                        break;
                    }
                    for row in rows {
                        let edge = khive_db::graph_lint::decode_edge(&row)?;
                        let id = Uuid::from(edge.id);
                        if register(
                            &mut identities,
                            &mut capture.record_owners,
                            id,
                            "edge",
                            &edge,
                            owner,
                        )? {
                            capture
                                .edge_guards
                                .insert(id, khive_db::graph_lint::edge_witness(&row)?);
                            all_edges.insert(id, edge);
                        }
                        after = Some(
                            row.text("id")
                                .map_err(|error| crate::RuntimeError::Internal(error.to_string()))?
                                .to_owned(),
                        );
                    }
                }
            }
        }
        // Resolve endpoint absence separately from hidden records. By-ID APIs
        // alone confer no visibility: every returned row is screened again.
        let endpoint_ids: HashSet<_> = all_edges
            .values()
            .flat_map(|edge| [edge.source_id, edge.target_id])
            .collect();
        let mut events = HashMap::new();
        for id in endpoint_ids {
            active()?;
            // The listing passes above captured this record and recorded its owners. The
            // capture holds no snapshot, so reading it again would compare it with a later
            // state of the store; only the event store, which no listing covers, is probed.
            let listed = identities.contains_key(&id);
            for (owner, runtime) in capture.owners.iter().enumerate() {
                if !listed {
                    if let Some(entity) = runtime.get_entity_including_deleted(token, id).await? {
                        if !visible(token, &entity.namespace) {
                            hidden.insert(id);
                            continue;
                        }
                        register(
                            &mut identities,
                            &mut capture.record_owners,
                            id,
                            "entity",
                            &entity,
                            owner,
                        )?;
                        capture.all_entities.entry(id).or_insert(entity);
                    }
                    if let Some(note) = runtime.get_note_including_deleted(token, id).await? {
                        if !visible(token, &note.namespace)
                            || !mailbox.permits_message_note(token, &note)
                        {
                            hidden.insert(id);
                            continue;
                        }
                        register(
                            &mut identities,
                            &mut capture.record_owners,
                            id,
                            "note",
                            &note,
                            owner,
                        )?;
                        capture.all_notes.entry(id).or_insert(note);
                    }
                    if let Some(edge) = runtime.get_edge_including_deleted(token, id).await? {
                        if !visible(token, &edge.namespace) {
                            hidden.insert(id);
                            continue;
                        }
                        register(
                            &mut identities,
                            &mut capture.record_owners,
                            id,
                            "edge",
                            &edge,
                            owner,
                        )?;
                    }
                }
                if let Some(event) = runtime.events(token)?.get_event(id).await? {
                    if !visible(token, &event.namespace) {
                        hidden.insert(id);
                        continue;
                    }
                    register(
                        &mut identities,
                        &mut capture.record_owners,
                        id,
                        "event",
                        &event,
                        owner,
                    )?;
                    events.insert(id, event);
                }
            }
        }
        // Annotation chains must not reveal a hidden edge through its UUID.
        loop {
            let before = hidden.len();
            for (id, edge) in &all_edges {
                if hidden.contains(&edge.source_id) || hidden.contains(&edge.target_id) {
                    hidden.insert(*id);
                }
            }
            if before == hidden.len() {
                break;
            }
        }
        for edge in all_edges.values().filter(|edge| edge.deleted_at.is_none()) {
            let id = Uuid::from(edge.id);
            if hidden.contains(&id) {
                continue;
            }
            if edge
                .target_backend
                .as_ref()
                .is_some_and(|name| !backend_aliases.contains(name))
            {
                return Err(crate::RuntimeError::Unconfigured(
                    "lint edge endpoint backend is unavailable".into(),
                ));
            }
            let resolved = |id| {
                capture
                    .all_entities
                    .get(&id)
                    .cloned()
                    .map(crate::Resolved::Entity)
                    .or_else(|| {
                        capture
                            .all_notes
                            .get(&id)
                            .cloned()
                            .map(crate::Resolved::Note)
                    })
                    .or_else(|| events.get(&id).cloned().map(crate::Resolved::Event))
            };
            let live = |id| {
                capture
                    .all_entities
                    .get(&id)
                    .is_some_and(|record| record.deleted_at.is_none())
                    || capture
                        .all_notes
                        .get(&id)
                        .is_some_and(|record| record.deleted_at.is_none())
                    || all_edges
                        .get(&id)
                        .is_some_and(|record| record.deleted_at.is_none())
                    || events.contains_key(&id)
            };
            let deleted = |id| {
                capture
                    .all_entities
                    .get(&id)
                    .is_some_and(|record| record.deleted_at.is_some())
                    || capture
                        .all_notes
                        .get(&id)
                        .is_some_and(|record| record.deleted_at.is_some())
                    || all_edges
                        .get(&id)
                        .is_some_and(|record| record.deleted_at.is_some())
            };
            let src = resolved(edge.source_id);
            let tgt = resolved(edge.target_id);
            let validator = &capture.owners[capture.record_owners[&id][0]];
            let contract_valid = if edge.relation == khive_storage::EdgeRelation::Annotates
                && all_edges.contains_key(&edge.target_id)
            {
                // The shared validator accepts annotates -> any resolved substrate;
                // graph edges have no Resolved variant. Reuse its source/self-loop
                // checks with the actual note source as the presence witness.
                Some(
                    validator
                        .validate_link_endpoints_by_resolved_with_rules(
                            edge.source_id,
                            edge.target_id,
                            edge.relation,
                            src.as_ref(),
                            src.as_ref(),
                            &rules,
                        )
                        .is_ok(),
                )
            } else if (all_edges.contains_key(&edge.source_id)
                && (tgt.is_some() || all_edges.contains_key(&edge.target_id)))
                || (all_edges.contains_key(&edge.target_id) && src.is_some())
            {
                // Graph edges can only be annotation targets; the ordinary
                // endpoint validator has no Resolved::Edge representation.
                Some(false)
            } else if src.is_some() && tgt.is_some() {
                Some(
                    validator
                        .validate_link_endpoints_by_resolved_with_rules(
                            edge.source_id,
                            edge.target_id,
                            edge.relation,
                            src.as_ref(),
                            tgt.as_ref(),
                            &rules,
                        )
                        .is_ok(),
                )
            } else {
                None
            };
            capture.edges.push(InspectedLintEdge {
                edge: edge.clone(),
                source_live: live(edge.source_id),
                target_live: live(edge.target_id),
                source_soft_deleted: deleted(edge.source_id),
                target_soft_deleted: deleted(edge.target_id),
                contract_valid,
            });
        }
        capture.entities.sort_by_key(|record| record.id);
        capture.notes.sort_by_key(|record| record.id);
        capture
            .edges
            .sort_by_key(|record| Uuid::from(record.edge.id));
        capture.all_edges = all_edges;
        active()?;
        Ok(capture)
    }
}
