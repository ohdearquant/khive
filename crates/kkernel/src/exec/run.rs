use std::io::{Seek as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use khive_mcp::serve::{
    apply_env_output_format, build_server_multi_backend_with_db_anchor,
    build_single_backend_runtime, config_discovery_db_anchor, enforce_strict_actor_mode,
    normalize_redundant_db_override_with_source, reject_conflicting_db_override_with_source,
    validate_declared_backend_access_modes, validate_wal_ceiling_topology, RuntimeConfigInputs,
};
use khive_mcp::server::KhiveMcpServer;
#[cfg(unix)]
use khive_mcp::server::{compute_config_id, compute_config_id_with_storage_mode};
use khive_mcp::tools::request::RequestParams;
#[cfg(unix)]
use khive_runtime::{daemon::PROTOCOL_VERSION, DaemonRequestFrame};
use khive_runtime::{KhiveConfig, Namespace, RuntimeConfig};
use khive_types::RefusalReason;

use super::ops_file_apply::{apply_ops_file_reader_with_dispatch_mode, OpsFileDispatchMode};
use super::ops_file_validate::{
    parse_atomic_validated_snapshot, preflight_typed_validated_snapshot, validate_ops_file,
    validated_tool_names,
};
use super::refusal::{
    acquire_local_construction_guard, emit_refusal, refusal_error, report_invocation_refusal,
    report_tools_refusal, report_unscoped_refusal, ExecRefusal,
};
#[cfg(unix)]
use super::refusal::{forward_or_spawn_boxed, ForwardFnPtr};
use super::{plan, ExecArgs};

// The scheduled-event drain now lives in `khive-mcp` (ADR-106: the
// daemon-resident tick needs to call it from `khive-mcp::serve`, which
// cannot depend back on `kkernel`).
use khive_mcp::pending_events;

/// Execute the DSL expression, routing through the warm daemon when available.
///
/// Strategy:
/// 1. Build `RuntimeConfig` from args (cheap — no I/O).
/// 2. On Unix, attempt to forward through the daemon via the same
///    length-prefixed socket protocol the MCP stdio server uses (ADR-049).
///    Config and namespace fingerprints are verified by the daemon; a mismatch
///    causes it to respond with a rejection and we fall through to step 3.
/// 3. Fall back to building the full in-process runtime when the daemon is
///    absent, unreachable, or returns a mismatch (KHIVE_NO_DAEMON=1 also skips).
///
/// Output byte-shape is identical in both paths — the daemon echoes the same
/// JSON the local dispatch produces.
///
/// When `--ops-file` is given, steps 2 and 3 differ: the daemon fast-path is
/// skipped entirely, and all ops are dispatched through the in-process runtime
/// in chunks (see module-level docs).
pub async fn run_exec(args: ExecArgs) -> Result<()> {
    if args.read_only {
        if args.plan {
            anyhow::bail!("--read-only conflicts with --plan");
        }
        if args.ops_file.is_some() {
            anyhow::bail!("--read-only conflicts with --ops-file");
        }
        if args.pending_events {
            anyhow::bail!("--read-only conflicts with --pending-events");
        }
        if args.dry_run || args.serial || args.atomic || args.atomic_max_ops.is_some() {
            anyhow::bail!(
                "--read-only supports inline DSL only; it conflicts with --dry-run, --serial, --atomic and --atomic-max-ops"
            );
        }
    }
    if args.plan {
        let result = plan::run(&args).await?;
        writeln!(std::io::stdout().lock(), "{result}")?;
        return Ok(());
    }

    // Clap enforces these relations for normal CLI entry. Keep the same
    // boundary for library callers that construct `ExecArgs` directly.
    if args.serial && (args.ops_file.is_none() || args.ops.is_some() || args.atomic) {
        anyhow::bail!(
            "--serial requires --ops-file and conflicts with positional ops and --atomic"
        );
    }

    // ── pending-events drain ─────────────────────────────────────────────────
    if args.pending_events {
        let summary = pending_events::run_pending_events_with_config(
            args.db.as_deref(),
            args.config.as_deref(),
            &args.namespace,
            args.verbose,
        )
        .await?;
        pending_events::print_summary(&summary);
        return Ok(());
    }

    // ── mutual exclusion check ─────────────────────────────────────────────────
    let mode = match (&args.ops, &args.ops_file) {
        (Some(_), Some(_)) => {
            anyhow::bail!(
                "cannot use both a positional ops string and --ops-file; supply exactly one"
            );
        }
        (None, None) => {
            anyhow::bail!(
                "no ops provided; supply a DSL expression as a positional argument or use \
                 --ops-file <PATH>"
            );
        }
        (Some(ops), None) => ExecMode::Inline(ops.clone()),
        (None, Some(path)) => ExecMode::OpsFile(path.clone()),
    };
    // Parsing is the invocation boundary, before identity/configuration guards
    // choose a competing refusal. This makes malformed inline DSL and malformed
    // JSONL deterministically report `parse-error` regardless of whether strict
    // actor mode or `--expect-actor` would also reject a valid invocation.
    preflight_exec_mode(&mode)?;
    if args.read_only {
        if let ExecMode::Inline(ops) = &mode {
            preflight_read_only_ops(ops)?;
        }
    }

    // Resolve through the SAME TOML-aware path `kkernel mcp` and `kkernel reindex`
    // use (`resolve_runtime_config`), so `kkernel exec`'s config_id and actor
    // identity agree with the daemon's. Previously this built `cfg` from
    // `RuntimeConfig::default()` (env-only) plus an env-only db override and
    // never called `KhiveConfig::load_with_home_fallback` at all, so a project's
    // tier-3 `.khive/config.toml` (`[actor] id`, `[[engines]]`) was invisible to
    // `kkernel exec`. That drift made `compute_config_id(&cfg, None)` diverge
    // from the daemon's TOML-resolved fingerprint, so the daemon rejected the
    // forwarded frame as a `ConfigMismatch` and `exec` silently fell back to an
    // in-process, TOML-blind, effectively-anonymous dispatch (issue #581).
    let (cfg, db_anchor) = match resolve_exec_runtime_config(&args) {
        Ok(resolved) => resolved,
        Err(error) => {
            if let Some(refusal) = error.downcast_ref::<ExecRefusal>() {
                return Err(report_mode_refusal(&mode, refusal.reason, &refusal.message));
            }
            return Err(error);
        }
    };

    // Regression fence: `cfg.db_path` must agree with the canonical anchor for
    // this same `--db`/`KHIVE_DB` input, or `compute_config_id` would silently
    // desynchronize `kkernel exec` from the daemon it is trying to reach.
    khive_runtime::assert_captured_db_anchor_consistent(
        cfg.db_path.as_deref(),
        db_anchor.as_deref(),
    )?;

    let db_context = ExecDbContext {
        raw: args.db,
        anchor: db_anchor,
        config: args.config,
    };

    match mode {
        ExecMode::Inline(ops) => {
            if args.read_only {
                return run_exec_inline_read_only(
                    ops,
                    cfg,
                    args.presentation,
                    args.output_format,
                    args.save_file,
                    db_context,
                    args.strict,
                )
                .await;
            }
            run_exec_inline(
                ops,
                cfg,
                args.presentation,
                args.output_format,
                args.save_file,
                db_context,
                args.strict,
            )
            .await
        }
        ExecMode::OpsFile(path) => {
            run_exec_ops_file(
                path,
                cfg,
                args.presentation,
                args.output_format,
                args.save_file,
                args.dry_run,
                db_context,
                args.serial,
                args.atomic,
                args.atomic_max_ops,
                args.strict,
            )
            .await
        }
    }
}

fn preflight_read_only_ops(ops: &str) -> Result<()> {
    let parsed = khive_request::parse_request(ops).map_err(|error| anyhow::anyhow!("{error}"))?;
    for op in &parsed.ops {
        let declared_read = khive_runtime::classify_operation(&op.tool)
            == Some(khive_runtime::OperationAccess::Read);
        let side_effecting_assertion =
            khive_runtime::VerbRegistry::SIDE_EFFECTING_ASSERTIVE_VERBS.contains(&op.tool.as_str());
        if !declared_read || side_effecting_assertion {
            anyhow::bail!(
                "--read-only refuses operation {:?}: it is not an allowed reviewed read operation",
                op.tool
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod read_only_policy_tests {
    use clap::Parser;
    use serde_json::{json, Value};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    const CHILD_ENV: &str = "KKERNEL_READ_ONLY_POLICY_CHILD";
    const CHILD_ROOT: &str = "KKERNEL_READ_ONLY_CHILD_ROOT";
    const CHILD_TOKEN: &str = "KKERNEL_READ_ONLY_CHILD_TOKEN";
    const CHILD_MARKER: &str = ".kkernel-read-only-child.json";
    const RECEIPT: &str = "READ_ONLY_POLICY_RECEIPT ";
    const OUTPUT_LIMIT: u64 = 1024 * 1024;
    const OUTPUT_HEAD_LIMIT: usize = 2048;
    const SELECTED_READ_TEST: &str =
        "zero_declared_backends_reads_explicit_existing_file_without_changing_store_tree";
    const STORE_BASELINE_DIR: &str = "selected-store-before";
    const STORE_BASELINE_META: &str = ".selected-store-before.json";

    struct ChildFixture {
        root: PathBuf,
        token: String,
        test_name: String,
    }

    fn child_fixture(test_name: &str) -> Option<ChildFixture> {
        let requested = std::env::var_os(CHILD_ENV)?;
        assert_eq!(
            requested.to_str().expect("UTF-8 child kind"),
            test_name,
            "invalid readonly child test kind"
        );
        let root = PathBuf::from(std::env::var_os(CHILD_ROOT).expect("missing private child root"));
        let token = std::env::var(CHILD_TOKEN).expect("missing private child token");
        assert!(token.len() >= 16, "invalid private child token");
        let root = root.canonicalize().expect("private child root must exist");
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("kkernel-read-only-"),
            "invalid child root name"
        );
        assert_eq!(
            std::env::current_dir().unwrap().canonicalize().unwrap(),
            root,
            "child cwd mismatch"
        );
        assert_eq!(
            PathBuf::from(std::env::var_os("HOME").expect("private HOME"))
                .canonicalize()
                .unwrap(),
            root,
            "child HOME mismatch"
        );
        let marker = root.join(CHILD_MARKER);
        let metadata = std::fs::symlink_metadata(&marker).expect("missing parent child marker");
        assert!(
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() < 4096,
            "invalid child marker"
        );
        let marker: Value =
            serde_json::from_slice(&std::fs::read(marker).unwrap()).expect("child marker JSON");
        let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert_eq!(marker["token"], token, "child token mismatch");
        assert_eq!(marker["test_name"], test_name, "child marker kind mismatch");
        assert_eq!(
            marker["exe"],
            exe.display().to_string(),
            "child executable mismatch"
        );
        assert_eq!(
            marker["root"],
            root.display().to_string(),
            "child marker root mismatch"
        );
        let parent_pid = marker["parent_pid"].as_u64().expect("missing parent pid");
        assert!(
            parent_pid != 0 && parent_pid != u64::from(std::process::id()),
            "parent cannot enter child branch"
        );
        Some(ChildFixture {
            root,
            token,
            test_name: test_name.to_string(),
        })
    }

    fn child_receipt(fixture: &ChildFixture, mut receipt: Value) {
        assert!(
            !fixture.root.join(".lattice").exists(),
            "model directory appeared in hermetic fixture"
        );
        receipt["fixture_token"] = json!(fixture.token);
        receipt["test_name"] = json!(fixture.test_name);
        receipt["child_pid"] = json!(std::process::id());
        println!("\n{RECEIPT}{receipt}");
    }

    fn params(ops: String) -> super::RequestParams {
        super::RequestParams {
            plan: None,
            ops,
            presentation: Some("verbose".into()),
            presentation_per_op: None,
            save_to: None,
            format: Some("json".into()),
            format_per_op: None,
            request_id: None,
        }
    }

    async fn create_and_read_back(server: &super::KhiveMcpServer, name: &str) -> String {
        let created = server
            .dispatch_request_local_for_exec(
                params(format!("create(kind=\"concept\", name={})", json!(name))),
                true,
            )
            .await
            .expect("valid writable-fixture create must reach the real handler");
        let created: Value = serde_json::from_str(&created).expect("create JSON");
        assert_eq!(created["results"][0]["ok"], true, "{created}");
        let id = created["results"][0]["result"]["id"]
            .as_str()
            .expect("create must return the stored id")
            .to_string();
        let read = server
            .dispatch_request_local_for_exec(params(format!("get(id={})", json!(id))), true)
            .await
            .expect("read back the real write");
        let read: Value = serde_json::from_str(&read).expect("get JSON");
        assert_eq!(read["results"][0]["ok"], true, "{read}");
        let record = read["results"][0]["result"].to_string();
        assert!(record.contains(&id), "readback omitted stored id: {read}");
        assert!(
            record.contains(name),
            "readback omitted stored name: {read}"
        );
        id
    }

    fn file_runtime(path: &Path, packs: &[&str]) -> khive_runtime::KhiveRuntime {
        let runtime = khive_runtime::KhiveRuntime::new(khive_runtime::RuntimeConfig {
            db_path: Some(path.to_path_buf()),
            packs: packs.iter().map(|name| (*name).to_string()).collect(),
            actor_id: Some("actor:read-only-policy-test".into()),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        })
        .expect("model-less writable file fixture");
        assert!(!runtime.is_read_only(), "seed must be writable");
        runtime
    }

    async fn seed_runtime(runtime: khive_runtime::KhiveRuntime, name: &str) -> String {
        let server = super::KhiveMcpServer::new_with_mounts_and_config(runtime, None)
            .await
            .expect("prepare current production pack schema");
        let id = create_and_read_back(&server, name).await;
        super::settle_exec_storage_before_return(server).await;
        id
    }

    async fn seed_file(path: &Path, name: &str, packs: &[&str]) -> String {
        seed_runtime(file_runtime(path, packs), name).await
    }

    fn try_store_snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>, String> {
        fn visit(
            root: &Path,
            path: &Path,
            files: &mut BTreeMap<PathBuf, Vec<u8>>,
            total: &mut usize,
        ) -> Result<(), String> {
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|error| format!("fixture metadata {}: {error}", path.display()))?;
            if metadata.file_type().is_symlink() {
                return Err("fixture symlink refused".into());
            }
            if metadata.is_dir() {
                for entry in std::fs::read_dir(path)
                    .map_err(|error| format!("fixture directory: {error}"))?
                {
                    let entry = entry.map_err(|error| format!("fixture entry: {error}"))?;
                    visit(root, &entry.path(), files, total)?;
                }
            } else {
                if !metadata.is_file() {
                    return Err("special fixture file refused".into());
                }
                if metadata.len() > 16 * 1024 * 1024 {
                    return Err("oversized fixture file".into());
                }
                let bytes = std::fs::read(path)
                    .map_err(|error| format!("snapshot private fixture bytes: {error}"))?;
                *total = total
                    .checked_add(bytes.len())
                    .ok_or_else(|| "fixture snapshot size overflow".to_string())?;
                if *total > 64 * 1024 * 1024 {
                    return Err("oversized fixture snapshot".into());
                }
                if files.len() >= 32 {
                    return Err("too many fixture files".into());
                }
                let relative = path
                    .strip_prefix(root)
                    .map_err(|error| format!("fixture relative path: {error}"))?;
                files.insert(relative.to_path_buf(), bytes);
            }
            Ok(())
        }
        let mut files = BTreeMap::new();
        let mut total = 0;
        visit(root, root, &mut files, &mut total)?;
        Ok(files)
    }

    fn store_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        try_store_snapshot(root).expect("snapshot bounded private fixture tree")
    }

    fn record_store_baseline(
        fixture: &ChildFixture,
        before: BTreeMap<PathBuf, Vec<u8>>,
        record_id: &str,
    ) {
        let baseline = fixture.root.join(STORE_BASELINE_DIR);
        std::fs::create_dir(&baseline).expect("private store baseline directory");
        assert!(
            before.contains_key(Path::new("selected.db")),
            "missing selected database baseline"
        );
        for (relative, bytes) in before {
            let target = baseline.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).expect("baseline parent directory");
            std::fs::write(target, bytes).expect("persist private store baseline");
        }
        // This marker is written only after the actual seed create/readback and
        // every baseline byte are settled, and before invoking run_exec.
        std::fs::write(
            fixture.root.join(STORE_BASELINE_META),
            serde_json::to_vec(&json!({
                "fixture_token": fixture.token,
                "test_name": fixture.test_name,
                "child_pid": std::process::id(),
                "record_id": record_id,
                "complete": true,
            }))
            .unwrap(),
        )
        .expect("persist private baseline marker");
    }

    #[derive(Debug)]
    struct ParentStoreWitness {
        record_id: String,
        changed_paths: Vec<PathBuf>,
    }

    fn parent_store_witness(
        root: &Path,
        token: &str,
        test_name: &str,
        child_pid: u32,
    ) -> Result<Option<ParentStoreWitness>, String> {
        if test_name != SELECTED_READ_TEST {
            return Ok(None);
        }
        let marker_path = root.join(STORE_BASELINE_META);
        let metadata = std::fs::symlink_metadata(&marker_path)
            .map_err(|error| format!("missing selected-store baseline: {error}"))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() >= 4096 {
            return Err("invalid selected-store baseline marker".into());
        }
        let marker: Value = serde_json::from_slice(
            &std::fs::read(marker_path)
                .map_err(|error| format!("read baseline marker: {error}"))?,
        )
        .map_err(|error| format!("baseline marker JSON: {error}"))?;
        if marker["fixture_token"] != token
            || marker["test_name"] != test_name
            || marker["child_pid"].as_u64() != Some(u64::from(child_pid))
            || marker["complete"] != true
        {
            return Err("selected-store baseline attribution mismatch".into());
        }
        let record_id = marker["record_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "baseline record id missing".to_string())?
            .to_string();
        let before = try_store_snapshot(&root.join(STORE_BASELINE_DIR))?;
        let after = try_store_snapshot(&root.join("stores"))?;
        if !before.contains_key(Path::new("selected.db")) {
            return Err("selected database absent from baseline".into());
        }
        let paths: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
        let changed_paths = paths
            .into_iter()
            .filter(|path| before.get(path) != after.get(path))
            .collect();
        Ok(Some(ParentStoreWitness {
            record_id,
            changed_paths,
        }))
    }

    fn stream_diagnostic(name: &str, bytes: &[u8]) -> String {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(OUTPUT_HEAD_LIMIT)]);
        format!(
            "{name}: captured_bytes={}, limit_reached={}, first_{}KiB={head:?}",
            bytes.len(),
            bytes.len() >= OUTPUT_LIMIT as usize,
            OUTPUT_HEAD_LIMIT / 1024,
        )
    }

    #[tokio::test]
    async fn zero_declared_backends_reads_explicit_existing_file_without_changing_store_tree() {
        if let Some(fixture) = child_fixture(
            "zero_declared_backends_reads_explicit_existing_file_without_changing_store_tree",
        ) {
            let cwd = &fixture.root;
            let seed_stores = cwd.join("seed-stores");
            std::fs::create_dir(&seed_stores).unwrap();
            let name = "read-only-explicit-database-row";
            let id = seed_file(&seed_stores.join("selected.db"), name, &["kg"]).await;
            let seed_snapshot = store_snapshot(&seed_stores);
            let stores = cwd.join("stores");
            std::fs::create_dir(&stores).unwrap();
            for (relative, bytes) in &seed_snapshot {
                let target = stores.join(relative);
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::write(target, bytes).unwrap();
            }
            let db = stores.join("selected.db");
            let config = cwd.join("inspection.toml");
            std::fs::write(&config, "engines = []\n").unwrap();
            let before = store_snapshot(&stores);
            assert!(
                before == seed_snapshot,
                "copied store differs from settled seed"
            );
            drop(seed_snapshot);
            record_store_baseline(&fixture, before.clone(), &id);
            // The explicit database holds a distinct actual record; the HOME
            // default database does not exist and must remain absent.
            let mut read_results = serde_json::Map::new();
            for (verb, ops) in [
                ("whoami", "whoami()".to_string()),
                ("get", format!("get(id={})", json!(id))),
                (
                    "search",
                    format!(
                        "search(query={}, kind=\"concept\", source=\"text\")",
                        json!(name)
                    ),
                ),
            ] {
                let output = cwd.join(format!("explicit-{verb}.jsonl"));
                let invocation = super::ExecArgs::try_parse_from([
                    "exec",
                    &ops,
                    "--read-only",
                    "--db",
                    db.to_str().unwrap(),
                    "--config",
                    config.to_str().unwrap(),
                    "--save-file",
                    output.to_str().unwrap(),
                    "--output-format",
                    "json",
                    "--actor",
                    "actor:read-only-policy-test",
                ])
                .unwrap();
                super::run_exec(invocation)
                    .await
                    .unwrap_or_else(|error| panic!("readonly copied-store {verb}: {error}"));
                let rows = std::fs::read_to_string(output).expect("saved read result");
                let mut lines = rows.lines();
                let row: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
                assert!(lines.next().is_none(), "one result row per read invocation");
                assert_eq!(row["ok"], true, "{verb}: {row}");
                match verb {
                    "whoami" => {
                        assert_eq!(row["result"]["actor_id"], "actor:read-only-policy-test");
                        assert_eq!(row["result"]["actor_kind"], "actor");
                        assert_eq!(row["result"]["unattributed"], false);
                    }
                    "get" => {
                        assert_eq!(row["result"]["id"].as_str(), Some(id.as_str()), "{row}");
                        assert_eq!(row["result"]["name"], name, "{row}");
                    }
                    "search" => {
                        let hits = row["result"].as_array().expect("search results array");
                        assert!(
                            hits.iter().any(|hit| {
                                hit["id"].as_str() == Some(id.as_str()) && hit["name"] == name
                            }),
                            "search did not return the copied record: {row}"
                        );
                    }
                    _ => unreachable!("closed read fixture cases"),
                }
                read_results.insert(
                    verb.to_string(),
                    json!({
                        "result_verified": true,
                        "store_tree_unchanged": before == store_snapshot(&stores),
                    }),
                );
            }

            let invocation = super::ExecArgs::try_parse_from([
                "exec",
                "create(kind=\"concept\", name=\"read-only-copied-store-write-probe\")",
                "--read-only",
                "--db",
                db.to_str().unwrap(),
                "--config",
                config.to_str().unwrap(),
                "--actor",
                "actor:read-only-policy-test",
            ])
            .unwrap();
            let refusal = super::run_exec(invocation)
                .await
                .expect_err("readonly create must refuse before dispatch")
                .to_string();
            assert!(
                refusal.contains("--read-only refuses operation \"create\""),
                "wrong write refusal: {refusal}"
            );
            let write_store_unchanged = before == store_snapshot(&stores);
            assert!(
                !cwd.join(".khive/khive.db").exists(),
                "created HOME default db"
            );
            // The parent compares actual disk bytes independently of capture.
            // A semantic byte difference must not dump a SQLite Vec<u8> here,
            // abort the child before receipt, or become a capture-limit failure.
            child_receipt(
                &fixture,
                json!({
                    "explicit_database_actual_record": true,
                    "copied_store_verified": true,
                    "read_results": read_results,
                    "write_refused": true,
                    "write_store_tree_unchanged": write_store_unchanged,
                    "home_default_absent": true,
                    "id": id,
                }),
            );
            return;
        }
        let receipt = isolated_child(
            "zero_declared_backends_reads_explicit_existing_file_without_changing_store_tree",
            "kg",
        )
        .await;
        assert_eq!(
            receipt["explicit_database_actual_record"], true,
            "{receipt}"
        );
        assert_eq!(receipt["copied_store_verified"], true, "{receipt}");
        assert_eq!(receipt["read_results"].as_object().unwrap().len(), 3);
        for verb in ["whoami", "get", "search"] {
            assert_eq!(
                receipt["read_results"][verb]["result_verified"], true,
                "{receipt}"
            );
            assert_eq!(
                receipt["read_results"][verb]["store_tree_unchanged"], true,
                "readonly {verb} changed store bytes: {receipt}"
            );
        }
        assert_eq!(receipt["write_refused"], true, "{receipt}");
        assert_eq!(receipt["write_store_tree_unchanged"], true, "{receipt}");
        assert_eq!(
            receipt["selected_store_tree_unchanged"], true,
            "readonly store bytes changed: {receipt}"
        );
        assert_eq!(receipt["home_default_absent"], true, "{receipt}");
    }

    #[tokio::test]
    async fn missing_registered_pack_column_refuses_read_only_boot_without_ddl() {
        if let Some(fixture) =
            child_fixture("missing_registered_pack_column_refuses_read_only_boot_without_ddl")
        {
            let cwd = &fixture.root;
            let stores = cwd.join("stores");
            std::fs::create_dir(&stores).unwrap();
            let current = stores.join("current.db");
            let current_id = seed_file(
                &current,
                "read-only-current-session-schema",
                &["kg", "session"],
            )
            .await;
            let config = cwd.join("inspection.toml");
            std::fs::write(&config, "engines = []\n").unwrap();
            let current_output = cwd.join("current-read.jsonl");
            let before_current = store_snapshot(&stores);
            let current_args = super::ExecArgs::try_parse_from([
                "exec",
                &format!("get(id={})", json!(current_id)),
                "--read-only",
                "--db",
                current.to_str().unwrap(),
                "--config",
                config.to_str().unwrap(),
                "--save-file",
                current_output.to_str().unwrap(),
                "--actor",
                "actor:read-only-policy-test",
            ])
            .unwrap();
            super::run_exec(current_args)
                .await
                .expect("current registered-pack schema must boot");
            let saved = std::fs::read_to_string(current_output).unwrap();
            let row: Value = serde_json::from_str(saved.lines().next().unwrap()).unwrap();
            assert_eq!(row["ok"], true, "{row}");
            assert!(row["result"].to_string().contains(&current_id), "{row}");
            assert_eq!(
                store_snapshot(&stores),
                before_current,
                "current readonly baseline changed store bytes"
            );

            // KG-only setup preserves the exact released historical cursor
            // shape while keeping the core and KG schemas current.
            let stale = stores.join("stale.db");
            let runtime = file_runtime(&stale, &["kg"]);
            let mut writer = runtime.sql().writer().await.expect("private seed writer");
            writer.execute_script(
                "CREATE TABLE session_mirror_cursor (file_path TEXT PRIMARY KEY, session_id TEXT, byte_offset INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL)".to_string(),
            ).await.expect("released historical session cursor schema");
            drop(writer);
            let stale_id = seed_runtime(runtime, "read-only-stale-session-schema").await;
            let before_stale = store_snapshot(&stores);
            let stale_output = cwd.join("stale-read.jsonl");
            let stale_args = super::ExecArgs::try_parse_from([
                "exec",
                &format!("get(id={})", json!(stale_id)),
                "--read-only",
                "--db",
                stale.to_str().unwrap(),
                "--config",
                config.to_str().unwrap(),
                "--save-file",
                stale_output.to_str().unwrap(),
                "--actor",
                "actor:read-only-policy-test",
            ])
            .unwrap();
            let error = super::run_exec(stale_args)
                .await
                .expect_err("missing registered pack column must refuse boot");
            let message = format!("{error:#}");
            assert!(
                message.contains("read-only schema validation failed"),
                "wrong boot refusal: {message}"
            );
            assert!(
                message.contains("session_mirror_cursor.file_identity"),
                "wrong missing column: {message}"
            );
            assert!(
                message.contains("open the database writable"),
                "missing repair guidance: {message}"
            );
            assert!(!stale_output.exists(), "reserved export before failed boot");
            assert_eq!(
                store_snapshot(&stores),
                before_stale,
                "readonly stale schema performed DDL or changed store bytes"
            );
            child_receipt(
                &fixture,
                json!({
                    "compatible_registered_schema_positive": true,
                    "named_missing_pack_column_refused": true,
                    "selected_store_tree_unchanged": true,
                    "output_not_reserved": true,
                }),
            );
            return;
        }
        let receipt = isolated_child(
            "missing_registered_pack_column_refuses_read_only_boot_without_ddl",
            "kg,session",
        )
        .await;
        for key in [
            "compatible_registered_schema_positive",
            "named_missing_pack_column_refused",
            "selected_store_tree_unchanged",
            "output_not_reserved",
        ] {
            assert_eq!(receipt[key], true, "{receipt}");
        }
    }

    #[tokio::test]
    async fn declaration_gate_blocks_actual_writable_dispatch() {
        if let Some(fixture) = child_fixture("declaration_gate_blocks_actual_writable_dispatch") {
            let runtime = khive_runtime::KhiveRuntime::new(khive_runtime::RuntimeConfig {
                db_path: None,
                packs: vec!["kg".into()],
                actor_id: Some("actor:read-only-policy-test".into()),
                ..khive_runtime::RuntimeConfig::no_embeddings()
            })
            .expect("model-less writable in-memory fixture");
            let server = super::KhiveMcpServer::new_with_mounts_and_config(runtime, None)
                .await
                .expect("wire the real handler fixture");
            // Qualify the same write kind and readback before interpreting the
            // policy result. Setup and storage failures earn no gate credit.
            let baseline_id = create_and_read_back(&server, "read-only-policy-baseline").await;
            let probe = "create(kind=\"concept\", name=\"read-only-policy-probe\")";
            let blocked = super::preflight_read_only_ops(probe).is_err();
            let probe_id = if blocked {
                None
            } else {
                Some(create_and_read_back(&server, "read-only-policy-probe").await)
            };
            super::settle_exec_storage_before_return(server).await;
            child_receipt(
                &fixture,
                json!({
                    "baseline_id": baseline_id,
                    "baseline_handler_and_readback": true,
                    "policy_blocked": blocked,
                    "probe_handler_and_readback": probe_id.is_some(),
                    "probe_id": probe_id,
                }),
            );
            return;
        }
        let receipt =
            isolated_child("declaration_gate_blocks_actual_writable_dispatch", "kg").await;
        assert_eq!(receipt["baseline_handler_and_readback"], true, "{receipt}");
        assert!(receipt["baseline_id"].as_str().is_some(), "{receipt}");
        if receipt["policy_blocked"] == false {
            assert_eq!(receipt["probe_handler_and_readback"], true, "{receipt}");
            assert!(receipt["probe_id"].as_str().is_some(), "{receipt}");
        }
        assert_eq!(
            receipt["policy_blocked"], true,
            "read_only_dispatch_not_reached: {receipt}"
        );
        assert_eq!(receipt["probe_handler_and_readback"], false, "{receipt}");
    }

    async fn isolated_child(test_name: &str, packs: &str) -> Value {
        let root = tempfile::Builder::new()
            .prefix("kkernel-read-only-")
            .tempdir()
            .expect("private policy fixture");
        let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
        let token = format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            root.path().file_name().unwrap().to_string_lossy(),
        );
        std::fs::write(
            root.path().join(CHILD_MARKER),
            serde_json::to_vec(&json!({
                "token": token,
                "test_name": test_name,
                "exe": exe.display().to_string(),
                "root": root.path().canonicalize().unwrap().display().to_string(),
                "parent_pid": std::process::id(),
            }))
            .unwrap(),
        )
        .expect("parent-owned child marker");
        let filter = module_path!().split_once("::").expect("test module").1;
        let mut command = tokio::process::Command::new(exe);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("KHIVE_")
                || name.to_string_lossy().starts_with("KKERNEL_READ_ONLY_")
            {
                command.env_remove(name);
            }
        }
        command
            .args(["--exact", &format!("{filter}::{test_name}"), "--nocapture"])
            .env(CHILD_ENV, test_name)
            .env(CHILD_ROOT, root.path())
            .env(CHILD_TOKEN, &token)
            .env_remove("LATTICE_MODEL_CACHE")
            .env("HOME", root.path())
            .env("KHIVE_PACKS", packs)
            .env("KHIVE_EVENTS_SPLIT", "0")
            .env("KHIVE_VOLUME_LOCK_DIR", root.path().join("volume-locks"))
            .current_dir(root.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("spawn isolated policy test");
        let child_pid = child.id().expect("spawned child pid");
        let out = child.stdout.take().expect("child stdout");
        let err = child.stderr.take().expect("child stderr");
        let out = tokio::spawn(async move {
            let mut bytes = Vec::new();
            out.take(OUTPUT_LIMIT).read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        });
        let err = tokio::spawn(async move {
            let mut bytes = Vec::new();
            err.take(OUTPUT_LIMIT).read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        });
        let status = match tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
            Ok(result) => result.expect("wait policy child"),
            Err(_) => {
                let _ = child.start_kill();
                let reaped = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                out.abort();
                err.abort();
                assert!(matches!(reaped, Ok(Ok(_))), "policy child was not reaped");
                let disk_witness = parent_store_witness(root.path(), &token, test_name, child_pid);
                root.close().expect("remove timed-out private fixture");
                panic!("policy fixture watchdog expired; no semantic gate credit; selected_store_witness={disk_witness:?}");
            }
        };
        let out = out.await.expect("join stdout").expect("read stdout");
        let err = err.await.expect("join stderr").expect("read stderr");
        let capture_valid = out.len() < OUTPUT_LIMIT as usize && err.len() < OUTPUT_LIMIT as usize;
        let capture_diagnostic = format!(
            "{}; {}",
            stream_diagnostic("stdout", &out),
            stream_diagnostic("stderr", &err)
        );
        // This disk verdict is captured after reaping and before deleting the
        // private fixture, regardless of cap/exit/receipt validity. It is only a
        // diagnostic until every existing fixture-validity guard below passes.
        let disk_witness = parent_store_witness(root.path(), &token, test_name, child_pid);
        let out = String::from_utf8_lossy(&out);
        let rows: Vec<_> = out
            .lines()
            .filter_map(|line| line.strip_prefix(RECEIPT))
            .collect();
        root.close().expect("remove completed private fixture");
        assert!(
            capture_valid,
            "invalid policy fixture: child output reached the capture limit; {capture_diagnostic}; selected_store_witness={disk_witness:?}"
        );
        assert!(
            status.success(),
            "invalid policy fixture exit ({status}); {capture_diagnostic}; selected_store_witness={disk_witness:?}"
        );
        assert_eq!(rows.len(), 1, "missing/duplicate child receipt; {capture_diagnostic}; selected_store_witness={disk_witness:?}");
        let mut receipt: Value = serde_json::from_str(rows[0]).expect("policy receipt JSON");
        assert_eq!(
            receipt["fixture_token"], token,
            "child receipt token mismatch"
        );
        assert_eq!(
            receipt["test_name"], test_name,
            "child receipt kind mismatch"
        );
        assert_eq!(
            receipt["child_pid"].as_u64(),
            Some(u64::from(child_pid)),
            "child receipt pid mismatch"
        );
        if let Some(witness) =
            disk_witness.expect("invalid selected-store baseline; no semantic gate credit")
        {
            assert_eq!(
                receipt["explicit_database_actual_record"], true,
                "missing actual selected-database readback"
            );
            assert_eq!(
                receipt["id"].as_str(),
                Some(witness.record_id.as_str()),
                "baseline/readback record mismatch"
            );
            receipt["selected_store_tree_unchanged"] = json!(witness.changed_paths.is_empty());
            receipt["selected_store_changed_paths"] = json!(witness.changed_paths);
        }
        receipt
    }
}

async fn run_exec_inline_read_only(
    ops: String,
    cfg: RuntimeConfig,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    db_context: ExecDbContext,
    strict: bool,
) -> Result<()> {
    if let Err(error) = enforce_strict_actor_mode(
        cfg.actor_id.as_deref(),
        &cfg.packs,
        cfg.require_attributed_actor,
    ) {
        return Err(report_invocation_refusal(
            Some(&ops),
            RefusalReason::AnonymousActor,
            error,
        ));
    }
    let (khive_cfg, _) = load_exec_config(&db_context)?;
    if !khive_cfg.backends.is_empty() {
        anyhow::bail!("--read-only does not support declared backends");
    }
    if !cfg.mounts.is_empty() {
        anyhow::bail!("--read-only does not support configured mounts");
    }
    if cfg.db_path.is_none() {
        anyhow::bail!("--read-only requires an existing file-backed database");
    }
    validate_wal_ceiling_topology(&cfg, &khive_cfg.backends, false)?;
    let path = cfg
        .db_path
        .as_deref()
        .expect("file-backed path checked above");
    if !path.is_file() {
        anyhow::bail!(
            "--read-only requires an existing file-backed database: {}",
            path.display()
        );
    }
    disclose_resolved_database(&cfg, &khive_cfg, false);
    disclose_resolved_actor(&cfg);
    // This path never takes the writable construction guard, prepares host
    // schema/cutover state or forwards to a daemon. The runtime validates the
    // existing schema and the registry validates pack columns without DDL.
    let runtime = khive_runtime::KhiveRuntime::new_readonly(cfg)?;
    if !runtime.is_read_only() {
        anyhow::bail!("--read-only requires a read-only file-backed runtime");
    }
    let env_fmt = apply_env_output_format(khive_cfg.runtime.default_output_format);
    let server = KhiveMcpServer::new_with_mounts_and_config(runtime, Some(&khive_cfg))
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?
        .with_default_output_format(env_fmt);
    let params = RequestParams {
        plan: None,
        ops,
        presentation,
        presentation_per_op: None,
        save_to: save_file,
        format: output_format,
        format_per_op: None,
        request_id: None,
    };
    let result = server
        .dispatch_request_local_for_exec(params, strict)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"));
    let result = match result {
        Ok(raw_output) => {
            let output = prepare_exec_output(&raw_output, strict);
            println!("{output}");
            enforce_strict_batch_result(&output, strict)
        }
        Err(error) => Err(error),
    };
    settle_exec_storage_before_return(server).await;
    result
}

fn resolve_exec_runtime_config(args: &ExecArgs) -> Result<(RuntimeConfig, Option<PathBuf>)> {
    let namespace =
        Namespace::parse(&args.namespace).map_err(|error| anyhow::anyhow!("{error}"))?;
    let (mut config, db_anchor, ownership) =
        khive_mcp::serve::resolve_runtime_config_with_actor_ownership(RuntimeConfigInputs {
            db: args.db.as_deref(),
            config: args.config.as_deref(),
            namespace,
            // `--namespace` has a clap `default_value = "local"`, so it is always
            // present: there is no way to distinguish "operator typed --namespace
            // local" from "operator didn't pass --namespace at all". `true` is the
            // conservative, behavior-preserving choice: it keeps exec's pre-existing
            // semantics (the CLI/default value always becomes `default_namespace`,
            // matching what `resolve_runtime_config`'s embed path already did
            // unconditionally). It is also empirically inert for config_id parity:
            // in the embed path (`no_embed: false`, exec's only mode), this flag
            // gates only the actor_id fill-when-None guard in `resolve_runtime_config`
            // and `compute_config_id` never reads identity fields (`actor_id` or
            // `visible_namespaces`; namespace is carried separately per its own doc
            // comment). See the
            // `namespace_explicit_changes_actor_id_fill_but_not_config_id` and
            // `exec_config_id_matches_serve_config_id_for_project_toml_actor` tests
            // in `exec_tests/config_identity_tests.rs`, which construct both arms and
            // assert this directly rather than assuming it.
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })?;

    // ADR-170 embedded mode: `kkernel exec`'s in-process fallback is a
    // one-shot: a socket forwarder would be reaped at process exit before
    // delivering, losing every event. The shared resolver already emits
    // direct (socket-less) mode for exactly this class of host; only the
    // resident daemon entrypoints upgrade to forwarding
    // (`enable_events_forwarding_for_daemon`). SQLite's per-file
    // cross-process exclusion covers direct appends overlapping a running
    // events daemon.
    debug_assert!(config
        .events_split
        .as_ref()
        .is_none_or(|split| split.socket_path.is_none()));

    // Apply the explicit actor only AFTER the shared resolver has loaded the
    // project/config/environment fallbacks. This makes the CLI value the true
    // highest-precedence tier without coupling identity to storage namespace.
    // Keeping the selected identity in `cfg` also sends every execution mode
    // through its existing gate seam: daemon request identity, local registry
    // dispatch, ops-file dispatch, or atomic apply's pre-write authorization.
    // `[actor] owned_namespaces` is finalized only after that pin, against the
    // actor whose configuration declared it (ADR-021 Delta 1).
    apply_actor_pin_and_expectation(
        &mut config,
        args.actor.as_deref(),
        args.expect_actor.as_deref(),
    )?;
    ownership.finalize(&mut config);
    Ok((config, db_anchor))
}

/// Apply the explicit exec actor tier and validate an optional identity
/// expectation before any daemon forwarding or local dispatch occurs.
pub(super) fn apply_actor_pin_and_expectation(
    cfg: &mut RuntimeConfig,
    actor: Option<&str>,
    expect_actor: Option<&str>,
) -> Result<()> {
    if let Some(raw) = actor {
        let parsed =
            Namespace::parse(raw).map_err(|e| anyhow::anyhow!("invalid --actor {raw:?}: {e}"))?;

        // The resolver may have already folded the displaced actor (project
        // `[actor] id`, `KHIVE_ACTOR`, etc.) into the default read visible-set
        // (ADR-007 Rev 4 Rule 3b). Drop exactly that entry before pinning, so
        // the new identity's default reads don't keep exposing the actor it
        // replaced; any other explicitly configured `visible_namespaces` entry
        // is untouched.
        if let Some(prev) = cfg.actor_id.as_deref() {
            if let Ok(prev_ns) = Namespace::parse(prev) {
                cfg.visible_namespaces.retain(|ns| *ns != prev_ns);
            }
        }
        cfg.actor_id = if parsed == Namespace::local() {
            None
        } else {
            if !cfg.visible_namespaces.contains(&parsed) {
                cfg.visible_namespaces.push(parsed.clone());
            }
            Some(parsed.as_str().to_owned())
        };
    }

    if let Some(raw_expected) = expect_actor {
        let expected = Namespace::parse(raw_expected)
            .map_err(|e| anyhow::anyhow!("invalid --expect-actor {raw_expected:?}: {e}"))?;
        let actual = khive_runtime::resolve_actor(cfg.actor_id.as_deref());
        if actual.id != expected.as_str() {
            return Err(refusal_error(
                RefusalReason::ExpectActorMismatch,
                format!(
                    "--expect-actor mismatch: expected {:?}, resolved {:?}",
                    expected.as_str(),
                    actual.id
                ),
            ));
        }
    }

    Ok(())
}

/// Decides the process exit code from the response envelope's `summary`.
/// `raw` must be the exact envelope string already printed to stdout — the
/// caller prints first, unconditionally, then this decides the exit code; a
/// caller piping the output still sees the full result either way.
///
/// Two tiers (#1220, #1339):
/// - Always: `Err` when the batch had ops and none succeeded. A fully-failed
///   invocation has no success to report; scripted single-op callers (the
///   dominant `exec` shape) check the process exit code, and exiting 0 there
///   converts loud op-level rejections into silent drops.
/// - `--strict` only: `Err` when any op failed or aborted (partial failure).
pub(super) fn enforce_strict_batch_result(raw: &str, strict: bool) -> Result<()> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw) else {
        // Non-JSON output (e.g. --output-format table/auto): nothing to
        // inspect here. Exit-code enforcement only applies to the default
        // JSON shape.
        return Ok(());
    };
    let succeeded = parsed["summary"]["succeeded"].as_u64().unwrap_or(0);
    let failed = parsed
        .get("summary")
        .and_then(|summary| summary.get("failed"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let aborted = parsed
        .get("summary")
        .and_then(|summary| summary.get("aborted"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if succeeded == 0 && (failed > 0 || aborted > 0) {
        anyhow::bail!(
            "every op failed: {failed} failed, {aborted} aborted, 0 succeeded (see printed output above)"
        );
    }
    if strict && (failed > 0 || aborted > 0) {
        anyhow::bail!(
            "--strict: {failed} op(s) failed, {aborted} op(s) aborted (see printed output above)"
        );
    }
    Ok(())
}

/// Emit stable classifications already attached by the dispatch layer. Under
/// `--strict`, otherwise-unclassified failed or aborted entries receive
/// `strict-op-failure`; a more specific server-owned reason always wins.
/// Returns whether the JSON value changed.
pub(super) fn annotate_and_emit_refusals(parsed: &mut serde_json::Value, strict: bool) -> bool {
    // Invocation-level errors produced by other CLI guards have no per-op
    // result array. Preserve their shape while still honoring a known token.
    if let Some(reason) = parsed
        .get("error")
        .and_then(|error| error.get("reason"))
        .and_then(serde_json::Value::as_str)
        .and_then(RefusalReason::from_token)
    {
        emit_refusal(reason);
        return false;
    }

    let failed = parsed["summary"]["failed"].as_u64().unwrap_or(0);
    let aborted = parsed["summary"]["aborted"].as_u64().unwrap_or(0);
    let strict_refusal = strict && (failed > 0 || aborted > 0);
    // Save manifests preserve compact failure metadata instead of the full
    // result payload. Treat that projection exactly like canonical results.
    let entries = parsed.as_object_mut().and_then(|object| {
        if object
            .get("results")
            .is_some_and(serde_json::Value::is_array)
        {
            object
                .get_mut("results")
                .and_then(serde_json::Value::as_array_mut)
        } else {
            object
                .get_mut("failures")
                .and_then(serde_json::Value::as_array_mut)
        }
    });

    let mut changed = false;
    let mut emitted = 0usize;
    if let Some(entries) = entries {
        for entry in entries {
            if entry["ok"].as_bool() == Some(true) {
                continue;
            }

            let specific = entry["reason"].as_str().and_then(RefusalReason::from_token);
            let reason = specific.or(strict_refusal.then_some(RefusalReason::StrictOpFailure));
            if let Some(reason) = reason {
                if specific.is_none() {
                    if let Some(object) = entry.as_object_mut() {
                        object.insert("reason".to_string(), serde_json::json!(reason.as_str()));
                        changed = true;
                    }
                }
                emit_refusal(reason);
                emitted += 1;
            }
        }
    }

    // A legacy aggregate can report failures without retaining per-op rows.
    // Keep strict mode machine-classifiable without inventing missing rows.
    if strict_refusal && emitted == 0 {
        emit_refusal(RefusalReason::StrictOpFailure);
    }
    changed
}

/// Prepare the exact string printed by inline exec. Existing JSON stays
/// byte-for-byte unchanged unless strict-mode annotation added a reason.
pub(super) fn prepare_exec_output(raw: &str, strict: bool) -> String {
    let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(raw) else {
        return raw.to_owned();
    };
    if annotate_and_emit_refusals(&mut parsed, strict) {
        serde_json::to_string(&parsed).expect("serde_json::Value is serializable")
    } else {
        raw.to_owned()
    }
}

enum ExecMode {
    Inline(String),
    OpsFile(PathBuf),
}

fn preflight_inline_ops(ops: &str) -> Result<()> {
    if let Err(error) = khive_request::parse_request(ops) {
        // Keep the CLI's established rendered prose byte-for-byte unchanged;
        // the invocation-level error object carries the structured reason.
        let error = rmcp::ErrorData::invalid_params(error.to_string(), None);
        return Err(report_unscoped_refusal(
            RefusalReason::ParseError,
            error.to_string(),
        ));
    }
    Ok(())
}

/// Validate the selected carrier before any actor expectation or dispatch gate.
/// The operation list remains authoritative only after this succeeds.
fn preflight_exec_mode(mode: &ExecMode) -> Result<()> {
    match mode {
        ExecMode::Inline(ops) => preflight_inline_ops(ops),
        ExecMode::OpsFile(path) => match validate_ops_file(path) {
            Ok(_) => Ok(()),
            Err(error) => {
                if let Some(refusal) = error.downcast_ref::<ExecRefusal>() {
                    Err(report_unscoped_refusal(
                        refusal.reason,
                        refusal.message.as_str(),
                    ))
                } else {
                    Err(error)
                }
            }
        },
    }
}

/// Report an invocation-level refusal against the real operation set whenever
/// that set can be parsed without dispatch. In particular, `--expect-actor`
/// mismatches happen before execution but a valid ops-file is still safe to
/// read and parse for its tool names; reporting one synthetic operation would
/// make `summary.total` and per-op correlation false.
fn report_mode_refusal(
    mode: &ExecMode,
    reason: RefusalReason,
    error: impl std::fmt::Display,
) -> anyhow::Error {
    let message = error.to_string();
    let parsed = match mode {
        ExecMode::Inline(raw) => khive_request::parse_request(raw).ok().map(|request| {
            let chain = request.mode == khive_request::ExecutionMode::Chain;
            let tools = request.ops.into_iter().map(|op| op.tool).collect();
            (tools, chain)
        }),
        ExecMode::OpsFile(path) => validate_ops_file(path).ok().and_then(|mut validated| {
            validated_tool_names(&mut validated.snapshot)
                .ok()
                .map(|tools| (tools, false))
        }),
    };
    match parsed {
        Some((tools, chain)) if !tools.is_empty() => {
            report_tools_refusal(tools, chain, reason, message)
        }
        _ => report_unscoped_refusal(reason, message),
    }
}

/// Issue #1586: disclose the resolved database target(s) once, before any
/// dispatch, so a no-override invocation's silent default
/// (`$HOME/.khive/khive.db` — the production database for most installs) is
/// visible. Emitted after the caller has loaded the `[[backends]]` topology,
/// so the line names the config-declared backend targets when those — not
/// `cfg.db_path` — are what receive writes. Stderr rather than a tracing
/// record because kkernel's default log level is `warn` (an INFO record would
/// never surface) and stdout is reserved for JSON results. Best-effort write:
/// the disclosure is nonessential, so a closed or failing stderr must not
/// become an exec failure (`eprintln!` panics on a failed stderr write).
/// Disclosure only: no prompt, no refusal.
fn disclose_resolved_database(cfg: &RuntimeConfig, khive_cfg: &KhiveConfig, force_memory: bool) {
    use std::io::Write;
    let line =
        khive_mcp::serve::resolved_database_disclosure(cfg.db_path.as_deref(), &khive_cfg.backends);
    let _ = writeln!(std::io::stderr(), "{line}");
    let lock_line =
        khive_mcp::serve::resolved_volume_lock_disclosure(cfg.volume_lock_dir.as_deref());
    let _ = writeln!(std::io::stderr(), "{lock_line}");
    let wal_line =
        khive_mcp::serve::resolved_wal_ceiling_disclosure(cfg, &khive_cfg.backends, force_memory);
    let _ = writeln!(std::io::stderr(), "{wal_line}");
}

fn disclose_resolved_actor(cfg: &RuntimeConfig) {
    use std::io::Write;
    let line = khive_mcp::serve::resolved_actor_disclosure(cfg.actor_id.as_deref());
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[cfg(unix)]
fn disclose_daemon_execution() {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr(),
        "execution: answered by daemon; --log and KHIVE_LOG set the client process log level only; \
         the daemon log level is fixed at startup"
    );
}

#[derive(Default)]
pub(super) struct ExecDbContext {
    pub(super) raw: Option<String>,
    pub(super) anchor: Option<PathBuf>,
    pub(super) config: Option<PathBuf>,
}

pub(super) fn load_exec_config(
    db_context: &ExecDbContext,
) -> Result<(KhiveConfig, Option<PathBuf>)> {
    let db_path_for_config = config_discovery_db_anchor(db_context.raw.as_deref());
    let loaded = KhiveConfig::load_with_home_fallback_and_source(
        db_context.config.as_deref(),
        db_path_for_config.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("config error: {e}"))?;
    Ok(match loaded {
        Some((config, source)) => (config, Some(source)),
        None => (KhiveConfig::default(), None),
    })
}

pub(super) async fn run_exec_inline(
    ops: String,
    cfg: RuntimeConfig,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    db_context: ExecDbContext,
    strict: bool,
) -> Result<()> {
    #[cfg(unix)]
    return run_exec_inline_with_forward(
        ops,
        cfg,
        presentation,
        output_format,
        save_file,
        db_context,
        strict,
        forward_or_spawn_boxed,
    )
    .await;
    #[cfg(not(unix))]
    return run_exec_inline_with_forward(
        ops,
        cfg,
        presentation,
        output_format,
        save_file,
        db_context,
        strict,
    )
    .await;
}

/// Shared wait budget for recall producers and writer-task joins. Audit
/// shutdown is awaited to completion; its elapsed time consumes this budget.
const EXEC_STORAGE_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Settle a short-lived exec invocation's storage before its
/// [`KhiveMcpServer`] is dropped, so the last connection SQLite closes on
/// this database is always writable and can checkpoint the WAL away
/// (#3089). Every failure here is logged and swallowed: this never changes
/// the caller's returned result. `code_ingest.rs`'s own writer-task-join
/// drain (`settle_writer_drain`) instead fails the whole call when its
/// ingest succeeded and the drain then timed out, because that command
/// documents "return implies settled". These exec dispatch paths make no
/// such durability promise today, so this settle stays advisory: bail on a
/// stuck writer task here would be a new exec failure mode with no
/// existing contract requiring it.
///
/// Order matters, and mirrors the settle-contract test at
/// `khive-mcp/src/serve.rs` (take joins, drop the owner, await
/// the joins) plus `code_ingest.rs`'s reason for awaiting a
/// writer-task join at all before returning:
///
/// 1. After the bounded recall wait, drain the ADR-133 audit-batch supervisor
///    (`KhiveMcpServer::shutdown_audit_batch`), following the shutdown
///    order `VerbRegistry::shutdown_audit_batch` documents. The supervisor
///    is a detached task that holds a clone of the audit `EventStore`
///    while it commits already-accepted rows and exits once its queue
///    drains. Draining it here means no such clone outlives this call, so
///    dropping the server in step 3 releases every storage reference the
///    server's registry held.
/// 2. Take every pool's writer-task join BEFORE dropping the server: once
///    the server and every local clone taken here are gone, there is no
///    live reference left to reach the field through.
/// 3. Drop the server. With the supervisor's clone released in step 1 and
///    no local clones outstanding from step 2, this drops the server's own
///    `Arc<ConnectionPool>` clones. If those were a pool's last
///    references, `ConnectionPool::drop` (`crates/khive-db/src/pool.rs`)
///    runs here: it closes every read-only reader before its own writer
///    connection, so a reader is never that pool's own last closer, and
///    the writer-task's channel sender (the pool's last field to drop) is
///    what actually signals the writer task to exit.
/// 4. Await each taken join, bounded. Dropping the sender in step 3 only
///    signals the writer task, a separate `tokio::spawn`ed task owning its
///    own standalone writable connection, to exit; it still has to run and
///    close that connection on its own schedule. Awaiting it here,
///    inside this still-running executor, is what makes that close happen
///    in this sequence instead of racing Tokio runtime shutdown, which
///    drops idle tasks in random per-worker order.
///
/// Together, steps 3 and 4 mean the only two connections capable of
/// closing last on this database (a pool's own writer connection and its
/// writer task's standalone connection) are both writable; neither is ever
/// a read-only reader, so the last real closer can always take the
/// EXCLUSIVE lock SQLite needs to checkpoint when the writer joins settle.
/// A deadline expiry is advisory and does not guarantee that a writer has
/// completed. Audit shutdown is awaited without a deadline, so total elapsed
/// time can exceed the shared recall/writer wait budget.
async fn settle_exec_storage_before_return(server: KhiveMcpServer) {
    let deadline = tokio::time::Instant::now() + EXEC_STORAGE_SETTLE_TIMEOUT;
    let recall = super::recall_drain::wait_for_pending(EXEC_STORAGE_SETTLE_TIMEOUT);
    let audit_and_joins = async move {
        if let Err(reason) = server.shutdown_audit_batch().await {
            tracing::warn!(
                target: "kkernel::exec",
                reason = ?reason,
                "exec: audit-batch drain did not complete cleanly before storage settle"
            );
        }

        let mut writer_task_joins = Vec::new();
        if let Some(pool) = server.pool() {
            if let Some(join) = pool.take_writer_task_join() {
                writer_task_joins.push(join);
            }
        }
        for pool in server.secondary_pools() {
            if let Some(join) = pool.take_writer_task_join() {
                writer_task_joins.push(join);
            }
        }
        drop(server);
        writer_task_joins
    };

    settle_exec_stages(deadline, recall, audit_and_joins).await;
}

// Audit is deliberately not timed out: cancellation of accepted audit work is
// not known to preserve its durability. Its elapsed time still reduces the
// shared recall/writer budget, and it can make the total settle exceed it.
async fn settle_exec_stages<R, A, W, E>(deadline: tokio::time::Instant, recall: R, audit: A)
where
    R: std::future::Future<Output = ()>,
    A: std::future::Future<Output = Vec<W>>,
    W: std::future::Future<Output = Result<(), E>>,
    E: std::fmt::Display,
{
    if tokio::time::Instant::now() >= deadline {
        tracing::warn!(
            target: "kkernel::exec",
            stage = "recall",
            "exec: settle stage skipped because the shared deadline elapsed"
        );
    } else if tokio::time::timeout_at(deadline, recall).await.is_err() {
        tracing::warn!(
            target: "kkernel::exec",
            stage = "recall",
            "exec: settle stage did not finish before the shared deadline"
        );
    }

    let writer_task_joins = audit.await;
    for (index, join) in writer_task_joins.into_iter().enumerate() {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                target: "kkernel::exec",
                stage = "writer",
                writer_index = index,
                "exec: settle stage skipped because the shared deadline elapsed"
            );
            continue;
        }
        match tokio::time::timeout_at(deadline, join).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(
                target: "kkernel::exec",
                stage = "writer",
                writer_index = index,
                %error,
                "exec: a writer task panicked while settling storage before return"
            ),
            Err(_) => tracing::warn!(
                target: "kkernel::exec",
                stage = "writer",
                writer_index = index,
                "exec: settle stage did not finish before the shared deadline"
            ),
        }
    }
}

/// Inner implementation of `run_exec_inline`, parameterised over the daemon
/// forwarding function.  On Unix the real caller passes `forward_or_spawn_boxed`;
/// tests pass a spy to assert that the strict-actor gate fires BEFORE any
/// forwarding attempt is made.
///
/// # Why this seam exists
///
/// The daemon bypass bug (fixed in the commit preceding this one) could only be
/// regression-tested by either spawning a real daemon subprocess (fragile) or
/// injecting a spy at the forwarding boundary (deterministic).  This function
/// enables the latter: tests pass a spy `forward_fn` and assert it is never
/// called when the gate should have rejected.
#[cfg_attr(not(unix), allow(unused_variables))]
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_exec_inline_with_forward(
    ops: String,
    mut cfg: RuntimeConfig,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    mut db_context: ExecDbContext,
    strict: bool,
    #[cfg(unix)] forward_fn: ForwardFnPtr,
) -> Result<()> {
    // Keep this local preflight even though `run_exec` already performs it:
    // tests and internal callers exercise this seam directly, and no caller may
    // let an identity refusal mask malformed DSL.
    preflight_inline_ops(&ops)?;

    // ── strict-actor gate (before any forwarding) ─────────────────────────────
    // Must run BEFORE the daemon fast-path so that a comm-capable anonymous daemon
    // already running cannot be used to bypass KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1.
    // The daemon receives requests over a socket and dispatches comm verbs — the
    // same tenant-isolation risk as in-process dispatch.  Checking only in the
    // in-process fallback (as was the case before this fix) allowed a strict-mode
    // client to silently forward through a pre-existing anonymous daemon and exit 0.
    if let Err(error) = enforce_strict_actor_mode(
        cfg.actor_id.as_deref(),
        &cfg.packs,
        cfg.require_attributed_actor,
    ) {
        return Err(report_invocation_refusal(
            Some(&ops),
            RefusalReason::AnonymousActor,
            error,
        ));
    }

    // Load the resolved `KhiveConfig` ONCE, up front, so both the daemon
    // forward-frame `config_id` below and the in-process fallback's backend
    // topology (further below) resolve from the identical TOML file the
    // daemon's own boot path loads (`serve.rs`'s `build_server`:
    // `KhiveConfig::load_with_home_fallback(args.config.as_deref(),
    // config_discovery_db_anchor(args.db.as_deref()).as_deref())` —
    // `kkernel exec` threads its `--config` / `KHIVE_CONFIG` selection through
    // this reload exactly like there. The second argument is the raw
    // `--db`/`KHIVE_DB` discovery anchor (`None` unless `--db` was set) rather
    // than `cfg.db_path` — `cfg.db_path` materializes the `$HOME/.khive`
    // default when `--db` is unset (#689), which would incorrectly re-anchor
    // tier-3 discovery away from the process cwd.
    //
    // Fixes the config_id topology-drift bug: the forward frame below used to
    // always fold `None` here, while the daemon folds `Some(&khive_cfg)`
    // (`serve.rs`, `compute_config_id(default_runtime.config(),
    // Some(khive_cfg))`). On a config declaring a non-empty `[[backends]]`
    // topology (e.g. a separate `sessions` backend) the two fingerprints
    // diverged, so a correctly-configured client was rejected as a
    // `ConfigMismatch` and silently fell back to the cold in-process path on
    // every call.
    let (khive_cfg, config_source) = load_exec_config(&db_context)?;

    // #1226: apply the same --db/[[backends]] conflict guard the in-process
    // fallback below applies, BEFORE the daemon fast-path — otherwise a warm
    // daemon answers this request without the override ever being checked at
    // all, while the identical override on `--ops-file` (always in-process)
    // correctly rejects it. A matching concrete override is redundant, so its
    // fingerprint and captured construction anchor are normalized to the same
    // values used when no override is supplied.
    let force_memory = if khive_cfg.backends.is_empty() {
        false
    } else {
        let force_memory = normalize_redundant_db_override_with_source(
            &mut cfg,
            db_context.raw.as_deref(),
            &khive_cfg.backends,
            config_source.as_deref(),
        )?;
        // Declared storage replaces the construction anchor even when --db
        // was omitted. Keep the captured anchor in step with that deliberate
        // normalization before the local fallback checks for path drift.
        db_context.anchor = cfg.db_path.clone();
        force_memory
    };

    if !force_memory {
        validate_declared_backend_access_modes(&khive_cfg.backends)?;
    }
    validate_wal_ceiling_topology(&cfg, &khive_cfg.backends, force_memory)?;

    disclose_resolved_database(
        &cfg,
        &khive_cfg,
        db_context.raw.as_deref() == Some(":memory:"),
    );
    disclose_resolved_actor(&cfg);

    // ── daemon fast-path (Unix only) ─────────────────────────────────────────
    // The daemon path does not support --save-file (the daemon returns a string;
    // we would need to parse it back to apply the sink).  Skip daemon forwarding
    // when --save-file is set so the in-process path handles everything.
    //
    // The --output-format CLI flag (ADR-078 tier-1) is forwarded to the daemon as
    // the per-request `format` field so the daemon applies it at its seam.
    #[cfg(unix)]
    if save_file.is_none() {
        let frame = DaemonRequestFrame {
            ops: ops.clone(),
            presentation: presentation.clone(),
            presentation_per_op: None,
            namespace: cfg.default_namespace.as_str().to_string(),
            actor_id: cfg.actor_id.clone(),
            process_ref: khive_runtime::process_ref_from_env(),
            visible_namespaces: cfg
                .visible_namespaces
                .iter()
                .map(|ns| ns.as_str().to_string())
                .collect(),
            owned_namespaces: cfg
                .owned_namespaces
                .iter()
                .map(|ns| ns.as_str().to_string())
                .collect(),
            // Fold the SAME backends topology the daemon folds (`Some(&khive_cfg)`)
            // instead of `None` — see the `khive_cfg` load above. A force-memory
            // override also supplies the effective writable mode explicitly:
            // the declaration can say `main.read_only = true`, but the runtime
            // the child opens is writable memory and fingerprints that captured
            // mode after construction.
            config_id: if force_memory {
                compute_config_id_with_storage_mode(&cfg, Some(&khive_cfg), false)
            } else {
                compute_config_id(&cfg, Some(&khive_cfg))
            },
            protocol_version: PROTOCOL_VERSION,
            plan: false,
            probe_only: false,
            metrics_only: false,
            format: output_format.clone(),
            format_per_op: None,
            // `kkernel exec` is a trusted operator surface: subhandler verbs are
            // allowed. Only the agent-facing MCP `request` tool sets this true.
            from_wire: false,
            request_id: None,
        };
        // Which override a daemon this call may need to SPAWN must be
        // constructed with (the spawn seam forwards whatever it receives):
        // - `:memory:` always: the child must stay ephemeral like the client.
        // - A concrete override in the SINGLE-backend case (no `[[backends]]`
        //   declared above): the fresh daemon has no config-declared database
        //   path, so without the override it would bind `$HOME/.khive/khive.db`
        //   and its `config_id` would never match this override-anchored frame.
        // - A concrete override here in the MULTI-backend case is, by
        //   construction, the redundant-main one proven and normalized above —
        //   withhold it: both sides use the config-declared main path and the
        //   override adds no storage selection information.
        // Which config file a daemon this call may need to SPAWN must be
        // constructed with:
        // - An explicit `--config`/`KHIVE_CONFIG` selection always: it is the
        //   operator's choice and the frame already folds its topology.
        // - Otherwise, in exactly the redundant-multi-backend case withheld
        //   above: the config that declared the backend topology was
        //   DISCOVERED (retained in `config_source` — e.g. via the db-dir
        //   tier-3 anchor of `KhiveConfig::load_with_home_fallback_and_source`),
        //   and the withheld override was the child's only other clue about
        //   which database to bind. Without forwarding the resolved path as
        //   the child's explicit `--config`, the spawned daemon re-discovers
        //   from its own cwd/HOME, fails to reach a config anchored only
        //   beside the database, binds `$HOME/.khive/khive.db`, and squats
        //   the socket with a `config_id` that never matches this frame.
        //   Forwarding the retained path makes the child fold the identical
        //   topology, so its fingerprint matches.
        // - Otherwise nothing: the empty-backends child gets its database
        //   directly via the forwarded concrete override above.
        let (spawn_config, spawn_db) = khive_mcp::daemon::select_daemon_spawn_overrides(
            db_context.config.as_deref(),
            db_context.raw.as_deref(),
            config_source.as_deref(),
            !khive_cfg.backends.is_empty(),
        );
        // Forward this client's already-resolved pack list unconditionally so
        // a daemon this call spawns matches `cfg`'s `config_id` fingerprint
        // (which folds `packs`) instead of re-deriving its own selection from
        // ambient env/config and risking a mismatch (khive-oss#1941).
        let spawn_packs = cfg.packs.clone();
        if let Some(res) = forward_fn(&frame, spawn_config, spawn_db.as_deref(), spawn_packs).await
        {
            let output = res.map_err(|e| anyhow::anyhow!("{}", e.message))?;
            disclose_daemon_execution();
            let output = prepare_exec_output(&output, strict);
            println!("{output}");
            enforce_strict_batch_result(&output, strict)?;
            return Ok(());
        }
    }

    // ── in-process fallback ───────────────────────────────────────────────────
    // Note: enforce_strict_actor_mode was called above before the daemon fast-path;
    // it is not repeated here — the single early check covers both paths.
    //
    // `build_local_fallback_server` resolves the ADR-078 §2 output-format
    // precedence chain (env var over TOML `[runtime] default_output_format`
    // over builtin json) AND honors `[[backends]]` multi-backend topology —
    // see its doc comment.
    let server = build_local_fallback_server(
        cfg,
        &khive_cfg,
        db_context.raw.as_deref(),
        db_context.anchor.as_deref(),
    )
    .await?;

    let params = RequestParams {
        plan: None,
        ops,
        presentation,
        presentation_per_op: None,
        save_to: save_file,
        // Tier-1: CLI --output-format overrides the server default (env/builtin).
        format: output_format,
        format_per_op: None,
        request_id: None,
    };

    let dispatch_result = server
        .dispatch_request_local_for_exec(params, strict)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"));

    // Every branch below reaches the same tail: settle storage before this
    // function (and, for a daemonless invocation, the process) returns,
    // regardless of whether dispatch or the strict-batch check failed.
    let final_result = match dispatch_result {
        Ok(raw_output) => {
            let output = prepare_exec_output(&raw_output, strict);
            println!("{output}");
            enforce_strict_batch_result(&output, strict)
        }
        Err(error) => Err(error),
    };
    settle_exec_storage_before_return(server).await;
    final_result
}

/// Build the server used whenever `kkernel exec` dispatches a request locally
/// instead of through the warm daemon (both the fallback and `--ops-file`
/// bulk-apply paths). See
/// `crates/kkernel/docs/design.md#exec-local-dispatch-fallback-server-adr-067-adr-028-8`
/// for why this must agree with the daemon's own multi-backend boot logic.
pub(super) async fn build_local_fallback_server(
    cfg: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    db_anchor: Option<&std::path::Path>,
) -> Result<KhiveMcpServer> {
    // Held across the real-host async schema/application coordinator and pack
    // assembly; dropped only after the fully wired server is ready.
    let _boot_guard = acquire_local_construction_guard(&cfg)?;
    if khive_cfg.backends.is_empty() {
        let rt = build_single_backend_runtime(cfg, khive_cfg).await?;
        let env_fmt = apply_env_output_format(khive_cfg.runtime.default_output_format);
        Ok(
            KhiveMcpServer::new_with_mounts_and_config(rt, Some(khive_cfg))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .with_default_output_format(env_fmt),
        )
    } else {
        build_server_multi_backend_with_db_anchor(cfg, khive_cfg, cli_db_override, db_anchor).await
    }
}

pub(super) struct AtomicSavePublishFailure {
    pub(super) stdout: String,
    pub(super) error: anyhow::Error,
}

/// Render the authoritative atomic stdout value. A save sink is preflighted
/// before execution, but its writes/flush/rename necessarily happen after the
/// database outcome is known. If that publication fails after a commit, keep
/// the process failure while returning a reconciliation envelope that makes
/// the durable, non-retryable outcome explicit.
pub(super) fn render_atomic_output(
    envelope: &mut serde_json::Value,
    save_sink: Option<khive_mcp::save_sink::JsonlSaveSink>,
) -> std::result::Result<String, AtomicSavePublishFailure> {
    let Some(save_sink) = save_sink else {
        return Ok(serde_json::to_string_pretty(envelope).expect("serialize atomic envelope"));
    };

    match save_sink.write_envelope(envelope) {
        Ok(manifest) => {
            Ok(serde_json::to_string(&manifest).expect("serialize atomic save manifest"))
        }
        Err(error) => {
            let committed = crate::atomic_apply::record_save_file_publish_failure(envelope, &error);
            let stdout = serde_json::to_string_pretty(envelope)
                .expect("serialize atomic save failure reconciliation envelope");
            let error = if committed {
                error.context(
                    "atomic database changes committed but --save-file publication failed; \
                     do not replay the mutation (inspect stdout for reconciliation details)",
                )
            } else {
                error.context("atomic --save-file publication failed")
            };
            Err(AtomicSavePublishFailure { stdout, error })
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_exec_ops_file(
    path: PathBuf,
    cfg: RuntimeConfig,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    dry_run: bool,
    db_context: ExecDbContext,
    serial: bool,
    atomic: bool,
    atomic_max_ops: Option<usize>,
    strict: bool,
) -> Result<()> {
    if serial && atomic {
        anyhow::bail!("--serial conflicts with --atomic");
    }

    // Validate the whole file and spool a stable bounded snapshot before any
    // runtime construction or writes. Non-atomic dispatch retains only one
    // request chunk plus ordered result envelopes in memory.
    let mut validated = match validate_ops_file(&path) {
        Ok(validated) => validated,
        Err(error) => {
            if let Some(refusal) = error.downcast_ref::<ExecRefusal>() {
                return Err(report_unscoped_refusal(
                    refusal.reason,
                    refusal.message.as_str(),
                ));
            }
            return Err(error);
        }
    };

    if validated.total == 0 {
        anyhow::bail!("ops-file is empty (no non-blank lines): {}", path.display());
    }

    if serial {
        if let Err(error) =
            preflight_typed_validated_snapshot(&mut validated.snapshot, validated.total)
        {
            if let Some(refusal) = error.downcast_ref::<ExecRefusal>() {
                return Err(report_unscoped_refusal(
                    refusal.reason,
                    refusal.message.as_str(),
                ));
            }
            return Err(error);
        }
    }

    if dry_run && !atomic {
        let summary = serde_json::json!({
            "dry_run": true,
            "total": validated.total,
            "per_verb": validated.per_verb,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).expect("serialize dry-run summary")
        );
        return Ok(());
    }

    // Build the in-process runtime (daemon fast-path is intentionally skipped
    // for bulk apply — bulk throughput benefits from a single warm runtime, not
    // the round-trip overhead of socket forwarding per chunk). Honors
    // `[[backends]]` multi-backend topology exactly like the daemon-fallback
    // path — see `build_local_fallback_server`.
    if let Err(error) = enforce_strict_actor_mode(
        cfg.actor_id.as_deref(),
        &cfg.packs,
        cfg.require_attributed_actor,
    ) {
        let tools = validated_tool_names(&mut validated.snapshot)?;
        return Err(report_tools_refusal(
            tools,
            false,
            RefusalReason::AnonymousActor,
            error.to_string(),
        ));
    }
    let (khive_cfg, config_source) = load_exec_config(&db_context)?;

    if !khive_cfg.backends.is_empty() {
        // Preserve the selected config path on a refusal, but leave accepted
        // cases to their downstream owner: the non-atomic shared builder logs
        // and normalizes them once, while `--atomic` rejects multi-backend
        // topology before opening storage.
        reject_conflicting_db_override_with_source(
            db_context.raw.as_deref(),
            &khive_cfg.backends,
            config_source.as_deref(),
        )?;
    }

    disclose_resolved_database(
        &cfg,
        &khive_cfg,
        db_context.raw.as_deref() == Some(":memory:"),
    );
    disclose_resolved_actor(&cfg);

    if atomic {
        let max_ops = atomic_max_ops.unwrap_or(khive_types::pack::ATOMIC_MAX_OPS_DEFAULT);
        let ops =
            parse_atomic_validated_snapshot(&mut validated.snapshot, validated.total, max_ops)?;
        if dry_run {
            if let Err(error) =
                crate::atomic_apply::preflight_atomic_ops_file(&ops, &cfg, &khive_cfg, max_ops)
            {
                if let Some(failure) =
                    error.downcast_ref::<crate::atomic_apply::AtomicExecFailure>()
                {
                    let mut envelope = failure.envelope();
                    annotate_and_emit_refusals(&mut envelope, strict);
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&envelope)
                            .expect("serialize atomic dry-run refusal envelope")
                    );
                }
                return Err(error);
            }
            let summary = serde_json::json!({
                "dry_run": true,
                "atomic": true,
                "total": validated.total,
                "per_verb": validated.per_verb,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&summary).expect("serialize atomic dry-run summary")
            );
            return Ok(());
        }
        // Preflight a deterministic save target before the atomic unit can
        // commit. An execution error drops the unfinished sibling temp file
        // and leaves any prior complete destination untouched.
        let save_sink = save_file
            .as_deref()
            .map(|path| khive_mcp::save_sink::JsonlSaveSink::new(Path::new(path), false))
            .transpose()?;
        let mut envelope =
            match crate::atomic_apply::execute_atomic_ops_file(ops, cfg, &khive_cfg, max_ops).await
            {
                Ok(envelope) => envelope,
                Err(error) => {
                    if let Some(failure) =
                        error.downcast_ref::<crate::atomic_apply::AtomicExecFailure>()
                    {
                        let mut envelope = failure.envelope();
                        annotate_and_emit_refusals(&mut envelope, strict);
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&envelope)
                                .expect("serialize atomic refusal envelope")
                        );
                    }
                    return Err(error);
                }
            };
        annotate_and_emit_refusals(&mut envelope, strict);
        let output = match render_atomic_output(&mut envelope, save_sink) {
            Ok(output) => output,
            Err(failure) => {
                // stdout is the machine reconciliation channel. Emit it before
                // returning the non-zero sink error so callers never infer that
                // silence means the atomic database unit is safe to replay.
                println!("{}", failure.stdout);
                return Err(failure.error);
            }
        };
        println!("{output}");
        if envelope["atomic"]["rolled_back"].as_bool() == Some(true) {
            anyhow::bail!("atomic unit rolled back; inspect stdout for the failed operation");
        }
        return Ok(());
    }

    let server = build_local_fallback_server(
        cfg,
        &khive_cfg,
        db_context.raw.as_deref(),
        db_context.anchor.as_deref(),
    )
    .await?;

    validated
        .snapshot
        .rewind()
        .context("rewind validated ops-file snapshot for dispatch")?;
    let dispatch_result = apply_ops_file_reader_with_dispatch_mode(
        &server,
        std::io::BufReader::new(validated.snapshot),
        validated.total,
        presentation,
        output_format,
        save_file,
        strict,
        if serial {
            OpsFileDispatchMode::Serial
        } else {
            OpsFileDispatchMode::BoundedParallel
        },
    )
    .await
    .map(|_| ());

    // Settle storage before this function (and, for a daemonless
    // invocation, the process) returns, regardless of dispatch outcome.
    settle_exec_storage_before_return(server).await;
    dispatch_result
}

#[cfg(test)]
mod settle_exec_stages_tests {
    use std::future::poll_fn;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    use std::task::Poll;
    use std::time::Duration;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    use super::settle_exec_stages;

    #[derive(Clone, Default)]
    struct Warnings(Arc<Mutex<Vec<Warning>>>);

    #[derive(Debug, Default)]
    struct Warning {
        stage: String,
        writer_index: Option<u64>,
        message: String,
    }

    impl Visit for Warning {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "stage" {
                self.stage = value.to_owned();
            }
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            if field.name() == "writer_index" {
                self.writer_index = Some(value);
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = format!("{value:?}");
            }
        }
    }

    impl Subscriber for Warnings {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() == &tracing::Level::WARN && metadata.target() == "kkernel::exec"
        }

        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut warning = Warning::default();
            event.record(&mut warning);
            self.0.lock().unwrap().push(warning);
        }
    }

    fn blocked_writer(
        entered: Arc<AtomicBool>,
    ) -> impl std::future::Future<Output = Result<(), &'static str>> {
        poll_fn(move |_| {
            entered.store(true, Ordering::SeqCst);
            Poll::<Result<(), &'static str>>::Pending
        })
    }

    // Expected red control: resetting the timeout for the writer after recall
    // would poll the blocked writer and extend the elapsed time past the margin.
    #[tokio::test(flavor = "current_thread")]
    async fn recall_exhausts_budget_and_writer_is_skipped() {
        let recall_entered = Arc::new(AtomicBool::new(false));
        let recall_flag = Arc::clone(&recall_entered);
        let recall = poll_fn(move |_| {
            recall_flag.store(true, Ordering::SeqCst);
            Poll::<()>::Pending
        });
        let writer_entered = Arc::new(AtomicBool::new(false));
        let writer_flag = Arc::clone(&writer_entered);
        let writer = poll_fn(move |_| {
            writer_flag.store(true, Ordering::SeqCst);
            Poll::<Result<(), &'static str>>::Pending
        });
        let audit_completed = Arc::new(AtomicBool::new(false));
        let audit_flag = Arc::clone(&audit_completed);
        let audit = async move {
            audit_flag.store(true, Ordering::SeqCst);
            vec![writer]
        };
        let budget = Duration::from_secs(2);
        let margin = Duration::from_secs(1);
        let start = tokio::time::Instant::now();
        settle_exec_stages(start + budget, recall, audit).await;
        assert!(recall_entered.load(Ordering::SeqCst));
        assert!(audit_completed.load(Ordering::SeqCst));
        assert!(!writer_entered.load(Ordering::SeqCst));
        assert!(start.elapsed() >= budget);
        assert!(start.elapsed() < budget + margin);
    }

    // Expected red control: resetting the writer timeout after a slow recall
    // takes about 6.5 seconds, beyond the shared 4-second budget plus 1.5-second
    // margin. The wall-clock margin remains sensitive to an overloaded host.
    #[tokio::test(flavor = "current_thread")]
    async fn recall_and_writer_both_consume_the_shared_budget() {
        let recall_entered = Arc::new(AtomicBool::new(false));
        let recall_flag = Arc::clone(&recall_entered);
        let recall = async move {
            recall_flag.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(2500)).await;
        };
        let writer_entered = Arc::new(AtomicBool::new(false));
        let writer = blocked_writer(Arc::clone(&writer_entered));
        let budget = Duration::from_secs(4);
        let margin = Duration::from_millis(1500);
        let start = tokio::time::Instant::now();
        settle_exec_stages(start + budget, recall, async { vec![writer] }).await;
        assert!(recall_entered.load(Ordering::SeqCst));
        assert!(writer_entered.load(Ordering::SeqCst));
        assert!(start.elapsed() >= budget);
        assert!(start.elapsed() < budget + margin);
    }

    // Expected red control: removing a warning or awaiting a skipped future
    // breaks the exact stage/index and polling assertions below.
    #[tokio::test(flavor = "current_thread")]
    async fn expired_stages_warn_once_and_are_not_polled() {
        let warnings = Warnings::default();
        let captured = Arc::clone(&warnings.0);
        let _subscriber = tracing::subscriber::set_default(warnings);
        let recall_entered = Arc::new(AtomicBool::new(false));
        let recall_flag = Arc::clone(&recall_entered);
        let recall = poll_fn(move |_| {
            recall_flag.store(true, Ordering::SeqCst);
            Poll::<()>::Pending
        });
        let first_entered = Arc::new(AtomicBool::new(false));
        let second_entered = Arc::new(AtomicBool::new(false));
        let first = blocked_writer(Arc::clone(&first_entered));
        let second = blocked_writer(Arc::clone(&second_entered));
        settle_exec_stages(
            tokio::time::Instant::now() - Duration::from_millis(1),
            recall,
            async { vec![first, second] },
        )
        .await;
        assert!(!recall_entered.load(Ordering::SeqCst));
        assert!(!first_entered.load(Ordering::SeqCst));
        assert!(!second_entered.load(Ordering::SeqCst));
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[0].stage, "recall");
        assert_eq!(captured[0].writer_index, None);
        assert!(captured[0].message.contains("skipped"));
        for (warning, index) in captured[1..].iter().zip([0, 1]) {
            assert_eq!(warning.stage, "writer");
            assert_eq!(warning.writer_index, Some(index));
            assert!(warning.message.contains("skipped"));
        }
    }

    // Positive control: one blocked writer receives the entire remaining budget.
    #[tokio::test(flavor = "current_thread")]
    async fn one_stalled_writer_uses_its_remaining_budget() {
        let writer_entered = Arc::new(AtomicBool::new(false));
        let writer_flag = Arc::clone(&writer_entered);
        let writer = poll_fn(move |_| {
            writer_flag.store(true, Ordering::SeqCst);
            Poll::<Result<(), &'static str>>::Pending
        });
        let budget = Duration::from_secs(2);
        let margin = Duration::from_secs(1);
        let start = tokio::time::Instant::now();
        settle_exec_stages(start + budget, async {}, async { vec![writer] }).await;
        assert!(writer_entered.load(Ordering::SeqCst));
        assert!(start.elapsed() >= budget);
        assert!(start.elapsed() < budget + margin);
    }

    // Expected red control: timing out or skipping audit would leave its flag
    // unset; starting the writer after expiry would set the writer flag.
    #[tokio::test(flavor = "current_thread")]
    async fn audit_finishes_even_after_the_deadline() {
        let audit_completed = Arc::new(AtomicBool::new(false));
        let audit_flag = Arc::clone(&audit_completed);
        let writer_entered = Arc::new(AtomicBool::new(false));
        let writer_flag = Arc::clone(&writer_entered);
        let writer = poll_fn(move |_| {
            writer_flag.store(true, Ordering::SeqCst);
            Poll::<Result<(), &'static str>>::Pending
        });
        let start = tokio::time::Instant::now();
        let audit = async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            audit_flag.store(true, Ordering::SeqCst);
            vec![writer]
        };
        settle_exec_stages(start + Duration::from_millis(100), async {}, audit).await;
        assert!(audit_completed.load(Ordering::SeqCst));
        assert!(!writer_entered.load(Ordering::SeqCst));
        assert!(start.elapsed() >= Duration::from_millis(150));
    }
}

#[cfg(test)]
#[path = "actor_pin_tests.rs"]
mod actor_pin_tests;
