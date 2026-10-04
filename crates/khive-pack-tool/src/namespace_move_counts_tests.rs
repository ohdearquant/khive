//! The namespace move counts the authorization rows it leaves behind with its own
//! restatement of this pack's liveness and expiry tests.
//!
//! `khive-db` cannot depend on this crate, so this is the test in a crate that
//! sees both: it writes rows with the pack's own writers, asks the pack's own
//! readers which of them are in force, and requires the move's two counts to
//! agree with those answers.

use khive_db::namespace_move::{move_namespace, MoveRequest, MoveRoute, SubjectClass};
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};

use crate::policy::{
    delete_policy, insert_grant_request, select_active_grant, select_deciding_policy,
    set_grant_status, upsert_policy, PolicyWrite,
};
use crate::ToolPack;

const ADMIN: &str = "operator";

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn khive_db_counts_match_the_tool_packs_own_liveness_and_expiry_reads() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let token = runtime
        .authorize(Namespace::parse("source").expect("test namespace"))
        .expect("namespace token");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(ToolPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(runtime.backend());

    // Three policies in the source, one of them soft-deleted, and one live policy
    // in a neighbour namespace that no count may include.
    for (ns, tool, decision) in [
        ("source", "tool-live", "allow"),
        ("source", "tool-live-2", "allow"),
        ("source", "tool-gone", "deny"),
        ("other", "tool-other", "allow"),
    ] {
        let write = PolicyWrite {
            actor: "agent",
            tool,
            decision,
            note: None,
            replaces: None,
            author: ADMIN,
        };
        upsert_policy(&runtime, ns, write)
            .await
            .expect("policy written");
    }
    delete_policy(&runtime, "source", "agent", "tool-gone", ADMIN)
        .await
        .expect("policy soft-deleted");

    // Four granted grants that differ only in expiry: none, in the future, one
    // microsecond ago, and exactly now, which the pack's reader treats as expired.
    let now = 1_000_000_000_000_i64;
    let expiries = [None, Some(now + 60_000_000), Some(now - 1), Some(now)];
    let mut actors = Vec::new();
    for (index, expires_at) in expiries.into_iter().enumerate() {
        let actor = format!("agent-{index}");
        let row = insert_grant_request(&runtime, "source", &actor, "tool-x", None, None)
            .await
            .expect("grant requested");
        set_grant_status(&runtime, &token, &row, "granted", ADMIN, expires_at, None)
            .await
            .expect("grant decided");
        actors.push(actor);
    }

    // What the pack itself says is in force.
    let mut live_policies = 0_u64;
    for tool in ["tool-live", "tool-live-2", "tool-gone"] {
        let decided = select_deciding_policy(&runtime, "source", "agent", tool)
            .await
            .expect("policy read");
        live_policies += u64::from(decided.is_some());
    }
    let mut unexpired_grants = 0_u64;
    for actor in &actors {
        let active = select_active_grant(&runtime, "source", actor, "tool-x", now, None)
            .await
            .expect("grant read");
        unexpired_grants += u64::from(active.is_some());
    }
    assert_eq!(live_policies, 2, "soft-deleted policy is not in force");
    assert_eq!(unexpired_grants, 2, "an expired grant is not in force");

    let request = MoveRequest::new(
        "source",
        vec![MoveRoute {
            class: SubjectClass::Note("memo".to_string()),
            target: "elsewhere".to_string(),
        }],
    )
    .at(now);
    let writer = runtime.backend().pool().writer().expect("writer");
    let counts = move_namespace(writer.conn(), &request).expect("nothing is routed here");

    assert_eq!(counts.left_behind.get("tool_policy"), Some(&3));
    assert_eq!(counts.left_behind.get("tool_grants"), Some(&4));
    assert_eq!(counts.live_policies_left_behind, live_policies);
    assert_eq!(counts.unexpired_grants_left_behind, unexpired_grants);
}
