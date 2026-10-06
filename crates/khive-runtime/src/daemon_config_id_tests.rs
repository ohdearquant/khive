fn config_id(primary: &str, extra: &str) -> String {
    format!(
        "packs=[kg];db=:memory:;embed={primary};extra=[{extra}];fresh_tail=true;\
             blob_hydration_bytes=268435456;backend=Sqlite;outbound=[];git_write=policy;\
             brain=readers;telemetry=default;display_tz=UTC"
    )
}

#[test]
fn an_available_extra_embedder_does_not_block_daemon_reuse() {
    let client = config_id("p", "");
    let daemon = config_id("p", "m");

    assert!(super::config_ids_compatible(&client, &daemon));
}

#[test]
fn a_missing_requested_extra_embedder_is_named_and_refused() {
    let client = config_id("p", "m");
    let daemon = config_id("p", "");

    assert!(!super::config_ids_compatible(&client, &daemon));
    assert_eq!(
        super::first_config_mismatch_field(&client, Some(&daemon)),
        "extra"
    );
}

#[test]
fn extra_embedder_order_and_duplicates_do_not_block_daemon_reuse() {
    let client = config_id("p", "a,b,a");
    let daemon = config_id("p", "b,a,c,b");

    assert!(super::config_ids_compatible(&client, &daemon));
}

#[test]
fn a_primary_repeated_as_an_extra_remains_available_to_the_client() {
    let client = config_id("AllMiniLmL6V2", "");
    let daemon = config_id("AllMiniLmL6V2", "AllMiniLmL6V2");

    assert!(super::config_ids_compatible(&client, &daemon));
    assert_eq!(
        super::config_id_extra_embedder_exclusions(&client, &daemon),
        Some(vec![]),
        "the request may still use its primary model"
    );

    let client_with_primary_extra = config_id("AllMiniLmL6V2", "AllMiniLmL6V2");
    let daemon_without_extra = config_id("AllMiniLmL6V2", "");
    assert!(super::config_ids_compatible(
        &client_with_primary_extra,
        &daemon_without_extra
    ));
}

#[test]
fn a_daemon_extra_matching_the_primary_is_not_hidden_with_other_extras() {
    let client = config_id("AllMiniLmL6V2", "BgeSmallEnV15");
    let daemon = config_id("AllMiniLmL6V2", "AllMiniLmL6V2,BgeSmallEnV15");

    assert!(super::config_ids_compatible(&client, &daemon));
    assert_eq!(
        super::config_id_extra_embedder_exclusions(&client, &daemon),
        Some(vec![])
    );
}

#[test]
fn a_legacy_exact_match_refusal_names_an_extra_superset() {
    let client = config_id("p", "");
    let daemon = config_id("p", "m");

    assert!(super::config_ids_compatible(&client, &daemon));
    assert_eq!(
        super::first_config_mismatch_field(&client, Some(&daemon)),
        "extra"
    );
}

/// The request-dispatch call site applies the superset rule, not only the
/// comparison helper: a daemon holding an extra embedder serves a client
/// that declares none, and a client requesting an extra the daemon lacks
/// is refused before dispatch.
#[tokio::test]
async fn dispatch_serves_a_client_whose_extra_embedders_the_daemon_covers() {
    let covering_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let covering = MockDispatch {
        namespace: "local".to_string(),
        config_id: config_id("p", "m"),
        dispatch_calls: Arc::clone(&covering_calls),
        pool: None,
        dispatch_err: None,
    };
    let served = round_trip(covering, &base_request_frame(&config_id("p", ""))).await;
    assert!(
        served.ok && !served.config_mismatch,
        "a daemon extra-embedder superset must be served: {served:?}"
    );
    assert_eq!(
        covering_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the covered request must reach dispatch"
    );

    let lacking_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let lacking = MockDispatch {
        namespace: "local".to_string(),
        config_id: config_id("p", ""),
        dispatch_calls: Arc::clone(&lacking_calls),
        pool: None,
        dispatch_err: None,
    };
    let refused = round_trip(lacking, &base_request_frame(&config_id("p", "m"))).await;
    assert!(
        !refused.ok && refused.config_mismatch,
        "a requested extra the daemon lacks must be refused: {refused:?}"
    );
    assert_eq!(
        lacking_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a refused request must not reach dispatch"
    );
}

#[test]
fn a_different_primary_embedder_is_refused_even_with_extra_superset() {
    let client = config_id("p", "a");
    let daemon = config_id("q", "a,b");

    assert!(!super::config_ids_compatible(&client, &daemon));
    assert_eq!(
        super::first_config_mismatch_field(&client, Some(&daemon)),
        "embed"
    );
}

const ROUTING: &str = ";backends=[main:Sqlite:/d:wal_ceiling_bytes=0];pack_backends=[kg=main]";

fn with_disk_policy(id: &str, reserve: u64) -> String {
    format!("{id};sqlite_disk_guard=[[\"main\",{reserve},2000]]")
}

#[test]
fn a_disk_policy_only_difference_is_named_by_its_own_field() {
    let plain = config_id("p", "");
    let routed = format!("{plain}{ROUTING}");
    for base in [plain, routed] {
        let client = with_disk_policy(&base, 1_073_741_824);
        let daemon = with_disk_policy(&base, 2_147_483_648);

        assert!(!super::config_ids_compatible(&client, &daemon));
        assert_eq!(
            super::first_config_mismatch_field(&client, Some(&daemon)),
            "sqlite_disk_guard"
        );
    }
}

#[test]
fn a_missing_disk_policy_segment_is_a_disk_policy_mismatch() {
    let client = config_id("p", "");
    let daemon = with_disk_policy(&client, 1_073_741_824);

    assert!(!super::config_ids_compatible(&client, &daemon));
    assert_eq!(
        super::first_config_mismatch_field(&client, Some(&daemon)),
        "sqlite_disk_guard"
    );
}

#[test]
fn the_disk_policy_segment_is_parsed_apart_from_the_fields_before_it() {
    let legacy = config_id("p", "m");
    let fields = super::parse_config_id(&legacy).expect("legacy id parses");
    assert_eq!(fields.sqlite_disk_guard, None);
    assert_eq!(fields.display_timezone, "UTC");
    assert_eq!(fields.pack_backends, None);

    let plain = with_disk_policy(&legacy, 7);
    let fields = super::parse_config_id(&plain).expect("id with policy parses");
    assert_eq!(fields.sqlite_disk_guard, Some("[[\"main\",7,2000]]"));
    assert_eq!(fields.display_timezone, "UTC");

    let routed = with_disk_policy(&format!("{legacy}{ROUTING}"), 7);
    let fields = super::parse_config_id(&routed).expect("routed id with policy parses");
    assert_eq!(fields.sqlite_disk_guard, Some("[[\"main\",7,2000]]"));
    assert_eq!(fields.display_timezone, "UTC");
    assert_eq!(fields.pack_backends, Some("kg=main"));
}
