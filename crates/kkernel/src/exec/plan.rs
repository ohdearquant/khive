use anyhow::{Context, Result};
use serde_json::Value;

use super::ExecArgs;

fn validate_args(args: &ExecArgs) -> Result<&str> {
    for (present, flag) in [
        (args.presentation.is_some(), "--presentation"),
        (args.strict, "--strict"),
        (args.output_format.is_some(), "--output-format"),
        (args.save_file.is_some(), "--save-file"),
        (args.ops_file.is_some(), "--ops-file"),
        (args.pending_events, "--pending-events"),
        (args.dry_run, "--dry-run"),
        (args.serial, "--serial"),
        (args.atomic, "--atomic"),
        (args.atomic_max_ops.is_some(), "--atomic-max-ops"),
        (args.verbose, "--verbose"),
        (args.actor.is_some(), "--actor"),
        (args.expect_actor.is_some(), "--expect-actor"),
        (args.namespace != "local", "--namespace"),
    ] {
        anyhow::ensure!(!present, "--plan cannot be used with {flag}");
    }
    args.ops
        .as_deref()
        .context("--plan requires a positional operations string")
}

#[cfg(unix)]
pub(super) async fn run(args: &ExecArgs) -> Result<Value> {
    use khive_mcp::serve::{
        normalize_redundant_db_override_with_source, resolve_runtime_config_with_db_anchor,
        validate_declared_backend_access_modes, RuntimeConfigInputs,
    };
    use khive_mcp::server::{compute_config_id, compute_config_id_with_storage_mode};
    use khive_runtime::daemon::{
        read_frame, socket_path, write_frame, DaemonResponseFrame, PROTOCOL_VERSION,
    };
    use khive_runtime::Namespace;
    use tokio::net::UnixStream;

    // A pre-plan protocol can discard the flag and execute the operations.
    const { assert!(PROTOCOL_VERSION >= 5) };

    let ops = validate_args(args)?;
    let (mut cfg, anchor) = resolve_runtime_config_with_db_anchor(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: args.config.as_deref(),
        namespace: Namespace::local(),
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: false,
        packs: None,
        brain_profile: None,
    })?;
    let (khive_cfg, source) = super::load_exec_config(&super::ExecDbContext {
        raw: args.db.clone(),
        anchor,
        config: args.config.clone(),
    })?;
    let force_memory = if khive_cfg.backends.is_empty() {
        false
    } else {
        normalize_redundant_db_override_with_source(
            &mut cfg,
            args.db.as_deref(),
            &khive_cfg.backends,
            source.as_deref(),
        )?
    };
    if !force_memory {
        validate_declared_backend_access_modes(&khive_cfg.backends)?;
    }
    let config_id = if force_memory {
        compute_config_id_with_storage_mode(&cfg, Some(&khive_cfg), false)
    } else {
        compute_config_id(&cfg, Some(&khive_cfg))
    };
    let frame = serde_json::json!({
        "ops": ops,
        "plan": true,
        "config_id": config_id,
        "protocol_version": PROTOCOL_VERSION,
        "namespace": "",
    });
    let response = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let mut stream = UnixStream::connect(socket_path()).await.context(
            "--plan requires an already-running daemon; start one with matching configuration",
        )?;
        write_frame(&mut stream, &serde_json::to_vec(&frame)?)
            .await
            .context("send plan request to daemon")?;
        let payload = read_frame(&mut stream)
            .await
            .context("read daemon plan response")?;
        serde_json::from_slice::<DaemonResponseFrame>(&payload)
            .context("decode daemon plan response")
    })
    .await
    .context("timed out waiting for daemon plan response")??;

    anyhow::ensure!(
        !response.version_mismatch && response.daemon_protocol_version == PROTOCOL_VERSION,
        "version_mismatch: plan client protocol {PROTOCOL_VERSION}, daemon protocol {}; restart the daemon with a matching version",
        response.daemon_protocol_version
    );
    anyhow::ensure!(
        !response.config_mismatch
            && !response.namespace_mismatch
            && response.served_config_id.as_deref() == Some(config_id.as_str()),
        "plan daemon configuration mismatch; start a daemon with matching --db and --config settings"
    );
    anyhow::ensure!(
        response.ok,
        "{}",
        response
            .error
            .as_deref()
            .unwrap_or("daemon refused the plan request")
    );
    let result = response
        .result
        .context("daemon plan response is missing its result")?;
    serde_json::from_str(&result).context("daemon returned invalid plan JSON")
}

#[cfg(not(unix))]
pub(super) async fn run(args: &ExecArgs) -> Result<Value> {
    validate_args(args)?;
    anyhow::bail!("--plan requires a platform with Unix daemon sockets")
}
