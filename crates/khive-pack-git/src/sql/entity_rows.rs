//! Closed entity-row selections for the Git pack.
//!
//! Values are bound as before; every identifier, predicate, projection and order
//! is selected here. Callers keep their reader lifetime and tolerant row decoding.

use khive_storage::types::{SqlStatement, SqlValue};

pub(crate) enum EntityRows<'a> {
    ProjectsBySlug(&'a str),
    ProjectsByLegacyRepoUrl(&'a str),
    ProjectsWithoutCanonicalSlug(&'a str),
    SoftDeletedProjectsWithoutCanonicalSlug(&'a str),
    OrphanedProjects {
        identity: &'a str,
        repo_url: &'a str,
    },
    DocumentsByPath {
        path: &'a str,
        file_name: &'a str,
        like_pattern: &'a str,
    },
    CodeModulesBySnapshotPath(&'a str),
}

impl EntityRows<'_> {
    pub(crate) fn statement(self, namespace: &str) -> SqlStatement {
        let (columns, kind, deletion, predicate, order, values, label) = match self {
            Self::ProjectsBySlug(value) => (
                "id",
                "kind='project'",
                "IS NULL",
                "json_extract(properties,'$.repo_slug')=?2",
                " ORDER BY created_at ASC, id ASC",
                [Some(value), None, None],
                "git_digest_find_projects_by_slug",
            ),
            Self::ProjectsByLegacyRepoUrl(value) => (
                "id",
                "kind='project'",
                "IS NULL",
                "json_extract(properties,'$.repo_slug') IS NULL AND json_extract(properties,'$.repo_url')=?2",
                " ORDER BY created_at ASC, id ASC",
                [Some(value), None, None],
                "git_digest_find_projects_by_legacy_repo_url",
            ),
            Self::ProjectsWithoutCanonicalSlug(value) => (
                "id, json_extract(properties,'$.repo_url') AS repo_url",
                "kind='project'",
                "IS NULL",
                "json_extract(properties,'$.repo_url') IS NOT NULL AND (json_extract(properties,'$.repo_slug') IS NULL OR json_extract(properties,'$.repo_slug')<>?2)",
                " ORDER BY created_at ASC, id ASC",
                [Some(value), None, None],
                "git_digest_find_projects_without_canonical_slug",
            ),
            Self::SoftDeletedProjectsWithoutCanonicalSlug(value) => (
                "id, deleted_at, json_extract(properties,'$.repo_url') AS repo_url",
                "kind='project'",
                "IS NOT NULL",
                "json_extract(properties,'$.repo_url') IS NOT NULL AND (json_extract(properties,'$.repo_slug') IS NULL OR json_extract(properties,'$.repo_slug')<>?2)",
                "",
                [Some(value), None, None],
                "git_digest_find_soft_deleted_projects_without_canonical_slug",
            ),
            Self::OrphanedProjects { identity, repo_url } => (
                "id, deleted_at",
                "kind='project'",
                "IS NOT NULL",
                "(json_extract(properties,'$.repo_slug')=?2 OR json_extract(properties,'$.repo_url')=?3)",
                "",
                [Some(identity), Some(repo_url), None],
                "git_digest_find_orphaned_anchor",
            ),
            Self::DocumentsByPath {
                path,
                file_name,
                like_pattern,
            } => (
                "id",
                "kind='document'",
                "IS NULL",
                "(json_extract(properties,'$.source_uri')=?2 OR json_extract(properties,'$.source_uri') LIKE ?4 ESCAPE '\\' OR (json_extract(properties,'$.source_uri') IS NULL AND name=?3))",
                " ORDER BY CASE WHEN json_extract(properties,'$.source_uri')=?2 THEN 0 WHEN json_extract(properties,'$.source_uri') LIKE ?4 ESCAPE '\\' THEN 1 ELSE 2 END, id LIMIT 1",
                [Some(path), Some(file_name), Some(like_pattern)],
                "git_ingest_find_document_for_path",
            ),
            Self::CodeModulesBySnapshotPath(value) => (
                "id, json_extract(properties,'$.source_path') AS source_path",
                "kind='concept' AND entity_type='module'",
                "IS NULL",
                "json_type(properties,'$.source_path')='text' AND json_extract(properties,'$.source_revision')=?2",
                " ORDER BY source_path, id",
                [Some(value), None, None],
                "git_ingest_load_code_modules_by_snapshot_path",
            ),
        };
        let mut params = Vec::with_capacity(4);
        params.push(SqlValue::Text(namespace.to_owned()));
        params.extend(
            values
                .into_iter()
                .flatten()
                .map(|value| SqlValue::Text(value.to_owned())),
        );
        SqlStatement {
            sql: format!(
                "SELECT {columns} FROM entities WHERE {kind} AND namespace=?1 AND deleted_at {deletion} AND {predicate}{order}"
            ),
            params,
            label: Some(label.into()),
        }
    }
}
