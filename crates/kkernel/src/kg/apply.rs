use anyhow::{bail, Context, Result};
use khive_changeset::Op;
use khive_rule_evaluator::{classify_changeset, Tier, TierFinding};

use super::types::{ApplyArgs, OutputFormat};
use super::{commit, review, validate};

pub(super) async fn cmd_apply(args: ApplyArgs) -> Result<()> {
    let delta = std::fs::read_to_string(&args.changeset)
        .with_context(|| format!("reading change-set {}", args.changeset.display()))?;
    let changeset = khive_changeset::from_ndjson(&delta).context("parsing canonical change-set")?;
    let max = khive_types::pack::ATOMIC_MAX_OPS_DEFAULT;
    if changeset.ops.len() > max {
        bail!(
            "change-set has {} operations; maximum is {max}",
            changeset.ops.len()
        );
    }
    if let Some(index) = changeset
        .ops
        .iter()
        .position(|op| matches!(op, Op::Merge(_)))
    {
        bail!("operation {index}: merge is KnownUnimplemented for canonical apply");
    }
    if let Some(op_index) = changeset
        .ops
        .iter()
        .position(|op| matches!(op, Op::Restore(_)))
    {
        return Err(khive_runtime::RuntimeError::CanonicalUnsupportedOperation {
            op_index,
            operation: "restore",
        }
        .into());
    }
    let rules = validate::load_rules(&args.rules).context("loading apply rules")?;
    let rule_results = commit::run_commit_time_rules_with_rules(&changeset, &rules)
        .context("running commit-time rule pass")?;
    require_tier_one(&changeset, &rules, &rule_results)?;
    let output = submit_delta(&args, &delta).await?;
    let value = successful_apply_result(&output)?;
    match args.format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&value)?),
        OutputFormat::Text | OutputFormat::Github => {
            println!("Committed {} operations", changeset.ops.len())
        }
    }
    Ok(())
}

fn require_tier_one(
    changeset: &khive_changeset::ChangeSet,
    rules: &khive_rule_evaluator::Rules,
    rule_results: &[super::types::RuleResult],
) -> Result<()> {
    let findings = review::classification_findings(changeset, rule_results);
    let tier_findings: Vec<_> = findings
        .iter()
        .map(|finding| TierFinding {
            severity: &finding.severity,
            subject_id: finding.subject_id.as_deref(),
        })
        .collect();
    for (index, decision) in classify_changeset(rules, changeset, &tier_findings)
        .iter()
        .enumerate()
    {
        if decision.tier != Tier::Tier1 {
            bail!("operation {index} requires Tier 2: {}", decision.reason);
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn submit_delta(args: &ApplyArgs, delta: &str) -> Result<String> {
    use khive_mcp::serve::{
        config_discovery_db_anchor, enforce_strict_actor_mode,
        normalize_redundant_db_override_with_source, resolve_runtime_config_with_db_anchor,
        validate_declared_backend_access_modes, validate_wal_ceiling_topology, RuntimeConfigInputs,
    };
    use khive_runtime::{DaemonRequestFrame, KhiveConfig, Namespace};

    let (mut config, _) = resolve_runtime_config_with_db_anchor(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: args.config.as_deref(),
        namespace: Namespace::parse(&args.namespace).context("invalid namespace")?,
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: (!args.packs.is_empty()).then(|| args.packs.clone()),
        brain_profile: None,
    })?;
    enforce_strict_actor_mode(config.actor_id.as_deref(), &config.packs)?;
    let discovery_anchor = config_discovery_db_anchor(args.db.as_deref());
    let loaded = KhiveConfig::load_with_home_fallback_and_source(
        args.config.as_deref(),
        discovery_anchor.as_deref(),
    )?;
    let (khive_config, source) = loaded.map_or_else(
        || (KhiveConfig::default(), None),
        |(config, source)| (config, Some(source)),
    );
    let force_memory = if khive_config.backends.is_empty() {
        false
    } else {
        normalize_redundant_db_override_with_source(
            &mut config,
            args.db.as_deref(),
            &khive_config.backends,
            source.as_deref(),
        )?
    };
    if !force_memory {
        validate_declared_backend_access_modes(&khive_config.backends)?;
    }
    validate_wal_ceiling_topology(&config, &khive_config.backends, force_memory)?;
    let config_id = if force_memory {
        khive_mcp::server::compute_config_id_with_storage_mode(&config, Some(&khive_config), false)
    } else {
        khive_mcp::server::compute_config_id(&config, Some(&khive_config))
    };
    let frame = DaemonRequestFrame {
        ops: serde_json::to_string(&vec![
            serde_json::json!({"tool":"apply_changeset","args":{"delta":delta}}),
        ])?,
        namespace: config.default_namespace.as_str().to_owned(),
        actor_id: config.actor_id.clone(),
        process_ref: khive_runtime::process_ref_from_env(),
        visible_namespaces: config
            .visible_namespaces
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect(),
        config_id,
        protocol_version: khive_runtime::daemon::PROTOCOL_VERSION,
        presentation: Some("verbose".into()),
        format: Some("json".into()),
        from_wire: true,
        ..Default::default()
    };
    let spawn_db = match args.db.as_deref() {
        Some(":memory:") => Some(":memory:"),
        Some(value) if khive_config.backends.is_empty() => Some(value),
        _ => None,
    };
    let spawn_config = args.config.as_deref().or(source.as_deref());
    match khive_mcp::daemon::forward_or_spawn_with_config_and_packs(
        &frame,
        spawn_config,
        spawn_db,
        Some(&config.packs),
    )
    .await
    {
        Some(Ok(output)) => Ok(output),
        Some(Err(error)) => bail!(
            "daemon apply failed; do not replay an uncertain write: {}",
            error.message
        ),
        None => bail!("canonical apply requires the daemon; local dispatch is unavailable"),
    }
}

#[cfg(not(unix))]
async fn submit_delta(_args: &ApplyArgs, _delta: &str) -> Result<String> {
    bail!("canonical apply requires the Unix daemon transport")
}

fn successful_apply_result(output: &str) -> Result<serde_json::Value> {
    let envelope: serde_json::Value = serde_json::from_str(output)
        .context("decoding daemon apply response; write outcome may be unknown")?;
    let results = envelope
        .get("results")
        .and_then(serde_json::Value::as_array)
        .context("daemon apply response has no operation results; do not replay")?;
    if results.len() != 1 {
        bail!(
            "daemon apply response has {} results, expected one; do not replay",
            results.len()
        );
    }
    if results[0].get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        bail!("canonical apply refused: {}", results[0]);
    }
    let result = results[0]
        .get("result")
        .context("daemon apply response lacks its result; do not replay")?;
    if result.get("committed").and_then(serde_json::Value::as_bool) != Some(true) {
        bail!("canonical apply did not confirm commit; do not replay: {result}");
    }
    if result
        .get("reconciliation_required")
        .and_then(serde_json::Value::as_bool)
        != Some(false)
    {
        bail!("canonical apply committed and requires reconciliation; do not replay: {result}");
    }
    Ok(result.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::{json, Value};

    fn changeset(ops: &[Value]) -> khive_changeset::ChangeSet {
        let envelope = json!({"schema_version":khive_changeset::CURRENT_SCHEMA_VERSION,"producer":"fixture",
            "producer_model_family":"fixture-family","staged_at":1_000_000});
        let mut delta = format!("{envelope}\n");
        for op in ops {
            delta.push_str(&format!("{op}\n"));
        }
        khive_changeset::from_ndjson(&delta).unwrap()
    }

    #[test]
    fn apply_requires_rules_and_has_no_chunk_or_local_escape_option() {
        assert!(ApplyArgs::try_parse_from(["apply", "delta.ndjson"]).is_err());
        let args = ApplyArgs::try_parse_from([
            "apply",
            "delta.ndjson",
            "--rules",
            "rules.toml",
            "--namespace",
            "local",
        ])
        .unwrap();
        assert_eq!(args.changeset.to_str(), Some("delta.ndjson"));
        for extra in ["--chunk-size", "--max-ops", "--local"] {
            assert!(ApplyArgs::try_parse_from([
                "apply",
                "delta.ndjson",
                "--rules",
                "rules.toml",
                extra,
            ])
            .is_err());
        }
    }

    #[test]
    fn apply_cannot_drop_partial_view_coverage_findings() {
        let delta = changeset(&[json!({"op":"update",
            "target_id":"00000000-0000-4000-8000-000000000001",
            "patch":{"target":"entity","description":"after"},
            "preimage":{"target":"entity","description":"before"}})]);
        let rules = khive_rule_evaluator::Rules::parse_toml("").unwrap();
        assert!(require_tier_one(&delta, &rules, &[]).is_err());
    }

    #[test]
    fn one_tier_two_operation_refuses_the_whole_batch() {
        let delta = changeset(&[
            json!({"op":"create","id":"00000000-0000-4000-8000-000000000001",
                "namespace":"local","target":{"kind":"entity","entity_kind":"concept","name":"additive"}}),
            json!({"op":"link","id":"00000000-0000-4000-8000-000000000002",
                "namespace":"local","source":"00000000-0000-4000-8000-000000000001",
                "target":"00000000-0000-4000-8000-000000000003","relation":"extends",
                "weight":0.69,"properties":{}}),
        ]);
        let rules = khive_rule_evaluator::Rules::parse_toml("").unwrap();
        assert!(require_tier_one(&delta, &rules, &[]).is_err());
        assert!(require_tier_one(
            &changeset(&[serde_json::to_value(&delta.ops[0]).unwrap()]),
            &rules,
            &[]
        )
        .is_ok());
    }

    #[test]
    fn daemon_commit_requires_one_explicit_settled_receipt() {
        let committed = json!({"committed":true,"reconciliation_required":false});
        let output = json!({"results":[{"ok":true,"result":committed}]}).to_string();
        assert_eq!(successful_apply_result(&output).unwrap(), committed);
        for output in [
            json!({}),
            json!({"results":[]}),
            json!({"results":[{"ok":false,"error":"refused"}]}),
            json!({"results":[{"ok":true,"result":{"committed":false}}]}),
            json!({"results":[{"ok":true,"result":{"committed":true,"reconciliation_required":true}}]}),
            json!({"results":[{"ok":true,"result":{"committed":true}}]}),
        ] {
            assert!(
                successful_apply_result(&output.to_string()).is_err(),
                "{output}"
            );
        }
        assert!(successful_apply_result("{truncated").is_err());
    }
}
