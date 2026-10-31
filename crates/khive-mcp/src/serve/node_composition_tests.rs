//! Composition tests for `prepare_node_channel`'s gating (C2).

use super::node::{require_comm_pack, COMM_PACK_REQUIRED};

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
