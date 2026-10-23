use std::str::FromStr;

use khive_storage::types::SqlValue;
use uuid::Uuid;

#[cfg(any(test, feature = "fault-injection"))]
use super::{consume_fault, PREFIX_RESOLVE_FAIL_NS};
use super::{resolve_prefix_statement, uuid_prefix_bounds, Resolved};
use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::{KhiveRuntime, NamespaceToken};

impl KhiveRuntime {
    /// Resolve a short UUID prefix (8+ hex chars) to a full UUID.
    ///
    /// Searches entities, notes, and edges tables for a UUID starting with the
    /// given prefix, scoped to the caller's primary namespace only. Returns
    /// `Ok(Some(uuid))` if exactly one match is found, `Ok(None)` if no
    /// matches, or an error if ambiguous (multiple matches).
    pub async fn resolve_prefix(
        &self,
        token: &NamespaceToken,
        prefix: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        let namespaces = [token.namespace().as_str().to_owned()];
        self.resolve_prefix_inner(Some(&namespaces), prefix, false, false)
            .await
    }

    pub async fn resolve_prefix_including_deleted(
        &self,
        token: &NamespaceToken,
        prefix: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        let namespaces = [token.namespace().as_str().to_owned()];
        self.resolve_prefix_inner(Some(&namespaces), prefix, true, false)
            .await
    }

    /// Resolve a short UUID prefix (8+ hex chars) to a full UUID with NO
    /// namespace filter at all: mirrors `resolve_by_id`'s by-ID contract:
    /// by-ID resolution is namespace-agnostic, since the Gate (not
    /// storage-layer filtering) is the authz seam. Used by the four by-ID
    /// CRUD verbs (get/update/delete/merge) so their prefix path matches
    /// their already-unfiltered full-UUID path. No token param: unlike
    /// `resolve_prefix`, there is no namespace to derive from one.
    pub async fn resolve_prefix_unfiltered(&self, prefix: &str) -> RuntimeResult<Option<Uuid>> {
        self.resolve_prefix_inner(None, prefix, false, false).await
    }

    /// `resolve_prefix_unfiltered`, including soft-deleted rows — used by the
    /// hard-delete by-ID path.
    pub async fn resolve_prefix_unfiltered_including_deleted(
        &self,
        prefix: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        self.resolve_prefix_inner(None, prefix, true, false).await
    }

    /// The configured multi-backend read inventory has completed base schema
    /// bootstrap. A missing table is a backend failure there, not an absent ID.
    pub(crate) async fn resolve_prefix_for_kg_read(
        &self,
        prefix: &str,
        include_deleted: bool,
    ) -> RuntimeResult<Option<Uuid>> {
        self.resolve_prefix_inner(None, prefix, include_deleted, true)
            .await
    }

    /// Shared indexed prefix-range lookup over an explicit namespace set.
    ///
    /// `namespaces` selects the lookup scope: `Some(&[ns])` reproduces the
    /// historical primary-only behaviour (`resolve_prefix` /
    /// `resolve_prefix_including_deleted`); `None` applies
    /// no namespace predicate at all (`resolve_prefix_unfiltered*`).
    /// Ambiguity (a prefix matching more than one UUID, even across
    /// different namespaces in the set, or across all namespaces when
    /// unfiltered) is still an error: UUIDs are globally unique, so two
    /// distinct rows sharing a prefix always requires caller disambiguation —
    /// no cross-namespace dedup is needed or performed.
    async fn resolve_prefix_inner(
        &self,
        namespaces: Option<&[String]>,
        prefix: &str,
        include_deleted: bool,
        require_tables: bool,
    ) -> RuntimeResult<Option<Uuid>> {
        // Every caller is expected to pre-validate hex-only input, but this is
        // the single choke point every `resolve_prefix*` variant funnels
        // through, so re-validate here too. A prefix containing anything other
        // than hex digits and canonical hyphen separators (`%`, `_`, or other
        // injection-shaped input) never matches a real id and is rejected
        // before it can reach the range query.
        if !prefix.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Ok(None);
        }

        // Injection: check PREFIX_RESOLVE_FAIL_NS (armed by
        // `arm_prefix_resolve_fail_scoped(prefix)`), exercising the storage-failure path
        // a genuine pool checkout timeout or WAL contention would take.
        #[cfg(any(test, feature = "fault-injection"))]
        if consume_fault(&PREFIX_RESOLVE_FAIL_NS, prefix) {
            return Err(RuntimeError::Storage(
                khive_storage::StorageError::Timeout {
                    operation: "resolve_prefix".into(),
                },
            ));
        }

        let Some((lower, upper)) = uuid_prefix_bounds(prefix) else {
            return Ok(None);
        };

        let tables = [
            ("entities", true),
            ("notes", true),
            ("events", false),
            ("graph_edges", false),
        ];

        // A UUID can legitimately exist in more than one scanned table
        // (e.g. an entity id string that also happens to be an edge id — the
        // lookup is a text-prefix range across independent tables, not a
        // substrate-exclusive lookup). Without dedup, a single record hit
        // twice across tables inflated `matches.len()` past 1 and produced a
        // false `AmbiguousPrefix` naming the SAME UUID twice. `seen` tracks
        // UUIDs already pushed so `matches` (and thus every length check,
        // including the early-exit below) reflects DISTINCT UUIDs only.
        let mut matches: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut reader = self.sql().reader().await.map_err(RuntimeError::Storage)?;

        for (table, has_deleted_at) in tables {
            let sql = resolve_prefix_statement(
                table,
                has_deleted_at,
                include_deleted,
                namespaces,
                &lower,
                &upper,
            );
            match reader.query_all(sql).await {
                Ok(rows) => {
                    for row in rows {
                        if let Some(col) = row.columns.first() {
                            if let SqlValue::Text(s) = &col.value {
                                if seen.insert(s.clone()) {
                                    matches.push(s.clone());
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !require_tables && msg.contains("no such table") {
                        continue;
                    }
                    return Err(RuntimeError::Storage(e));
                }
            }
            if matches.len() > 1 {
                break;
            }
        }

        // Sidecar-resident event rows (events-split lane) are invisible to the
        // main-store scan above; a prefix naming one must still resolve, and a
        // prefix colliding across the two files must still read as ambiguous,
        // so the sidecar scan merges into the same `matches`/`seen` set.
        if matches.len() <= 1 {
            if let Some(sidecar_sql) = self.events_sidecar_sql_read_only()? {
                // The main-store statement, so the sidecar's candidates come back in
                // the same id order rather than whichever index the planner picks.
                let mut sql = resolve_prefix_statement(
                    "events",
                    false,
                    include_deleted,
                    namespaces,
                    &lower,
                    &upper,
                );
                sql.label = Some("resolve_prefix.events_sidecar".into());
                let mut sidecar_reader =
                    sidecar_sql.reader().await.map_err(RuntimeError::Storage)?;
                match sidecar_reader.query_all(sql).await {
                    Ok(rows) => {
                        for row in rows {
                            if let Some(col) = row.columns.first() {
                                if let SqlValue::Text(s) = &col.value {
                                    if seen.insert(s.clone()) {
                                        matches.push(s.clone());
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if require_tables || !msg.contains("no such table") {
                            return Err(RuntimeError::Storage(e));
                        }
                    }
                }
            }
        }

        match matches.len() {
            0 => Ok(None),
            1 => {
                let uuid = Uuid::from_str(&matches[0])
                    .map_err(|e| RuntimeError::Internal(format!("stored UUID is invalid: {e}")))?;
                Ok(Some(uuid))
            }
            _ => {
                let uuids: Vec<uuid::Uuid> = matches
                    .iter()
                    .filter_map(|s| Uuid::from_str(s).ok())
                    .collect();
                Err(RuntimeError::AmbiguousPrefix {
                    prefix: prefix.to_string(),
                    matches: uuids,
                })
            }
        }
    }

    /// Resolve a UUID to its substrate kind with NO namespace filter.
    ///
    /// By-ID contract: UUID v4 is globally unique: by-ID substrate
    /// inference must return the record regardless of caller namespace.  Used by
    /// the public `update` and `delete` verb handlers when no explicit `kind` is
    /// supplied.
    ///
    /// Does NOT consult the visible set or the primary-namespace check.  The
    /// token is still required to route to the correct backend pool but its
    /// namespace value is not used as a filter.
    pub async fn resolve_by_id(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        self.resolve_records(token, id, false, None, false).await
    }

    /// Resolve a UUID to its substrate kind with NO namespace filter, including
    /// soft-deleted rows.
    ///
    /// Used by the hard-delete path when no explicit `kind` is supplied, so
    /// already-soft-deleted records can still be located by UUID alone.
    pub async fn resolve_by_id_including_deleted(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        self.resolve_records(token, id, true, None, false).await
    }

    /// Resolve a UUID to its substrate kind by trying entity, then note, then event stores.
    ///
    /// Returns `None` if the UUID is not found in any substrate.
    /// Cost: at most 3 store lookups per call (cheap for v0.1).
    pub async fn resolve(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        // Entity: use the namespace-checked getter (errors on mismatch/absent).
        match self.get_entity(token, id).await {
            Ok(entity) => return Ok(Some(Resolved::Entity(entity))),
            Err(RuntimeError::NotFound(_) | RuntimeError::NamespaceMismatch { .. }) => {}
            Err(e) => return Err(e),
        }

        // Note: storage get_note is ID-only — verify against visible set.
        if let Some(note) = self.notes(token)?.get_note(id).await? {
            if Self::ensure_namespace_visible(&note.namespace, token).is_ok() {
                return Ok(Some(Resolved::Note(note)));
            }
        }

        // Event: storage get_event is ID-only — verify against visible set.
        if let Some(event) = self.events(token)?.get_event(id).await? {
            if Self::ensure_namespace_visible(&event.namespace, token).is_ok() {
                return Ok(Some(Resolved::Event(event)));
            }
        }

        Ok(None)
    }

    /// Resolve a UUID to its substrate kind with NO namespace filter, for edge
    /// endpoint validation.
    ///
    /// `link` and `create`'s `annotates` targets consume by-ID endpoints, so
    /// their existence check must follow the same by-ID contract as `get()`:
    /// by-ID ops are namespace-agnostic: the Gate, not storage-layer
    /// filtering, is the authz seam. Mirrors `resolve_by_id`
    /// (entity + note, unfiltered) and additionally resolves events,
    /// unfiltered, so edge endpoint validation resolves exactly what `get()`
    /// resolves regardless of the caller's namespace.
    pub async fn resolve_edge_endpoint(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        self.resolve_records(token, id, false, None, true).await
    }

    /// Resolve a UUID to its substrate kind using primary-namespace-only enforcement.
    ///
    /// Unlike `resolve`, never consults the visible set. Use from GTD dependency
    /// validation paths where strict primary ownership is required.
    pub async fn resolve_primary(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        self.resolve_records(token, id, false, Some(token.namespace().as_str()), true)
            .await
    }

    /// Resolve a UUID to its substrate kind, including soft-deleted rows.
    ///
    /// Used exclusively by the hard-delete path to locate records that have
    /// already been soft-deleted. Namespace isolation is still enforced.
    pub async fn resolve_including_deleted(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        self.resolve_records(token, id, true, Some(token.namespace().as_str()), true)
            .await
    }

    // Preserve entity-before-note-before-event lookup and lazy store access. A namespace
    // mismatch falls through; None deliberately leaves by-ID reads namespace-agnostic.
    // Tombstones apply only to entities/notes, and by-ID CRUD never probes events.
    async fn resolve_records(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        include_deleted: bool,
        namespace: Option<&str>,
        include_events: bool,
    ) -> RuntimeResult<Option<Resolved>> {
        let entity = if include_deleted {
            self.entities(token)?
                .get_entity_including_deleted(id)
                .await?
        } else {
            self.entities(token)?.get_entity(id).await?
        };
        if let Some(entity) = entity {
            if namespace.is_none_or(|ns| Self::ensure_namespace(&entity.namespace, ns).is_ok()) {
                return Ok(Some(Resolved::Entity(entity)));
            }
        }

        let note = if include_deleted {
            self.notes(token)?.get_note_including_deleted(id).await?
        } else {
            self.notes(token)?.get_note(id).await?
        };
        if let Some(note) = note {
            if namespace.is_none_or(|ns| Self::ensure_namespace(&note.namespace, ns).is_ok()) {
                return Ok(Some(Resolved::Note(note)));
            }
        }

        if include_events {
            if let Some(event) = self.events(token)?.get_event(id).await? {
                if namespace.is_none_or(|ns| Self::ensure_namespace(&event.namespace, ns).is_ok()) {
                    return Ok(Some(Resolved::Event(event)));
                }
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
#[path = "identity_reads/event_lookup_tests.rs"]
mod event_lookup_tests;
