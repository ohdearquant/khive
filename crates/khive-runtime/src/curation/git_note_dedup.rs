//! Transactional evidence checks for the explicit git-note repair operator.
//!
//! Normal ingest and ordinary note merge remain duplicate-tolerant. The operator
//! must provide exact note revisions; project lineage and URL evidence are read
//! again on the merge writer connection before the first mutation.

use super::*;

const MAX_LINEAGE_HOPS: usize = 64;
const MAX_URL_CANDIDATES: usize = 10_000;

/// Expected grouping evidence from one bounded operator census.
#[derive(Clone, Debug)]
pub struct GitNoteMergeGuard {
    pub project_id: Uuid,
    pub kind: String,
    pub number: i64,
    pub into_version: i64,
    pub from_version: i64,
    /// Only for a numberless placeholder: exact stored URL on both records.
    pub placeholder_url: Option<String>,
}

/// The committed revision lets the operator merge another planned duplicate
/// without silently accepting an intervening edit through a post-commit reread.
#[derive(Debug)]
pub struct GuardedGitNoteMerge {
    pub summary: MergeSummary,
    pub kept_version: i64,
}

impl KhiveRuntime {
    pub async fn merge_git_note_guarded(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        guard: GitNoteMergeGuard,
        dry_run: bool,
    ) -> RuntimeResult<GuardedGitNoteMerge> {
        if !matches!(guard.kind.as_str(), "issue" | "pull_request")
            || guard.number <= 0
            || guard.into_version <= 0
            || guard.from_version <= 0
        {
            return Err(RuntimeError::InvalidInput(
                "invalid git-note merge evidence".into(),
            ));
        }
        let store = self.notes(token)?;
        let into = store
            .get_note(into_id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound("git-note survivor not found".into()))?;
        let from = store
            .get_note(from_id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound("git-note duplicate not found".into()))?;
        let content = if into.content == from.content {
            ContentMergeStrategy::PreferInto
        } else {
            ContentMergeStrategy::Append
        };
        let (summary, kept_version) = self
            .merge_note_with_guard(
                token,
                into_id,
                from_id,
                EntityDedupMergePolicy::PreferInto,
                content,
                dry_run,
                Some("explicit git-note duplicate repair".into()),
                Some(guard),
            )
            .await?;
        Ok(GuardedGitNoteMerge {
            summary,
            kept_version,
        })
    }
}

fn refusal(message: &str) -> MergeSqlError {
    MergeSqlError::Refusal(RuntimeError::Khive(KhiveError::conflict(format!(
        "git-note repair evidence changed or is ambiguous: {message}"
    ))))
}

fn number(note: &Note) -> Option<i64> {
    note.properties
        .as_ref()?
        .get("number")?
        .as_i64()
        .filter(|value| *value > 0)
}

/// Accept a bounded absolute HTTP(S) forge URL without credentials, query or
/// fragment. The parsed form validates syntax only; callers compare the original
/// stored strings byte-for-byte and never fetch or normalize them.
pub fn valid_git_note_forge_url(value: &str) -> bool {
    if value.len() > 4096
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || value.contains('\\')
    {
        return false;
    }
    let Some((scheme, remainder)) = value.split_once("://") else {
        return false;
    };
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        || remainder
            .split('/')
            .next()
            .is_none_or(|authority| authority.is_empty() || authority.contains('@'))
    {
        return false;
    }
    let Ok(parsed) = url::Url::parse(value) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && parsed.path() != "/"
}

fn canonical_project_exists(
    conn: &rusqlite::Connection,
    namespace: &str,
    project: Uuid,
) -> Result<bool, MergeSqlError> {
    Ok(conn.query_row(
        crate::sql!("git_note_dedup_canonical_project_select"),
        rusqlite::params![project.to_string(), namespace],
        |row| row.get(0),
    )?)
}

fn project_reaches(
    conn: &rusqlite::Connection,
    namespace: &str,
    mut project: Uuid,
    canonical: Uuid,
    budget: &mut MergeTxBudget,
) -> Result<bool, MergeSqlError> {
    let mut seen = HashSet::new();
    for _ in 0..=MAX_LINEAGE_HOPS {
        if project == canonical {
            return Ok(true);
        }
        if !seen.insert(project) {
            return Ok(false);
        }
        budget.charge(1, 128, "checking git project lineage")?;
        let next: Option<Option<String>> = conn
            .query_row(
                crate::sql!("git_note_dedup_lineage_next_select"),
                rusqlite::params![project.to_string(), namespace],
                |row| row.get(0),
            )
            .optional()?;
        let Some(next) = next
            .flatten()
            .and_then(|value| Uuid::parse_str(&value).ok())
        else {
            return Ok(false);
        };
        project = next;
    }
    Ok(false)
}

fn membership(
    conn: &rusqlite::Connection,
    namespace: &str,
    note: Uuid,
    project: Option<&str>,
    canonical: Uuid,
    budget: &mut MergeTxBudget,
) -> Result<bool, MergeSqlError> {
    let Some(project) = project.and_then(|value| Uuid::parse_str(value).ok()) else {
        return Ok(false);
    };
    if !project_reaches(conn, namespace, project, canonical, budget)? {
        return Ok(false);
    }
    budget.charge(1, 128, "checking git project annotations")?;
    // Other live project annotations contradict a single-project grouping even
    // when that project lives in another namespace. We disclose no foreign ID.
    Ok(conn.query_row(
        crate::sql!("git_note_dedup_membership_select"),
        rusqlite::params![note.to_string(), canonical.to_string(), namespace],
        |row| row.get(0),
    )?)
}

pub(super) fn validate(
    conn: &rusqlite::Connection,
    namespace: &str,
    into: &Note,
    from: &Note,
    guard: &GitNoteMergeGuard,
    budget: &mut MergeTxBudget,
) -> Result<(), MergeSqlError> {
    for (note, expected) in [(into, guard.into_version), (from, guard.from_version)] {
        if note.version != expected {
            return Err(MergeSqlError::Refusal(stale_note_snapshot_error(note.id)));
        }
        if note.namespace != namespace || note.kind != guard.kind || note.deleted_at.is_some() {
            return Err(refusal("note namespace, kind or live state"));
        }
        if note
            .properties
            .as_ref()
            .and_then(|p| p.get("_merge_history"))
            .is_some_and(|history| !history.is_array())
        {
            return Err(refusal(
                "malformed merge history cannot preserve provenance",
            ));
        }
    }
    if !canonical_project_exists(conn, namespace, guard.project_id)? {
        return Err(refusal("canonical project is no longer live"));
    }
    for note in [into, from] {
        let project = note
            .properties
            .as_ref()
            .and_then(|p| p.get("project_id"))
            .and_then(Value::as_str);
        if !membership(conn, namespace, note.id, project, guard.project_id, budget)? {
            return Err(refusal("project lineage or annotation membership"));
        }
    }
    if number(into) != Some(guard.number) {
        return Err(refusal("survivor number"));
    }
    if number(from) == Some(guard.number) {
        if guard.placeholder_url.is_some() {
            return Err(refusal("unexpected placeholder evidence"));
        }
        return Ok(());
    }
    let missing = from
        .properties
        .as_ref()
        .and_then(|p| p.get("number"))
        .is_none_or(Value::is_null);
    if !missing || from.name.as_deref() != Some(format!("[{}]", guard.kind).as_str()) {
        return Err(refusal("duplicate number or placeholder name"));
    }
    let Some(url) = guard
        .placeholder_url
        .as_deref()
        .filter(|url| valid_git_note_forge_url(url))
    else {
        return Err(refusal("numberless placeholder lacks exact forge URL"));
    };
    for note in [into, from] {
        if note
            .properties
            .as_ref()
            .and_then(|p| p.get("url"))
            .and_then(Value::as_str)
            != Some(url)
        {
            return Err(refusal("stored forge URL"));
        }
    }
    // Re-evaluate uniqueness at execution time; a new contradictory numbered
    // record after preview must stop adoption. Bound both rows and materialized bytes.
    let mut stmt = conn.prepare(crate::sql!("git_note_dedup_url_candidates_select"))?;
    let mut rows = stmt.query(rusqlite::params![namespace, url])?;
    let mut count = 0;
    let mut found = false;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > MAX_URL_CANDIDATES {
            return Err(refusal("URL evidence census exceeds bound"));
        }
        budget.charge(1, 256, "checking git URL uniqueness")?;
        let kind: String = row.get(1)?;
        let number_type: Option<String> = row.get(2)?;
        if number_type.as_deref() != Some("integer") {
            continue;
        }
        let current: i64 = row.get(3)?;
        if current <= 0 {
            continue;
        }
        let id: String = row.get(0)?;
        let id = Uuid::parse_str(&id).map_err(|_| refusal("invalid URL candidate ID"))?;
        let project: Option<String> = row.get(4)?;
        if !membership(
            conn,
            namespace,
            id,
            project.as_deref(),
            guard.project_id,
            budget,
        )? {
            continue;
        }
        if kind != guard.kind || current != guard.number {
            return Err(refusal("URL identifies more than one numbered group"));
        }
        found = true;
    }
    if !found {
        return Err(refusal("URL has no numbered group"));
    }
    Ok(())
}

pub(super) fn normalize_project(
    properties: &mut Option<Value>,
    guard: &GitNoteMergeGuard,
) -> Result<(), MergeSqlError> {
    let properties = properties
        .as_mut()
        .and_then(Value::as_object_mut)
        .ok_or_else(|| refusal("survivor properties are not an object"))?;
    properties.insert(
        "project_id".into(),
        Value::String(guard.project_id.to_string()),
    );
    Ok(())
}

pub(super) fn annotate_history(
    entry: &mut Value,
    guard: &GitNoteMergeGuard,
    into: &Note,
    from: &Note,
) {
    entry["git_note_repair"] = serde_json::json!({
        "canonical_project_id": guard.project_id,
        "kind": guard.kind,
        "number": guard.number,
        "into_version": into.version,
        "from_version": from.version,
        "into_project_id": into.properties.as_ref().and_then(|p| p.get("project_id")),
        "from_project_id": from.properties.as_ref().and_then(|p| p.get("project_id")),
        "placeholder_url": guard.placeholder_url,
    });
}

#[cfg(test)]
mod tests;
