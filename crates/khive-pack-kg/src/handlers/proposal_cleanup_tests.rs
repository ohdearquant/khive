use super::*;
use khive_runtime::{KhiveRuntime, Namespace};
use serde_json::json;

const NOW: i64 = 10_000_000_000;

#[test]
fn fixed_duration_units_and_checked_timestamp_boundaries() {
    for (text, duration) in [
        ("1s", 1_000_000),
        ("2m", 120_000_000),
        ("3h", 10_800_000_000),
        ("4d", 345_600_000_000),
        ("001s", 1_000_000),
    ] {
        assert_eq!(proposal_cleanup_cutoff(text, NOW).unwrap(), NOW - duration);
    }
    for text in [
        "",
        "s",
        "0s",
        "00d",
        "-1s",
        "+1s",
        "1.0s",
        " 1s",
        "1s ",
        "1 s",
        "1h30m",
        "1w",
        "1S",
        "１s",
        "١s",
        "1\0s",
        "1秒",
        "9223372036854775808s",
        "9223372036854775807d",
        "9223372036855s",
    ] {
        assert!(proposal_cleanup_cutoff(text, NOW).is_err(), "{text:?}");
    }
    assert_eq!(
        proposal_cleanup_cutoff("1s", i64::MIN + 1_000_000).unwrap(),
        i64::MIN
    );
    assert!(proposal_cleanup_cutoff("1s", i64::MIN + 999_999).is_err());
}

#[tokio::test]
async fn invalid_parameters_and_cutoff_overflow_never_acquire_a_writer() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let pack = KgPack::new(runtime.clone());
    runtime.sql().writer().await.unwrap().execute(SqlStatement::new(
        "INSERT INTO proposals_open (proposal_id,namespace,proposer,title,status,created_at,updated_at) VALUES ('invalid-input-witness','local','another proposer','Untouched','rejected',1,1)", vec![]
    )).await.unwrap();
    let projection_sql = "SELECT * FROM proposals_open";
    let projection_before = serde_json::to_value(
        runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_all(SqlStatement::new(projection_sql, vec![]))
            .await
            .unwrap(),
    )
    .unwrap();
    let before = runtime
        .backend()
        .pool()
        .writer_acquisition_snapshot()
        .acquisitions;
    for params in [
        json!({}),
        json!({"older_than": null}),
        json!({"older_than": 1}),
        json!({"older_than": true}),
        json!({"older_than": []}),
        json!({"older_than": "0s"}),
        json!({"older_than": "1d", "cutoff": 0}),
        json!({"older_than": "1s", "now": 0}),
        json!([]),
        json!({"older_than": "9223372036854775808s"}),
        json!({"older_than": "9223372036854775807d"}),
    ] {
        assert!(matches!(
            pack.handle_proposal_cleanup_at(&token, params, NOW).await,
            Err(RuntimeError::InvalidInput(_))
        ));
        assert_eq!(
            runtime
                .backend()
                .pool()
                .writer_acquisition_snapshot()
                .acquisitions,
            before
        );
    }
    assert!(pack
        .handle_proposal_cleanup_at(&token, json!({"older_than": "1s"}), i64::MIN)
        .await
        .is_err());
    assert_eq!(
        runtime
            .backend()
            .pool()
            .writer_acquisition_snapshot()
            .acquisitions,
        before
    );
    assert_eq!(
        serde_json::to_value(
            runtime
                .sql()
                .reader()
                .await
                .unwrap()
                .query_all(SqlStatement::new(projection_sql, vec![]))
                .await
                .unwrap()
        )
        .unwrap(),
        projection_before
    );
    // Positive control: this counter observes the operator's real writer route.
    assert_eq!(
        pack.handle_proposal_cleanup_at(&token, json!({"older_than": "1s"}), NOW)
            .await
            .unwrap(),
        json!({"archived": 1})
    );
    assert!(
        runtime
            .backend()
            .pool()
            .writer_acquisition_snapshot()
            .acquisitions
            > before
    );
}

#[tokio::test]
async fn eligibility_is_exact_and_only_the_archive_marker_changes() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let cutoff = NOW - 1_000_000;
    // The explicit expected archive set exercises each independent SQL guard.
    let cases = [
        ("applied", "local", cutoff - 1, None, true),
        ("rejected", "local", cutoff - 1, None, true),
        ("applied", "local", cutoff, None, false),
        ("rejected", "local", cutoff + 1, None, false),
        ("applied", "other", cutoff - 1, None, false),
        ("rejected", "local", cutoff - 1, Some(7_i64), false),
        ("open", "local", cutoff - 1, None, false),
        ("changes_requested", "local", cutoff - 1, None, false),
        ("approved", "local", cutoff - 1, None, false),
        ("applying", "local", cutoff - 1, None, false),
        ("withdrawn", "local", cutoff - 1, None, false),
        ("applied", "local", cutoff + 1, None, false),
        ("open", "local", cutoff + 1, None, false),
        ("changes_requested", "local", cutoff + 1, None, false),
        ("approved", "local", cutoff + 1, None, false),
        ("applying", "local", cutoff + 1, None, false),
        ("withdrawn", "local", cutoff + 1, None, false),
        ("rejected", "local", cutoff + 1, Some(7_i64), false),
        ("applied", "other", cutoff + 1, None, false),
    ];
    let sql = runtime.sql();
    {
        let mut writer = sql.writer().await.unwrap();
        for (index, (status, namespace, updated, marker, _)) in cases.iter().enumerate() {
            writer.execute(SqlStatement::new(
                "INSERT INTO proposals_open (proposal_id,namespace,proposer,title,status,created_at,updated_at,expiry,last_decision,review_count,approve_count,reject_count,archived_at) VALUES (?1,?2,?3,'retained title',?4,17,?5,19,'comment',11,3,2,?6)",
                vec![SqlValue::Text(index.to_string()), SqlValue::Text((*namespace).into()),
                    SqlValue::Text(format!("proposer-{index}")), SqlValue::Text((*status).into()),
                    SqlValue::Integer(*updated), SqlValue::from_opt_i64(*marker)],
            )).await.unwrap();
        }
    }
    let before = sql
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement::new(
            "SELECT * FROM proposals_open ORDER BY CAST(proposal_id AS INTEGER)",
            vec![],
        ))
        .await
        .unwrap();
    let pack = KgPack::new(runtime.clone());
    assert_eq!(
        pack.handle_proposal_cleanup_at(&token, json!({"older_than": "1s"}), NOW)
            .await
            .unwrap(),
        json!({"archived": 2})
    );
    let after = sql
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement::new(
            "SELECT * FROM proposals_open ORDER BY CAST(proposal_id AS INTEGER)",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(after.len(), cases.len());
    for (index, ((before, after), case)) in before.iter().zip(&after).zip(cases).enumerate() {
        assert_eq!(
            after.opt_i64("archived_at").unwrap(),
            if case.4 { Some(NOW) } else { case.3 },
            "row {index}"
        );
        let without_marker = |row: &khive_storage::SqlRow| {
            serde_json::to_value(
                row.columns
                    .iter()
                    .filter(|column| column.name != "archived_at")
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        };
        assert_eq!(without_marker(after), without_marker(before), "row {index}");
    }
    assert_eq!(
        pack.handle_proposal_cleanup_at(&token, json!({"older_than": "1s"}), NOW)
            .await
            .unwrap(),
        json!({"archived": 0})
    );
}
