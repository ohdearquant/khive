//! Read-only operator reports over the canonical main backend's attachment rows.

use crate::sql::sql;

use std::collections::{BTreeSet, HashMap};
use std::fs::Metadata;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use khive_runtime::{BackendKind, KhiveConfig, KhiveRuntime, RuntimeConfig};
#[cfg(all(test, unix))]
use khive_storage::TopLevelMaintenance;
use khive_storage::{SqlAccess, SqlStatement, SqlValue};
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
        ensure_reportable_path(&name, &canonical)?;
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

fn ensure_reportable_path(name: &str, canonical: &Path) -> Result<()> {
    if canonical.to_str().is_none() {
        bail!(
            "roster member {name:?}: canonical path {canonical:?} contains non-UTF-8 bytes and cannot be represented in the JSON report"
        );
    }
    Ok(())
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
    let (statement, params) = match after {
        Some((record_uuid, role)) => (
            sql!("blob_ownerless_attachment_page_after"),
            vec![
                SqlValue::Text(record_uuid.clone()),
                SqlValue::Text(role.clone()),
                SqlValue::Integer(PAGE_SIZE as i64),
            ],
        ),
        None => (
            sql!("blob_ownerless_attachment_page_first"),
            vec![SqlValue::Integer(PAGE_SIZE as i64)],
        ),
    };
    let rows = reader
        .query_all(SqlStatement {
            sql: statement.into(),
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
                record_uuid: row.text("record_uuid").map(str::to_owned).map_err(|_| {
                    anyhow!(
                        "record_uuid: expected text, got {:?}",
                        row.get("record_uuid")
                    )
                })?,
                substrate: row.text("substrate").map(str::to_owned).map_err(|_| {
                    anyhow!("substrate: expected text, got {:?}", row.get("substrate"))
                })?,
                role: row
                    .text("role")
                    .map(str::to_owned)
                    .map_err(|_| anyhow!("role: expected text, got {:?}", row.get("role")))?,
                content_ref: row.text("content_ref").map(str::to_owned).map_err(|_| {
                    anyhow!(
                        "content_ref: expected text, got {:?}",
                        row.get("content_ref")
                    )
                })?,
                media_type: row
                    .opt_text("media_type")
                    .map(|value| value.map(str::to_owned))
                    .map_err(|_| {
                        anyhow!(
                            "media_type: expected nullable text, got {:?}",
                            row.get("media_type")
                        )
                    })?,
                size_bytes: row.opt_i64("size_bytes").map_err(|_| {
                    anyhow!(
                        "size_bytes: expected nullable integer, got {:?}",
                        row.get("size_bytes")
                    )
                })?,
                created_at: row.i64("created_at").map_err(|_| {
                    anyhow!(
                        "created_at: expected integer, got {:?}",
                        row.get("created_at")
                    )
                })?,
                read_at,
            })
        })
        .collect()
}

async fn probe_member(member: &Member, ids: &[String]) -> Result<HashMap<String, Presence>> {
    let sql = member.runtime.sql();
    let mut reader = sql.reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: sql!("blob_ownerless_record_probe").into(),
            params: vec![SqlValue::Text(serde_json::to_string(ids)?)],
            label: Some("ownerless-record-probe".into()),
        })
        .await?;
    drop(reader);
    let mut found = HashMap::<String, Presence>::new();
    for row in rows {
        let id = row
            .text("id")
            .map(str::to_owned)
            .map_err(|_| anyhow!("id: expected text, got {:?}", row.get("id")))?;
        let live = row
            .opt_i64("deleted_at")
            .map_err(|_| {
                anyhow!(
                    "deleted_at: expected nullable integer, got {:?}",
                    row.get("deleted_at")
                )
            })?
            .is_none();
        found.entry(id).or_default().live |= live;
    }
    Ok(found)
}

#[cfg(test)]
#[cfg(unix)]
#[path = "blob_tests.rs"]
mod tests;
