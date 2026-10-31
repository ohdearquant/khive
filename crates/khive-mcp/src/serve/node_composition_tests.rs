//! Composition tests for `prepare_node_channel`'s gating (C2).

use std::sync::Arc;

use clap::Parser;
use khive_runtime::node_config::NodeChannelConfig;
use khive_runtime::{KhiveRuntime, RuntimeConfig};

use super::node::{prepare_node_channel, require_comm_pack, COMM_PACK_REQUIRED};
use crate::args::Args;
use crate::server::KhiveMcpServer;

#[test]
fn enabled_without_comm_pack_refuses_with_exact_text() {
    // Kills: dropping the comm-pack check, or changing the refusal text.
    let err = require_comm_pack(true, ["kg", "gtd"]).unwrap_err();
    assert_eq!(
        err.to_string(),
        "channel.node: enabled channel requires the comm pack"
    );
    assert_eq!(COMM_PACK_REQUIRED, err.to_string());
}

#[test]
fn enabled_with_no_packs_refuses() {
    // Kills: treating an empty pack selection as satisfied.
    assert!(require_comm_pack(true, Vec::<String>::new()).is_err());
}

#[test]
fn enabled_with_comm_pack_passes() {
    // Kills: refusing unconditionally when enabled.
    assert!(require_comm_pack(true, ["kg", "comm"]).is_ok());
}

#[test]
fn disabled_without_comm_pack_prepares_nothing() {
    // Kills: refusing when the node channel is disabled or absent.
    assert!(require_comm_pack(false, ["kg"]).is_ok());
    assert!(require_comm_pack(false, Vec::<String>::new()).is_ok());
}

#[test]
fn pack_name_match_is_exact() {
    // Kills: a substring or prefix match on the pack name.
    assert!(require_comm_pack(true, ["comms", "xcomm"]).is_err());
}

// Enabled, valid node table. Copied from the known-valid fixture at
// `crates/khive-runtime/src/node_config_tests.rs:33` (also
// `crates/khive-mcp/src/server/node_config_id_tests.rs:21`); both are private to their
// modules, so this file carries its own copy.
fn enabled_table() -> NodeChannelConfig {
    NodeChannelConfig {
        enabled: true,
        slug: Some("north-1".to_string()),
        namespace: "local".to_string(),
        actor: Some("node-actor".to_string()),
        realm: Some("realm.one".to_string()),
        service_url: Some("https://channel.example.com".to_string()),
        agent_id: Some("11111111-1111-4111-8111-111111111111".to_string()),
        device_id: Some("22222222-2222-4222-8222-222222222222".to_string()),
        key_epoch: Some(1),
        key_reference: Some("node-key.1".to_string()),
        key_fingerprint: Some("d".repeat(64)),
        ..NodeChannelConfig::default()
    }
}

/// An in-memory server whose runtime config carries `node` and whose selected packs are
/// `packs`. `with_runtime` is required: `KhiveMcpServer::new`/`with_packs` leave
/// `channel_outbox_runtime` empty (`crates/khive-mcp/src/server.rs:245`) and only
/// `with_runtime` fills it (`server.rs:255-262`), which `prepare_node_channel` reads.
fn build_server(node: Option<NodeChannelConfig>, packs: &[&str]) -> KhiveMcpServer {
    let packs: Vec<String> = packs.iter().map(|name| name.to_string()).collect();
    let config = RuntimeConfig {
        db_path: None,
        packs: packs.clone(),
        node_channel: node.map(Arc::new),
        ..RuntimeConfig::no_embeddings()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    KhiveMcpServer::with_packs(runtime.clone(), &packs)
        .expect("server")
        .with_runtime(runtime)
}

/// A non-daemon invocation (`args.daemon` is false by default; `args.rs:96`).
fn client_args() -> Args {
    Args::parse_from(["mcp"])
}

// Kills M1: in `crates/khive-mcp/src/serve/node.rs`, replacing
// `        .map(|runtime| runtime.config().node_channel.is_some())` with
// `        .map(|_runtime| false)` makes `enabled` always false, so the comm-pack
// refusal never fires and this test sees `Ok(None)` instead of the refusal text.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn prepare_refuses_an_enabled_table_without_the_comm_pack() {
    let server = build_server(Some(enabled_table()), &["kg"]);
    let err = prepare_node_channel(&server, &client_args())
        .await
        .err()
        .expect("an enabled table without the comm pack must be refused");
    assert_eq!(err.to_string(), COMM_PACK_REQUIRED);
}

// Kills: refusing or preparing state when the runtime carries no node table. It does not
// kill M1 (with `enabled` forced false this path is already `Ok(None)`).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn prepare_without_a_node_table_prepares_nothing() {
    let server = build_server(None, &["kg"]);
    match prepare_node_channel(&server, &client_args()).await {
        Ok(None) => {}
        Ok(Some(_)) => panic!("no node table must prepare no host state"),
        Err(err) => panic!("no node table must not refuse: {err}"),
    }
}

// Targets M2: in `crates/khive-mcp/src/serve/node.rs`, replacing
// `    if !resident || server.default_runtime_is_read_only() {` with `    if false {`
// removes the resident check, so a non-daemon server falls through to the host-state
// lookup (`node.rs:63`) and `state.services().await?` (`node.rs:66`). By reading, the
// comm pack builds its host state from the runtime without I/O
// (`khive-pack-comm/src/node/mod.rs:80-101`) and `validate()` needs no pins
// (`khive-runtime/src/node_config.rs:162-299`), so the lookup returns `Some` here and the
// mutant then returns `Err` or `Ok(Some(_))`, never `Ok(None)`, failing this test. If the
// lookup returned `None` here instead, this test would also pass under that mutant.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn prepare_on_a_non_resident_server_prepares_nothing() {
    let server = build_server(Some(enabled_table()), &["kg", "comm"]);
    let args = client_args();
    assert!(
        !args.daemon,
        "the fixture must be a non-resident invocation"
    );
    match prepare_node_channel(&server, &args).await {
        Ok(None) => {}
        Ok(Some(_)) => panic!("a non-resident server must not prepare host state"),
        Err(err) => panic!("a non-resident server must not initialize services: {err}"),
    }
}
