use super::*;

/// Look up an existing `commit` note by its `properties.sha`. New ingests use
/// a note key to arbitrate concurrent creates, but older notes had no key.
/// Detect legacy duplicates rather than selecting an arbitrary holder.
pub(super) async fn find_commit_by_sha(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    sha: &str,
) -> Result<Option<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(anyhow::Error::new)?;
    let rows = r
        .query_all(SqlStatement {
            sql: sql!("commits_by_sha_select").into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(sha.to_string()),
            ],
            label: Some("git_ingest_find_commit_by_sha".into()),
        })
        .await
        .map_err(anyhow::Error::new)?;
    if rows.len() > 1 {
        bail!("multiple live commit notes hold SHA {sha}; reconcile legacy duplicates before advancing an ingest checkpoint");
    }
    rows.first()
        .map(|row| {
            row.uuid("id")
                .ok()
                .ok_or_else(|| anyhow!("stored commit note has an invalid ID for SHA {sha}"))
        })
        .transpose()
}

pub(super) fn is_note_key_conflict(error: &RuntimeError) -> bool {
    matches!(error.refusal_source(), RuntimeError::Khive(error)
        if error.details().and_then(|details| details.get("reason")) == Some("key_conflict"))
}

pub(super) async fn missing_commit_annotation_specs(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    commit_id: Uuid,
    annotates: &BTreeSet<String>,
) -> Result<Vec<LinkSpec>> {
    let existing_targets = commit_annotation_targets(runtime, token, commit_id).await?;
    Ok(annotates
        .iter()
        .map(|target| Uuid::parse_str(target).expect("annotation target is a UUID"))
        .filter(|target| !existing_targets.contains(target))
        .map(|target_id| LinkSpec {
            namespace: None,
            source_id: commit_id,
            target_id,
            relation: EdgeRelation::Annotates,
            weight: 1.0,
            metadata: None,
            resurrect: false,
        })
        .collect())
}

/// Include tombstones: a deleted annotation is a deliberate curation choice,
/// not a missing link for replay to recreate.
pub(super) async fn commit_annotation_targets(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    commit_id: Uuid,
) -> Result<BTreeSet<Uuid>> {
    let sql = runtime.sql();
    let mut reader = sql.reader().await.map_err(anyhow::Error::new)?;
    let rows = reader
        .query_all(SqlStatement {
            sql: sql!("commit_annotation_targets_select").into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(commit_id.to_string()),
            ],
            label: Some("git_ingest_commit_annotation_targets".into()),
        })
        .await
        .map_err(anyhow::Error::new)?;
    rows.iter()
        .map(|row| {
            row.uuid("id")
                .ok()
                .ok_or_else(|| anyhow!("stored commit annotation has an invalid target ID"))
        })
        .collect()
}

/// Look up an existing `issue`/`pull_request` note by its `properties.number`,
/// scoped by kind + namespace + `project_id` (GitHub numbers are
/// repository-scoped — see crates/khive-pack-git/docs/api/ingest.md).
pub(super) async fn find_by_number(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    kind: &str,
    project_id: Uuid,
    number: u64,
) -> Result<Option<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(anyhow::Error::new)?;
    let row = r
        .query_row(SqlStatement {
            sql: sql!("notes_by_number_select").into(),
            params: vec![
                SqlValue::Text(kind.to_string()),
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Integer(number as i64),
                SqlValue::Text(project_id.to_string()),
            ],
            label: Some("git_ingest_find_by_number".into()),
        })
        .await
        .map_err(anyhow::Error::new)?;
    Ok(row.and_then(|r| r.uuid("id").ok()))
}

/// Find an existing `document` entity by exact `properties.source_uri`, then
/// by a source-URI suffix, then by basename only when `source_uri` is absent
/// (ADR-086 Amendment 1); `None` when no match. The ingester never creates
/// documents on the caller's behalf. See
/// crates/khive-pack-git/docs/api/ingest.md#find_document_for_path for the
/// single-query ordering rationale.
pub(super) async fn find_document_for_path(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    path: &str,
) -> Result<Option<Uuid>> {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(path);
    let sql = runtime.sql();
    let namespace = token.namespace().as_str().to_string();
    let like_pattern = format!("%/{}", khive_types::escape_like_literal(path));

    let mut r = sql.reader().await.map_err(anyhow::Error::new)?;
    let row = r
        .query_row(
            EntityRows::DocumentsByPath {
                path,
                file_name,
                like_pattern: &like_pattern,
            }
            .statement(&namespace),
        )
        .await
        .map_err(anyhow::Error::new)?;
    Ok(row.and_then(|r| r.uuid("id").ok()))
}

/// Load the live code-map module index for the exact repository snapshot
/// being digested, keyed by `(source_revision, source_path)`. See
/// crates/khive-pack-git/docs/api/ingest.md#changed-paths-and-code-module-annotations
/// for the ambiguity contract and the best-effort degradation rule.
pub(super) async fn load_code_modules_by_snapshot_path(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    source_revision: &str,
) -> Result<HashMap<String, Option<Uuid>>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(anyhow::Error::new)?;
    let rows = r
        .query_all(
            EntityRows::CodeModulesBySnapshotPath(source_revision)
                .statement(token.namespace().as_str()),
        )
        .await
        .map_err(anyhow::Error::new)?;

    let mut modules: HashMap<String, Option<Uuid>> = HashMap::new();
    for row in rows {
        let Some(SqlValue::Text(path)) = row.get("source_path") else {
            continue;
        };
        // A row whose id does not parse is still a live row for its
        // `(source_revision, source_path)` key: it occupies the slot (so any
        // second row for the same key marks the pair ambiguous) but can
        // never bind as an annotation target itself.
        let id = row.uuid("id").ok();
        modules
            .entry(path.clone())
            .and_modify(|candidate| *candidate = None)
            .or_insert(id);
    }
    Ok(modules)
}

/// A single SHA cannot describe a visited prefix of a branching DAG. Retain
/// the immutable walk and a position inside its deterministic topological order.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CommitCheckpoint {
    pub(super) version: u8,
    pub(super) namespace: String,
    pub(super) base_cursor: Option<String>,
    pub(super) snapshot_head: String,
    pub(super) last_completed_sha: String,
}

pub(super) const COMMIT_CHECKPOINT_MAX_BYTES: usize = 8 * 1024;

pub(super) fn is_commit_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Read both rows at one SQL snapshot. A present invalid continuation cannot
/// safely fall back to a SHA range: that would lose the already visited DAG prefix.
pub(super) async fn read_commit_checkpoint(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project_id: Uuid,
) -> Result<(Option<String>, Option<CommitCheckpoint>)> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(anyhow::Error::new)?;
    let row = r
        .query_row(SqlStatement {
            sql: sql!("commit_checkpoint_select").into(),
            params: vec![SqlValue::Text(project_id.to_string())],
            label: Some("git_ingest_read_commit_checkpoint".into()),
        })
        .await
        .map_err(anyhow::Error::new)?;
    let value = |name| {
        row.as_ref().and_then(|r| match r.get(name) {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
    };
    let cursor = value("cursor");
    let Some(raw) = value("progress") else {
        return Ok((cursor, None));
    };
    if raw.len() > COMMIT_CHECKPOINT_MAX_BYTES {
        return Err(anyhow!("commits checkpoint exceeds the size limit; reset both commit cursor rows to replay history"));
    }
    let checkpoint: CommitCheckpoint = serde_json::from_str(&raw)
        .context("invalid commits checkpoint; reset both commit cursor rows to replay history")?;
    if checkpoint.version != 1
        || checkpoint.namespace != token.namespace().as_str()
        || !is_commit_oid(&checkpoint.snapshot_head)
        || !is_commit_oid(&checkpoint.last_completed_sha)
        || checkpoint
            .base_cursor
            .as_deref()
            .is_some_and(|sha| !is_commit_oid(sha))
        || cursor.as_deref() != Some(checkpoint.last_completed_sha.as_str())
    {
        return Err(anyhow!(
            "inconsistent commits checkpoint; reset both commit cursor rows to replay history"
        ));
    }
    Ok((cursor, Some(checkpoint)))
}

/// Atomically publish the compatibility SHA and its frozen-walk continuation.
pub(super) async fn write_commit_checkpoint(
    runtime: &KhiveRuntime,
    project_id: Uuid,
    checkpoint: &CommitCheckpoint,
) -> Result<()> {
    let progress = serde_json::to_string(checkpoint)?;
    if progress.len() > COMMIT_CHECKPOINT_MAX_BYTES {
        return Err(anyhow!("commits checkpoint exceeds the size limit"));
    }
    let sql = runtime.sql();
    let mut w = sql.writer().await.map_err(anyhow::Error::new)?;
    w.execute(SqlStatement {
        sql: sql!("commit_checkpoint_upsert").into(),
        params: vec![
            SqlValue::Text(project_id.to_string()),
            SqlValue::Text(progress),
            SqlValue::Text(checkpoint.last_completed_sha.clone()),
            SqlValue::Integer(khive_storage::now_micros()),
        ],
        label: Some("git_ingest_write_commit_checkpoint".into()),
    })
    .await
    .map_err(anyhow::Error::new)?;
    Ok(())
}

/// The timestamp remains the public cursor. Exact boundary acknowledgments let
/// an inclusive search resume even with max_items=1, without dropping unseen
/// ties. UUIDs also restore PR linking maps without another natural-key lookup.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PageCheckpoint {
    pub(super) version: u8,
    pub(super) namespace: String,
    pub(super) floor: Option<String>,
    pub(super) at_floor: BTreeMap<u64, Uuid>,
    pub(super) undated: BTreeMap<u64, Uuid>,
}

impl PageCheckpoint {
    pub(super) fn new(namespace: &str, floor: Option<String>) -> Self {
        Self {
            version: 1,
            namespace: namespace.into(),
            floor,
            at_floor: BTreeMap::new(),
            undated: BTreeMap::new(),
        }
    }

    pub(super) fn acknowledged(&self, number: u64, updated: Option<&str>) -> Option<Uuid> {
        match updated {
            None => self.undated.get(&number).copied(),
            Some(u) if self.floor.as_deref() == Some(u) => self.at_floor.get(&number).copied(),
            Some(_) => None,
        }
    }

    /// Only called before a stall. Refuse an unbounded acknowledgment set;
    /// dropping an entry would otherwise make a tiny-budget pass cycle forever.
    pub(super) fn acknowledge(&mut self, number: u64, updated: Option<&str>, id: Uuid) -> bool {
        let entries = match updated {
            None => &mut self.undated,
            Some(u) => {
                if self.floor.as_deref().is_none_or(|floor| u > floor) {
                    self.floor = Some(u.to_owned());
                    self.at_floor.clear();
                }
                if self.floor.as_deref() != Some(u) {
                    return true;
                }
                &mut self.at_floor
            }
        };
        if entries.len() >= PAGE_LIMIT && !entries.contains_key(&number) {
            return false;
        }
        entries.insert(number, id);
        true
    }
}

pub(super) async fn read_page_checkpoint(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project_id: Uuid,
    kind: &str,
    warnings: &mut Vec<String>,
) -> Result<PageCheckpoint> {
    // One read snapshot: a concurrent atomic checkpoint cannot tear this pair.
    let row = runtime
        .sql()
        .reader()
        .await?
        .query_row(SqlStatement {
            sql: sql!("page_checkpoint_select").into(),
            params: vec![
                SqlValue::Text(project_id.to_string()),
                SqlValue::Text(kind.into()),
                SqlValue::Text(format!("{kind}_checkpoint")),
            ],
            label: Some("git_ingest_read_page_checkpoint".into()),
        })
        .await?;
    let floor = match row.as_ref().and_then(|r| r.get("floor")) {
        Some(SqlValue::Text(raw)) => match chrono::DateTime::parse_from_rfc3339(raw) {
            Ok(dt) => Some(khive_runtime::presentation::format_utc_rfc3339_seconds(&dt)),
            Err(_) => {
                warnings.push(format!(
                    "{kind}: invalid stored timestamp cursor; restarting the window"
                ));
                None
            }
        },
        _ => None,
    };
    let namespace = token.namespace().as_str();
    if let Some(SqlValue::Text(raw)) = row.as_ref().and_then(|r| r.get("progress")) {
        // Two maps of at most PAGE_LIMIT UUIDs fit comfortably below this cap.
        let parsed = (raw.len() <= 256 * 1024)
            .then(|| serde_json::from_str::<PageCheckpoint>(raw).ok())
            .flatten();
        if let Some(progress) = parsed.filter(|p| {
            p.version == 1
                && p.namespace == namespace
                && p.floor == floor
                && p.at_floor.len() <= PAGE_LIMIT
                && p.undated.len() <= PAGE_LIMIT
                && (p.floor.is_some() || p.at_floor.is_empty())
        }) {
            return Ok(progress);
        }
        warnings.push(format!(
            "{kind}: incompatible checkpoint metadata; resuming from the timestamp cursor"
        ));
    }
    Ok(PageCheckpoint::new(namespace, floor))
}

pub(super) async fn write_page_checkpoint(
    runtime: &KhiveRuntime,
    project_id: Uuid,
    kind: &str,
    checkpoint: &PageCheckpoint,
) -> Result<()> {
    // One statement commits the timestamp and exact acknowledgments together.
    // An undated-only window has no main timestamp row yet.
    let mut params = vec![
        SqlValue::Text(project_id.to_string()),
        SqlValue::Text(format!("{kind}_checkpoint")),
        SqlValue::Text(serde_json::to_string(checkpoint)?),
        SqlValue::Integer(khive_storage::now_micros()),
    ];
    let statement_sql = if let Some(floor) = &checkpoint.floor {
        params.extend([SqlValue::Text(kind.into()), SqlValue::Text(floor.clone())]);
        sql!("page_checkpoint_with_floor_upsert")
    } else {
        sql!("page_checkpoint_without_floor_upsert")
    };
    let statement = SqlStatement {
        sql: statement_sql.into(),
        params,
        label: Some("git_ingest_write_page_checkpoint".into()),
    };
    runtime.sql().writer().await?.execute(statement).await?;
    Ok(())
}

// ── commits ─────────────────────────────────────────────────────────────────

pub(super) const RECORD_SEP: char = '\u{1e}';
pub(super) const TOUCHED_HEADER_PREFIX: &[u8] = b"/\x1e";

pub(super) struct RawCommit {
    pub(super) sha: String,
    pub(super) short_sha: String,
    pub(super) author: String,
    pub(super) author_email: String,
    pub(super) committed_at: String,
    pub(super) parents: Vec<String>,
    pub(super) subject: String,
    pub(super) body: String,
}

/// Which `git log` pass a classified failure came from (issue #765). See
/// crates/khive-pack-git/docs/api/ingest.md#issue-765-commit-snapshot-recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitLogPhase {
    Metadata,
    TouchedFiles,
}

/// A non-zero-exit history command failure, carrying its phase and raw stderr for
/// classification by `is_missing_promisor_object`.
#[derive(Debug)]
pub(crate) struct GitLogError {
    pub(super) phase: GitLogPhase,
    pub(super) stderr: String,
}

impl std::fmt::Display for GitLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cmd = match self.phase {
            GitLogPhase::Metadata => "git log/cat-file",
            GitLogPhase::TouchedFiles => "git log --name-only",
        };
        write!(f, "{cmd} failed: {}", self.stderr)
    }
}

impl std::error::Error for GitLogError {}

impl GitLogError {
    /// `true` for exactly the class of failure issue #765 authorizes
    /// self-healing for: a missing-object diagnostic that names a promisor
    /// remote. Deliberately narrow — see
    /// crates/khive-pack-git/docs/api/ingest.md#issue-765-commit-snapshot-recovery.
    pub(crate) fn is_missing_promisor_object(&self) -> bool {
        let lower = self.stderr.to_ascii_lowercase();
        lower.contains("promisor")
            && (lower.contains("not in the object database") || lower.contains("missing object"))
    }
}

/// Walk local history using `git log` only for fixed-format metadata, then
/// length-framed raw objects from `git cat-file --batch` for contributor text.
/// See crates/khive-pack-git/docs/api/ingest.md#issue-765-commit-snapshot-recovery.
pub(super) fn walk_commits(
    repo: &Path,
    since_sha: Option<&str>,
    snapshot_head: &str,
) -> Result<Vec<RawCommit>> {
    // These four fields are Git-generated IDs/date only. Contributor-controlled
    // author and message bytes never enter this line-delimited stream.
    let format = "%H%x00%h%x00%cI%x00%P%x00";
    let mut args = vec![
        "log".to_string(),
        "--reverse".to_string(),
        "--topo-order".to_string(),
        format!("--pretty=format:{format}"),
    ];
    args.push(match since_sha {
        Some(sha) => format!("{sha}..{snapshot_head}"),
        None => snapshot_head.to_string(),
    });
    args.push("--".into());
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || command.spawn())
        .and_then(|child| child.wait_with_output())
        .context("spawning git log")?;
    if !output.status.success() {
        return Err(anyhow::Error::new(GitLogError {
            phase: GitLogPhase::Metadata,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }));
    }
    let mut metadata = Vec::new();
    for record in output.stdout.split(|byte| *byte == b'\n') {
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&[u8]> = record.split(|byte| *byte == 0).collect();
        if fields.len() != 5 || !fields[4].is_empty() {
            bail!("git log returned a malformed commit metadata record");
        }
        let text = |field: &[u8]| String::from_utf8_lossy(field).into_owned();
        let sha = text(fields[0]);
        if !crate::object_id::is_40_hex(sha.as_bytes()) {
            bail!("git log returned a malformed commit id");
        }
        metadata.push((
            sha,
            text(fields[1]),
            text(fields[2]),
            text(fields[3])
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        ));
    }
    if metadata.is_empty() {
        return Ok(Vec::new());
    }

    // A regular file avoids a bidirectional pipe deadlock for long histories:
    // cat-file can read every SHA while its length-framed stdout is collected.
    let mut input = tempfile::tempfile().context("creating cat-file request list")?;
    for (sha, _, _, _) in &metadata {
        writeln!(input, "{sha}").context("writing cat-file request list")?;
    }
    input.seek(SeekFrom::Start(0))?;
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::from(input))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let objects = spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || command.spawn())
        .and_then(|child| child.wait_with_output())
        .context("spawning git cat-file --batch")?;
    if !objects.status.success() {
        return Err(anyhow::Error::new(GitLogError {
            phase: GitLogPhase::Metadata,
            stderr: String::from_utf8_lossy(&objects.stderr).into_owned(),
        }));
    }
    parse_batch_commits(&metadata, &objects.stdout)
}

pub(super) type CommitMetadata = (String, String, String, Vec<String>);

pub(super) fn decode_commit_message(headers: &[u8], raw: &[u8]) -> Result<String> {
    let Some(label) = headers
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"encoding "))
    else {
        return Ok(String::from_utf8_lossy(raw).into_owned());
    };
    // WHATWG labels map ISO-8859-1 to Windows-1252; Git's declared Latin-1 is literal.
    if label.eq_ignore_ascii_case(b"iso-8859-1")
        || label.eq_ignore_ascii_case(b"iso8859-1")
        || label.eq_ignore_ascii_case(b"latin1")
        || label.eq_ignore_ascii_case(b"latin-1")
    {
        return Ok(raw.iter().map(|byte| char::from(*byte)).collect());
    }
    let encoding = encoding_rs::Encoding::for_label_no_replacement(label)
        .context("git commit declares unsupported message encoding")?;
    let decoded = encoding
        .decode_without_bom_handling_and_without_replacement(raw)
        .context("git commit message is invalid for its declared encoding")?;
    Ok(decoded.into_owned())
}

pub(super) fn parse_batch_commits(
    metadata: &[CommitMetadata],
    bytes: &[u8],
) -> Result<Vec<RawCommit>> {
    let mut offset = 0;
    let mut commits = Vec::with_capacity(metadata.len());
    for (sha, short_sha, committed_at, parents) in metadata {
        let line_end = bytes[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| offset + index)
            .context("git cat-file returned a truncated object header")?;
        let header = std::str::from_utf8(&bytes[offset..line_end])
            .context("git cat-file returned a non-UTF-8 object header")?;
        let expected = format!("{sha} commit ");
        let length: usize = header
            .strip_prefix(&expected)
            .context("git cat-file returned an unexpected object")?
            .parse()
            .context("git cat-file returned an invalid object length")?;
        offset = line_end + 1;
        let end = offset
            .checked_add(length)
            .context("git cat-file object length overflow")?;
        let object = bytes
            .get(offset..end)
            .context("git cat-file returned a truncated object")?;
        if bytes.get(end) != Some(&b'\n') {
            bail!("git cat-file omitted the object terminator");
        }
        offset = end + 1;
        let separator = object
            .windows(2)
            .position(|part| part == b"\n\n")
            .context("git commit object has no message separator")?;
        let headers = &object[..separator];
        let author_line = headers
            .split(|byte| *byte == b'\n')
            .find_map(|line| line.strip_prefix(b"author "))
            .context("git commit object has no author")?;
        let author_text = String::from_utf8_lossy(author_line);
        let email_end = author_text
            .rfind('>')
            .context("git commit author has no email end")?;
        let email_start = author_text[..email_end]
            .rfind(" <")
            .context("git commit author has no email start")?;
        let author = author_text[..email_start].to_string();
        let author_email = author_text[email_start + 2..email_end].to_string();
        let message = decode_commit_message(headers, &object[separator + 2..])?;
        let mut lines = message.lines().skip_while(|line| line.trim().is_empty());
        let subject = lines
            .by_ref()
            .take_while(|line| !line.trim().is_empty())
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(" ");
        let body = lines
            .skip_while(|line| line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        commits.push(RawCommit {
            sha: sha.clone(),
            short_sha: short_sha.clone(),
            author,
            author_email,
            committed_at: committed_at.clone(),
            parents: parents.clone(),
            subject,
            body: body.trim_end_matches('\n').to_string(),
        });
    }
    if offset != bytes.len() {
        bail!("git cat-file returned trailing object data");
    }
    Ok(commits)
}
