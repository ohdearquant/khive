//! Read-only operator reports over the canonical main backend's attachment rows.

use std::collections::{BTreeSet, HashMap};
use std::fs::Metadata;
use std::path::PathBuf;

#[cfg(all(test, unix))]
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use khive_runtime::{BackendKind, KhiveConfig, KhiveRuntime, RuntimeConfig};
#[cfg(all(test, unix))]
use khive_storage::TopLevelMaintenance;
use khive_storage::{SqlAccess, SqlRow, SqlStatement, SqlValue};
use serde::Serialize;

const PAGE_SIZE: usize = 128;
const CANDIDATE_NOTICE: &str = "Best-effort interval report over operator-quiesced or frozen roster members only. A main row present for the whole walk is read exactly once. If inputs change despite that requirement, an insertion behind the cursor can be missed and a changed or deleted row retains the values read. Counters are observations, not a simultaneous total. Member probes are separate reads of a roster as of quiescence or freeze, and frozen copies may have different capture times. A record committed after the snapshot is not visible; after service resumes it may be published, may live outside the stated roster, or may return when an older backend copy is restored. A listed row is only a candidate for investigation, never a deletion manifest. Row age proves nothing.";

#[derive(Subcommand, Debug)]
pub enum BlobCommand {
    /// Report main attachment rows whose record was not found on the stated roster.
    OwnerlessRows(OwnerlessRowsArgs),
}

#[derive(clap::Parser, Debug)]
pub struct OwnerlessRowsArgs {
    /// Main database path (defaults to `~/.khive/khive.db`).
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<String>,

    /// Explicit khive config path (otherwise use normal discovery).
    #[arg(long, env = "KHIVE_CONFIG")]
    pub config: Option<PathBuf>,

    /// Add a previously used database to the read-only probe roster.
    #[arg(long = "with-db", value_name = "PATH")]
    pub with_db: Vec<PathBuf>,
}

#[derive(Serialize)]
struct MemberHeader {
    names: Vec<String>,
    canonical_path: PathBuf,
    device: u64,
    inode: u64,
    schema_version: u32,
}

struct Member {
    header: MemberHeader,
    runtime: KhiveRuntime,
}

#[derive(Serialize)]
struct Absence {
    member: Vec<String>,
    status: &'static str,
    probed_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct OwnerlessRow {
    record_uuid: String,
    substrate: String,
    role: String,
    content_ref: String,
    media_type: Option<String>,
    size_bytes: Option<i64>,
    created_at: i64,
    created_at_source: &'static str,
    read_at: DateTime<Utc>,
    member_probes: Vec<Absence>,
}

#[derive(Default, Serialize)]
struct Counters {
    members: usize,
    scanned: u64,
    owned: u64,
    owned_deleted: u64,
    owned_multiple: u64,
    ownerless: u64,
}

#[derive(Serialize)]
struct OwnerlessReport {
    notice: &'static str,
    roster_completeness_verified: bool,
    members: Vec<MemberHeader>,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    counters: Counters,
    rows: Vec<OwnerlessRow>,
}

struct AttachmentRow {
    record_uuid: String,
    substrate: String,
    role: String,
    content_ref: String,
    media_type: Option<String>,
    size_bytes: Option<i64>,
    created_at: i64,
    read_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Default)]
struct Presence {
    live: bool,
}

pub async fn run_blob(command: BlobCommand) -> Result<()> {
    match command {
        BlobCommand::OwnerlessRows(args) => {
            let report = ownerless_rows(args).await?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
    }
}

async fn ownerless_rows(args: OwnerlessRowsArgs) -> Result<OwnerlessReport> {
    ownerless_rows_with_probe_hook(args, |_, _| Ok(())).await
}

async fn ownerless_rows_with_probe_hook<F>(
    args: OwnerlessRowsArgs,
    mut before_probe: F,
) -> Result<OwnerlessReport>
where
    F: FnMut(usize, usize) -> Result<()>,
{
    let paths = resolve_roster_paths(&args)?;
    let members = open_roster(paths)?;
    let started_at = Utc::now();
    let mut counters = Counters {
        members: members.len(),
        ..Counters::default()
    };
    let mut ownerless = Vec::new();
    let main_sql = members[0].runtime.sql();
    let mut after: Option<(String, String)> = None;
    let mut page_index = 0;

    loop {
        let page = read_page(&main_sql, after.as_ref()).await?;
        if page.is_empty() {
            break;
        }
        page_index += 1;
        let page_len = page.len();
        let ids: Vec<String> = page
            .iter()
            .map(|row| row.record_uuid.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut probes = Vec::with_capacity(members.len());
        for (member_index, member) in members.iter().enumerate() {
            let found = async {
                before_probe(page_index, member_index)?;
                probe_member(member, &ids).await
            }
            .await
            .with_context(|| {
                format!(
                    "probe roster member {:?} at {}",
                    member.header.names,
                    member.header.canonical_path.display()
                )
            })?;
            probes.push((found, Utc::now()));
        }
        for row in page {
            counters.scanned += 1;
            let mut holders = 0;
            let mut live = false;
            let mut absence = Vec::new();
            for (member, (found, probed_at)) in members.iter().zip(&probes) {
                if let Some(presence) = found.get(&row.record_uuid) {
                    holders += 1;
                    live |= presence.live;
                } else {
                    absence.push(Absence {
                        member: member.header.names.clone(),
                        status: "absent",
                        probed_at: *probed_at,
                    });
                }
            }
            if holders == 0 {
                counters.ownerless += 1;
                ownerless.push(OwnerlessRow {
                    record_uuid: row.record_uuid.clone(),
                    substrate: row.substrate,
                    role: row.role.clone(),
                    content_ref: row.content_ref,
                    media_type: row.media_type,
                    size_bytes: row.size_bytes,
                    created_at: row.created_at,
                    created_at_source: "producer_supplied",
                    read_at: row.read_at,
                    member_probes: absence,
                });
            } else {
                counters.owned += 1;
                counters.owned_deleted += u64::from(!live);
                counters.owned_multiple += u64::from(holders > 1);
            }
            after = Some((row.record_uuid, row.role));
        }
        if page_len < PAGE_SIZE {
            break;
        }
    }

    Ok(OwnerlessReport {
        notice: CANDIDATE_NOTICE,
        roster_completeness_verified: false,
        members: members.into_iter().map(|member| member.header).collect(),
        started_at,
        ended_at: Utc::now(),
        counters,
        rows: ownerless,
    })
}

fn resolve_roster_paths(args: &OwnerlessRowsArgs) -> Result<Vec<(String, PathBuf)>> {
    if args.db.as_deref() == Some(":memory:") {
        bail!("--db :memory: is not a file-backed main backend for ownerless-rows");
    }
    let discovery_anchor = khive_mcp::serve::config_discovery_db_anchor(args.db.as_deref());
    let loaded = KhiveConfig::load_with_home_fallback_and_source(
        args.config.as_deref(),
        discovery_anchor.as_deref(),
    )
    .context("load ownerless-rows config")?;
    let config_source = loaded.as_ref().map(|(_, source)| source.clone());
    let config = loaded.map(|(config, _)| config).unwrap_or_default();
    khive_mcp::serve::reject_conflicting_db_override_with_source(
        args.db.as_deref(),
        &config.backends,
        config_source.as_deref(),
    )?;

    let mut paths = Vec::new();
    if config.backends.is_empty() {
        let path = khive_runtime::resolve_db_anchor(args.db.as_deref())
            .ok_or_else(|| anyhow!("main backend is in memory; ownerless-rows requires a file"))?;
        paths.push(("main".to_string(), path));
    } else {
        let main = config
            .backends
            .iter()
            .find(|backend| backend.name == "main")
            .ok_or_else(|| anyhow!("configured roster has no main backend"))?;
        paths.push(("main".to_string(), backend_path(main)?));
        for backend in config
            .backends
            .iter()
            .filter(|backend| backend.name != "main")
        {
            paths.push((backend.name.clone(), backend_path(backend)?));
        }
    }
    paths.extend(args.with_db.iter().map(|path| {
        (
            format!("--with-db {}", path.display()),
            khive_runtime::expand_tilde(path),
        )
    }));
    Ok(paths)
}

fn backend_path(backend: &khive_runtime::BackendConfig) -> Result<PathBuf> {
    if backend.kind == BackendKind::Memory {
        bail!(
            "roster member {:?} is in memory; ownerless-rows requires a file",
            backend.name
        );
    }
    let path = backend
        .path
        .as_deref()
        .ok_or_else(|| anyhow!("roster member {:?} has no database path", backend.name))?;
    Ok(khive_runtime::expand_tilde(path))
}

fn open_roster(paths: Vec<(String, PathBuf)>) -> Result<Vec<Member>> {
    let mut members: Vec<Member> = Vec::new();
    let mut seen: HashMap<(u64, u64), usize> = HashMap::new();
    for (name, path) in paths {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("roster member {name:?}: resolve {}", path.display()))?;
        let metadata = canonical
            .metadata()
            .with_context(|| format!("roster member {name:?}: stat {}", canonical.display()))?;
        if !metadata.is_file() {
            bail!(
                "roster member {name:?}: {} is not a file",
                canonical.display()
            );
        }
        let identity = file_identity(&metadata)?;
        if let Some(&index) = seen.get(&identity) {
            members[index].header.names.push(name);
            continue;
        }
        let runtime = KhiveRuntime::new_readonly(RuntimeConfig {
            db_path: Some(canonical.clone()),
            packs: Vec::new(),
            ..RuntimeConfig::no_embeddings()
        })
        .with_context(|| {
            format!(
                "roster member {name:?}: open {} read-only at current schema",
                canonical.display()
            )
        })?;
        let after_open = canonical
            .metadata()
            .with_context(|| format!("roster member {name:?}: restat {}", canonical.display()))?;
        if file_identity(&after_open)? != identity {
            bail!("roster member {name:?}: database file changed while opening");
        }
        let schema_version = khive_db::inspect_schema_is_current(&canonical)
            .with_context(|| format!("roster member {name:?}: inspect applied schema"))?;
        seen.insert(identity, members.len());
        members.push(Member {
            header: MemberHeader {
                names: vec![name],
                canonical_path: canonical,
                device: identity.0,
                inode: identity.1,
                schema_version,
            },
            runtime,
        });
    }
    Ok(members)
}

#[cfg(all(test, unix))]
fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(unix)]
fn file_identity(metadata: &Metadata) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_metadata: &Metadata) -> Result<(u64, u64)> {
    bail!("ownerless-rows requires device/inode file identity support")
}

async fn read_page(
    sql: &std::sync::Arc<dyn SqlAccess>,
    after: Option<&(String, String)>,
) -> Result<Vec<AttachmentRow>> {
    let mut reader = sql.reader().await.context("read main attachment page")?;
    let (where_clause, params) = match after {
        Some((record_uuid, role)) => (
            "WHERE (record_uuid, role) > (?1, ?2)",
            vec![
                SqlValue::Text(record_uuid.clone()),
                SqlValue::Text(role.clone()),
            ],
        ),
        None => ("", Vec::new()),
    };
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at \
                 FROM attachments {where_clause} ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY LIMIT {PAGE_SIZE}"
            ),
            params,
            label: Some("ownerless-attachment-page".into()),
        })
        .await
        .context("read main attachment page")?;
    drop(reader);
    let read_at = Utc::now();
    rows.into_iter()
        .map(|row| {
            Ok(AttachmentRow {
                record_uuid: text_column(&row, "record_uuid")?,
                substrate: text_column(&row, "substrate")?,
                role: text_column(&row, "role")?,
                content_ref: text_column(&row, "content_ref")?,
                media_type: optional_text_column(&row, "media_type")?,
                size_bytes: optional_integer_column(&row, "size_bytes")?,
                created_at: integer_column(&row, "created_at")?,
                read_at,
            })
        })
        .collect()
}

async fn probe_member(member: &Member, ids: &[String]) -> Result<HashMap<String, Presence>> {
    let placeholders = (1..=ids.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = member.runtime.sql();
    let mut reader = sql.reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT id, deleted_at FROM entities WHERE id IN ({placeholders}) \
                 UNION ALL SELECT id, deleted_at FROM notes WHERE id IN ({placeholders})"
            ),
            params: ids.iter().cloned().map(SqlValue::Text).collect(),
            label: Some("ownerless-record-probe".into()),
        })
        .await?;
    drop(reader);
    let mut found = HashMap::<String, Presence>::new();
    for row in rows {
        let id = text_column(&row, "id")?;
        let live = optional_integer_column(&row, "deleted_at")?.is_none();
        found.entry(id).or_default().live |= live;
    }
    Ok(found)
}

fn text_column(row: &SqlRow, column: &str) -> Result<String> {
    match row.get(column) {
        Some(SqlValue::Text(value)) => Ok(value.clone()),
        other => Err(anyhow!("{column}: expected text, got {other:?}")),
    }
}

fn integer_column(row: &SqlRow, column: &str) -> Result<i64> {
    match row.get(column) {
        Some(SqlValue::Integer(value)) => Ok(*value),
        other => Err(anyhow!("{column}: expected integer, got {other:?}")),
    }
}

fn optional_text_column(row: &SqlRow, column: &str) -> Result<Option<String>> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
        other => Err(anyhow!("{column}: expected nullable text, got {other:?}")),
    }
}

fn optional_integer_column(row: &SqlRow, column: &str) -> Result<Option<i64>> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Integer(value)) => Ok(Some(*value)),
        other => Err(anyhow!(
            "{column}: expected nullable integer, got {other:?}"
        )),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;

    use super::*;
    use khive_db::StorageBackend;

    fn fixture(path: &Path) -> StorageBackend {
        let backend = StorageBackend::sqlite(path).expect("create fixture database");
        backend
            .prepare_core_schema()
            .expect("migrate fixture database");
        backend
    }

    async fn insert_record(backend: &StorageBackend, id: &str, deleted_at: Option<i64>) {
        insert_record_in_namespace(backend, id, "local", deleted_at).await;
    }

    async fn insert_record_in_namespace(
        backend: &StorageBackend,
        id: &str,
        namespace: &str,
        deleted_at: Option<i64>,
    ) {
        let sql = backend.sql();
        let mut writer = sql.writer().await.expect("fixture writer");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO entities (id, namespace, kind, name, created_at, updated_at, deleted_at) \
                      VALUES (?1, ?2, 'concept', 'fixture', 1, 1, ?3)"
                    .into(),
                params: vec![
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(namespace.to_string()),
                    deleted_at.map_or(SqlValue::Null, SqlValue::Integer),
                ],
                label: None,
            })
            .await
            .expect("insert fixture record");
    }

    async fn insert_note(backend: &StorageBackend, id: &str) {
        let sql = backend.sql();
        let mut writer = sql.writer().await.expect("fixture writer");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO notes (id, namespace, kind, created_at, updated_at) \
                      VALUES (?1, 'local', 'observation', 1, 1)"
                    .into(),
                params: vec![SqlValue::Text(id.to_string())],
                label: None,
            })
            .await
            .expect("insert fixture note");
    }

    async fn insert_attachment(backend: &StorageBackend, id: &str) {
        insert_attachment_role(backend, id, "body").await;
    }

    async fn insert_attachment_role(backend: &StorageBackend, id: &str, role: &str) {
        let sql = backend.sql();
        let mut writer = sql.writer().await.expect("fixture writer");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO attachments \
                      (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at) \
                      VALUES (?1, 'entity', ?2, ?3, 'text/plain', 4, 123)"
                    .into(),
                params: vec![
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(role.to_string()),
                    SqlValue::Text("0".repeat(64)),
                ],
                label: None,
            })
            .await
            .expect("insert fixture attachment");
    }

    #[tokio::test]
    async fn report_paginates_and_probes_every_roster_member_without_deleting() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let main = dir.path().join("main.db");
        let old_backend = dir.path().join("old-backend.db");
        let config = dir.path().join("roster.toml");
        std::fs::write(
            &config,
            format!(
                "[[backends]]\nname = 'main'\nkind = 'sqlite'\npath = {:?}\n\
                 [[backends]]\nname = 'unassigned'\nkind = 'sqlite'\npath = {:?}\n",
                main.display().to_string(),
                old_backend.display().to_string()
            ),
        )
        .expect("roster config");
        let main_backend = fixture(&main);
        let secondary = fixture(&old_backend);
        let ids = (1..=130)
            .map(|n| uuid::Uuid::from_u128(n).to_string())
            .collect::<Vec<_>>();
        for id in &ids {
            insert_attachment(&main_backend, id).await;
        }
        // The first role for id[127] ends page one; the second starts page two.
        insert_attachment_role(&main_backend, &ids[127], "metadata").await;
        insert_record_in_namespace(&secondary, &ids[0], "other-namespace", None).await;
        insert_record(&secondary, &ids[1], Some(456)).await;
        let sql = secondary.sql();
        let mut writer = sql.writer().await.expect("fixture writer");
        writer
            .execute(SqlStatement {
                sql: "UPDATE entities SET merged_into = ?1, version = version + 1 WHERE id = ?2"
                    .into(),
                params: vec![
                    SqlValue::Text(ids[0].clone()),
                    SqlValue::Text(ids[1].clone()),
                ],
                label: None,
            })
            .await
            .expect("mark merged tombstone");
        drop(writer);
        drop(sql);
        insert_record(&main_backend, &ids[2], None).await;
        insert_record(&secondary, &ids[2], None).await;
        insert_note(&secondary, &ids[3]).await;
        let sql = main_backend.sql();
        let mut writer = sql.writer().await.expect("fixture writer");
        writer
            .execute(SqlStatement {
                sql: "UPDATE attachments SET substrate = 'note' WHERE record_uuid = ?1".into(),
                params: vec![SqlValue::Text(ids[3].clone())],
                label: None,
            })
            .await
            .expect("mark note attachment");
        drop(writer);
        drop(sql);
        drop(main_backend);
        drop(secondary);
        khive_storage::test_support::freeze_snapshot_sidecars(&main);
        khive_storage::test_support::freeze_snapshot_sidecars(&old_backend);

        let report = ownerless_rows(OwnerlessRowsArgs {
            db: None,
            config: Some(config),
            with_db: vec![old_backend],
        })
        .await
        .expect("read-only report");

        assert_eq!(report.counters.members, 2);
        assert_eq!(report.counters.scanned, 131);
        assert_eq!(report.counters.owned, 4);
        assert_eq!(report.counters.owned_deleted, 1);
        assert_eq!(report.counters.owned_multiple, 1);
        assert_eq!(report.counters.ownerless, 127);
        assert_eq!(report.rows.len(), 127);
        assert!(report.rows.iter().any(|row| row.record_uuid == ids[129]));
        assert!(report
            .rows
            .iter()
            .any(|row| row.record_uuid == ids[127] && row.role == "metadata"));
        assert!(report.rows.iter().all(|row| row.member_probes.len() == 2));
        assert_eq!(report.members[1].names.len(), 2);
        assert!(!report.roster_completeness_verified);
        assert_eq!(
            report.members[0].schema_version,
            khive_db::migrations::latest_schema_version()
        );
        for phrase in [
            "exactly once",
            "behind the cursor",
            "changed or deleted row",
            "Counters are observations",
            "outside the stated roster",
            "older backend copy is restored",
            "never a deletion manifest",
            "committed after the snapshot is not visible",
        ] {
            assert!(report.notice.contains(phrase), "missing notice: {phrase}");
        }

        let backend = StorageBackend::sqlite_read_only(&main).expect("main stays readable");
        let sql = backend.sql();
        let mut reader = sql.reader().await.expect("read attachment count");
        let count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM attachments".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("count attachments");
        assert!(matches!(count, Some(SqlValue::Integer(131))));
    }

    #[tokio::test]
    async fn missing_roster_member_refuses_entire_report() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let main = dir.path().join("main.db");
        let config = dir.path().join("empty.toml");
        std::fs::write(&config, "").expect("empty config");
        let backend = fixture(&main);
        let id = uuid::Uuid::from_u128(1).to_string();
        insert_attachment(&backend, &id).await;
        let writer_join = backend
            .pool()
            .take_writer_task_join()
            .expect("fixture writer task started");
        drop(backend);
        tokio::time::timeout(std::time::Duration::from_secs(5), writer_join)
            .await
            .expect("fixture writer task drains")
            .expect("fixture writer task does not panic");
        assert!(!sidecar_path(&main, "-wal").exists());
        assert!(!sidecar_path(&main, "-shm").exists());

        let error = match ownerless_rows(OwnerlessRowsArgs {
            db: Some(main.display().to_string()),
            config: Some(config),
            with_db: vec![dir.path().join("missing.db")],
        })
        .await
        {
            Ok(_) => panic!("missing member must fail closed"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("missing.db"));
    }

    #[tokio::test]
    async fn stale_schema_and_memory_member_refuse_the_report() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let main = dir.path().join("main.db");
        let stale = dir.path().join("stale.db");
        let config = dir.path().join("roster.toml");
        let backend = fixture(&main);
        let id = uuid::Uuid::from_u128(1).to_string();
        insert_attachment(&backend, &id).await;
        drop(backend);
        drop(StorageBackend::sqlite(&stale).expect("unmigrated sqlite file"));
        khive_storage::test_support::freeze_snapshot_sidecars(&main);
        khive_storage::test_support::freeze_snapshot_sidecars(&stale);
        std::fs::write(&config, "").expect("empty config");
        let error = match ownerless_rows(OwnerlessRowsArgs {
            db: Some(main.display().to_string()),
            config: Some(config.clone()),
            with_db: vec![stale.clone()],
        })
        .await
        {
            Ok(_) => panic!("stale schema must fail closed"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("stale.db"));
        assert!(message.contains("behind the latest known migration"));

        std::fs::write(
            &config,
            format!(
                "[[backends]]\nname = 'main'\nkind = 'sqlite'\npath = {:?}\n\
                 [[backends]]\nname = 'memory-member'\nkind = 'memory'\n",
                main.display().to_string()
            ),
        )
        .expect("roster config");
        let error = match ownerless_rows(OwnerlessRowsArgs {
            db: None,
            config: Some(config.clone()),
            with_db: vec![],
        })
        .await
        {
            Ok(_) => panic!("memory member must fail closed"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("memory-member"));

        let error = match ownerless_rows(OwnerlessRowsArgs {
            db: Some(":memory:".to_string()),
            config: Some(config),
            with_db: vec![],
        })
        .await
        {
            Ok(_) => panic!("in-memory main override must fail closed"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("--db :memory:"));
    }

    #[tokio::test]
    async fn removed_configured_member_refuses_entire_report() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let main = dir.path().join("main.db");
        let departed = dir.path().join("departed.db");
        let config = dir.path().join("roster.toml");
        let backend = fixture(&main);
        drop(backend);
        khive_storage::test_support::freeze_snapshot_sidecars(&main);
        std::fs::write(
            &config,
            format!(
                "[[backends]]\nname = 'main'\nkind = 'sqlite'\npath = {:?}\n\
                 [[backends]]\nname = 'departed'\nkind = 'sqlite'\npath = {:?}\n",
                main.display().to_string(),
                departed.display().to_string()
            ),
        )
        .expect("roster config");
        let error = match ownerless_rows(OwnerlessRowsArgs {
            db: None,
            config: Some(config),
            with_db: vec![],
        })
        .await
        {
            Ok(_) => panic!("removed declaration target must fail closed"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("departed.db"));
    }

    #[tokio::test]
    async fn removing_a_declaration_changes_roster_but_with_db_restores_it() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let main = dir.path().join("main.db");
        let old = dir.path().join("old.db");
        let config = dir.path().join("roster.toml");
        let main_backend = fixture(&main);
        let old_backend = fixture(&old);
        let id = uuid::Uuid::from_u128(1).to_string();
        insert_attachment(&main_backend, &id).await;
        insert_record(&old_backend, &id, None).await;
        drop(main_backend);
        drop(old_backend);
        khive_storage::test_support::freeze_snapshot_sidecars(&main);
        khive_storage::test_support::freeze_snapshot_sidecars(&old);

        let configured = format!(
            "[[backends]]\nname = 'main'\nkind = 'sqlite'\npath = {:?}\n\
             [[backends]]\nname = 'old'\nkind = 'sqlite'\npath = {:?}\n",
            main.display().to_string(),
            old.display().to_string()
        );
        std::fs::write(&config, configured).expect("two-member roster");
        let args = |with_db: Vec<PathBuf>| OwnerlessRowsArgs {
            db: None,
            config: Some(config.clone()),
            with_db,
        };
        let first = ownerless_rows(args(vec![])).await.expect("declared owner");
        assert_eq!(first.counters.members, 2);
        assert_eq!(first.counters.owned, 1);
        assert_eq!(first.counters.ownerless, 0);

        std::fs::write(
            &config,
            format!(
                "[[backends]]\nname = 'main'\nkind = 'sqlite'\npath = {:?}\n",
                main.display().to_string()
            ),
        )
        .expect("main-only roster");
        let omitted = ownerless_rows(args(vec![]))
            .await
            .expect("undeclared old backend is not probed");
        assert_eq!(omitted.counters.members, 1);
        assert_eq!(omitted.counters.ownerless, 1);
        assert_eq!(omitted.rows[0].record_uuid, id);

        let restored = ownerless_rows(args(vec![old]))
            .await
            .expect("explicit old backend is probed");
        assert_eq!(restored.counters.members, 2);
        assert_eq!(restored.counters.owned, 1);
        assert_eq!(restored.counters.ownerless, 0);
    }

    #[tokio::test]
    async fn writable_walk_keyset_survives_delete_between_pages() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let main = dir.path().join("main.db");
        let backend = fixture(&main);
        let ids = (1..=130)
            .map(|number| uuid::Uuid::from_u128(number).to_string())
            .collect::<Vec<_>>();
        for id in &ids {
            insert_attachment(&backend, id).await;
        }
        let sql = backend.sql();
        let first = read_page(&sql, None).await.expect("first writable page");
        assert_eq!(first.len(), PAGE_SIZE);
        assert_eq!(first.last().expect("last key").record_uuid, ids[127]);
        let last = first.last().expect("last key");
        let after = (last.record_uuid.clone(), last.role.clone());

        let mut writer = sql.writer().await.expect("fixture writer");
        writer
            .execute(SqlStatement {
                sql: "DELETE FROM attachments WHERE record_uuid = ?1 AND role = 'body'".into(),
                params: vec![SqlValue::Text(ids[0].clone())],
                label: None,
            })
            .await
            .expect("delete a row in the already-read page");
        drop(writer);
        let second = read_page(&sql, Some(&after))
            .await
            .expect("keyset page after deletion");
        let mut reader = sql.reader().await.expect("offset control reader");
        let offset_control = reader
            .query_all(SqlStatement {
                sql: format!(
                    "SELECT record_uuid FROM attachments ORDER BY record_uuid COLLATE BINARY, \
                     role COLLATE BINARY LIMIT {PAGE_SIZE} OFFSET {PAGE_SIZE}"
                ),
                params: vec![],
                label: None,
            })
            .await
            .expect("offset control page");
        assert_eq!(offset_control.len(), 1, "offset loses one unread row");
        assert_eq!(
            second.len(),
            2,
            "offset paging would skip one remaining row"
        );
        assert_eq!(second[0].record_uuid, ids[128]);
        assert_eq!(second[1].record_uuid, ids[129]);
    }

    #[tokio::test]
    async fn second_page_probe_error_aborts_without_a_report() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let main = dir.path().join("main.db");
        let config = dir.path().join("empty.toml");
        std::fs::write(&config, "").expect("empty config");
        let backend = fixture(&main);
        for number in 1..=129 {
            insert_attachment(&backend, &uuid::Uuid::from_u128(number).to_string()).await;
        }
        drop(backend);
        khive_storage::test_support::freeze_snapshot_sidecars(&main);
        let args = || OwnerlessRowsArgs {
            db: Some(main.display().to_string()),
            config: Some(config.clone()),
            with_db: vec![],
        };
        let error = match ownerless_rows_with_probe_hook(args(), |page, member| {
            if page == 2 && member == 0 {
                bail!("injected page-two probe failure");
            }
            Ok(())
        })
        .await
        {
            Ok(_) => panic!("late probe failure must abort without a report"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("injected page-two probe failure"));
        assert!(message.contains("main"));
    }

    #[tokio::test]
    async fn live_wal_refuses_and_frozen_wal_only_row_is_reported() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let source = dir.path().join("serving.db");
        let frozen = dir.path().join("frozen.db");
        let immutable = dir.path().join("immutable.db");
        let config = dir.path().join("empty.toml");
        std::fs::write(&config, "").expect("empty config");
        let backend = fixture(&source);
        let sql = backend.sql();
        let mut writer = sql.writer().await.expect("fixture writer");
        writer
            .execute_script_top_level(TopLevelMaintenance::WalCheckpointTruncate)
            .await
            .expect("checkpoint migrated schema before writing the candidate row");
        drop(writer);
        drop(sql);
        let id = uuid::Uuid::from_u128(1).to_string();
        insert_attachment(&backend, &id).await;
        let source_wal = sidecar_path(&source, "-wal");
        let source_shm = sidecar_path(&source, "-shm");
        assert!(source_wal.exists() && source_shm.exists());

        let args = |path: &Path| OwnerlessRowsArgs {
            db: Some(path.display().to_string()),
            config: Some(config.clone()),
            with_db: vec![],
        };
        let error = match ownerless_rows(args(&source)).await {
            Ok(_) => panic!("live writable WAL shared memory must be refused"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("serving.db"));
        assert!(message.contains("writable WAL shared-memory sidecar"));

        let frozen_wal = sidecar_path(&frozen, "-wal");
        let frozen_shm = sidecar_path(&frozen, "-shm");
        let immutable_wal = sidecar_path(&immutable, "-wal");
        std::fs::copy(&source, &frozen).expect("copy database header before checkpoint");
        std::fs::copy(&source_wal, &frozen_wal).expect("copy committed WAL frames");
        std::fs::copy(&source_shm, &frozen_shm).expect("copy matching WAL index");
        std::fs::copy(&source, &immutable).expect("copy database without WAL frames");
        std::fs::copy(&source_wal, &immutable_wal).expect("copy WAL without shared-memory index");
        let paths = [&frozen, &frozen_wal, &frozen_shm];
        let original_permissions = paths.map(|path| {
            let permissions = std::fs::metadata(path)
                .expect("snapshot metadata")
                .permissions();
            let mut frozen_permissions = permissions.clone();
            frozen_permissions.set_readonly(true);
            std::fs::set_permissions(path, frozen_permissions).expect("freeze snapshot file");
            permissions
        });
        let before = paths.map(|path| std::fs::read(path).expect("read frozen file"));
        assert!(!before[1].is_empty(), "the only copy of the row is in WAL");

        let immutable_uri = format!("file:{}?mode=ro&immutable=1", immutable.display());
        let immutable_connection = rusqlite::Connection::open_with_flags(
            immutable_uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .expect("test-only immutable control open");
        let immutable_count: i64 = immutable_connection
            .query_row("SELECT COUNT(*) FROM attachments", [], |row| row.get(0))
            .expect("immutable control reads checkpointed schema only");
        assert_eq!(
            immutable_count, 0,
            "immutable SQLite silently misses WAL row"
        );
        drop(immutable_connection);

        let report = ownerless_rows(args(&frozen))
            .await
            .expect("frozen WAL frames must be visible through read-only open");
        assert_eq!(report.counters.scanned, 1);
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].record_uuid, id);
        for (path, (contents, permissions)) in paths
            .into_iter()
            .zip(before.into_iter().zip(original_permissions))
        {
            assert_eq!(std::fs::read(path).expect("read after report"), contents);
            std::fs::set_permissions(path, permissions).expect("restore fixture permissions");
        }
        drop(backend);
    }

    #[tokio::test]
    async fn in_flight_record_refuses_then_abort_and_frozen_copy_lists_candidate() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let source = dir.path().join("main.db");
        let frozen = dir.path().join("frozen.db");
        let config = dir.path().join("empty.toml");
        std::fs::write(&config, "").expect("empty config");
        let backend = fixture(&source);
        let id = uuid::Uuid::from_u128(1).to_string();
        insert_attachment(&backend, &id).await;

        let pending = rusqlite::Connection::open(&source).expect("outside writer");
        pending
            .execute_batch(&format!(
                "BEGIN IMMEDIATE; INSERT INTO entities \
                 (id, namespace, kind, name, created_at, updated_at) \
                 VALUES ('{id}', 'local', 'concept', 'pending', 1, 1);"
            ))
            .expect("hold record publication uncommitted");
        let args = |path: &Path| OwnerlessRowsArgs {
            db: Some(path.display().to_string()),
            config: Some(config.clone()),
            with_db: vec![],
        };
        let error = match ownerless_rows(args(&source)).await {
            Ok(_) => panic!("live writer must be refused while record is uncommitted"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("main.db"));
        assert!(message.contains("close every live writer"));
        assert!(message.contains("frozen snapshot"));
        pending
            .execute_batch("ROLLBACK")
            .expect("abort publication");
        drop(pending);

        let sql = backend.sql();
        let mut writer = sql.writer().await.expect("fixture writer");
        writer
            .execute_script_top_level(TopLevelMaintenance::WalCheckpointTruncate)
            .await
            .expect("checkpoint after aborted publication");
        drop(writer);
        drop(sql);
        let source_wal = sidecar_path(&source, "-wal");
        let source_shm = sidecar_path(&source, "-shm");
        assert!(source_wal.exists() && source_shm.exists());
        let frozen_wal = sidecar_path(&frozen, "-wal");
        let frozen_shm = sidecar_path(&frozen, "-shm");
        std::fs::copy(&source, &frozen).expect("copy checkpointed main");
        std::fs::copy(&source_wal, &frozen_wal).expect("copy WAL sidecar");
        std::fs::copy(&source_shm, &frozen_shm).expect("copy matching SHM sidecar");
        let paths = [&frozen, &frozen_wal, &frozen_shm];
        let original_permissions = paths.map(|path| {
            let permissions = std::fs::metadata(path)
                .expect("snapshot metadata")
                .permissions();
            let mut frozen_permissions = permissions.clone();
            frozen_permissions.set_readonly(true);
            std::fs::set_permissions(path, frozen_permissions).expect("freeze snapshot file");
            permissions
        });

        let report = ownerless_rows(args(&frozen))
            .await
            .expect("frozen control must list the leftover row");
        assert_eq!(report.counters.scanned, 1);
        assert_eq!(report.counters.ownerless, 1);
        assert_eq!(report.rows[0].record_uuid, id);
        for (path, permissions) in paths.into_iter().zip(original_permissions) {
            std::fs::set_permissions(path, permissions).expect("restore fixture permissions");
        }
        drop(backend);
    }
}
