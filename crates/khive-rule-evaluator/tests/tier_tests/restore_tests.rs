fn restore(substrate: &str) -> Op {
    let Op::Delete(delete) = delete(substrate) else {
        unreachable!("delete fixture must build a delete")
    };
    Op::Restore(khive_changeset::RestoreOp::new(delete.target_id, delete.preimage).unwrap())
}

#[test]
fn restores_keep_tier_two_and_coverage_findings_stay_target_scoped() {
    for substrate in ["entity", "note", "edge"] {
        let baseline = classify("", vec![restore(substrate)], &[]);
        assert_eq!(baseline[0].tier, Tier::Tier2, "{substrate}");
        assert_eq!(baseline[0].reason_ids, ["adr102.restore"], "{substrate}");
        let result = classify(
            "",
            vec![restore(substrate), create(C, false)],
            &[TierFinding {
                severity: "error",
                subject_id: Some(A),
            }],
        );
        assert_eq!(result[0].tier, Tier::Tier2, "{substrate}");
        assert_eq!(
            result[0].reason_ids,
            ["adr102.restore", "validation.error_severity_finding"],
            "{substrate}"
        );
        assert_eq!(result[1].tier, Tier::Tier1, "{substrate}");
        assert_eq!(result[1].reason_ids, ["adr102.create"], "{substrate}");
    }
}
