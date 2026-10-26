// ── Record identifier in rendered output ─────────────────────────

#[test]
fn violation_message_includes_entity_id_and_name() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[(
            "bbbbbbbb-0000-0000-0000-000000000002",
            "nonsense",
            "BadEntity",
        )],
    );
    let kinds = base_entity_kinds();
    let result = check_valid_entity_kinds(&kg_dir.join("entities.ndjson"), &kinds);
    assert!(!result.passed);
    let msg = &result.violations[0].message;
    assert!(
        msg.contains("bbbbbbbb-0000-0000-0000-000000000002"),
        "violation message must include entity id: {msg}"
    );
    assert!(
        msg.contains("BadEntity"),
        "violation message must include entity name: {msg}"
    );
}

#[test]
fn edge_violation_message_includes_source_target() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "not_a_real_relation",
        )],
    );
    let result = check_valid_edge_relations(&kg_dir.join("edges.ndjson"));
    assert!(!result.passed);
    let msg = &result.violations[0].message;
    assert!(
        msg.contains("aaaaaaaa-0000-0000-0000-000000000001"),
        "edge violation message must include source_id: {msg}"
    );
    assert!(
        msg.contains("bbbbbbbb-0000-0000-0000-000000000002"),
        "edge violation message must include target_id: {msg}"
    );
}

// ── Edge relation tests (preserved from prior PR) ─────────────────────────

#[test]
fn invalid_edge_relation_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[
            (
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "extends",
            ),
            (
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "not_a_real_relation",
            ),
        ],
    );
    let result = check_valid_edge_relations(&kg_dir.join("edges.ndjson"));
    assert!(!result.passed, "invalid edge relation must fail");
    assert_eq!(result.violations.len(), 1);
    assert!(
        result.violations[0].message.contains("not_a_real_relation"),
        "violation message should name the bad relation: {}",
        result.violations[0].message
    );
    assert!(
        result.violations[0].message.contains("extends"),
        "violation message should list valid relations: {}",
        result.violations[0].message
    );
}

#[test]
fn valid_edge_relations_all_pass() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[
            (
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "extends",
            ),
            (
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "variant_of",
            ),
        ],
    );
    let result = check_valid_edge_relations(&kg_dir.join("edges.ndjson"));
    assert!(result.passed, "valid edge relations must pass");
    assert!(result.violations.is_empty());
}

#[test]
fn structural_checks_include_taxonomy_rules() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "nonsense", "Bad")],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "aaaaaaaa-0000-0000-0000-000000000001",
            "not_valid",
        )],
    );
    let taxonomy = real_taxonomy();
    let results = structural_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &taxonomy,
    );
    let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
    assert!(
        ids.contains(&"valid-entity-kinds"),
        "structural_checks must include valid-entity-kinds"
    );
    assert!(
        ids.contains(&"valid-edge-relations"),
        "structural_checks must include valid-edge-relations"
    );
    let entity_kind_result = results
        .iter()
        .find(|r| r.id == "valid-entity-kinds")
        .unwrap();
    assert!(!entity_kind_result.passed, "nonsense kind must fail");
    let edge_rel_result = results
        .iter()
        .find(|r| r.id == "valid-edge-relations")
        .unwrap();
    assert!(!edge_rel_result.passed, "invalid relation must fail");
}

// ── The command's check/fix/recheck and rendering pipeline (#6105) ───────────

fn fix_report_args(tmp: &TempDir) -> ValidateArgs {
    ValidateArgs {
        repo: tmp.path().to_path_buf(),
        fix: true,
        strict: true,
        format: OutputFormat::Text,
        verbose: false,
        quiet: false,
        rules: None,
        no_rules: true,
    }
}

fn render_fix_report(run: &ValidationRun, args: &ValidateArgs) -> (String, String) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    render_validation(run, args, &mut stdout, &mut stderr).unwrap();
    (
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

#[test]
fn validate_fix_reports_moved_entities_and_post_fix_status() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("c", "concept", "C"),
            ("a", "concept", "A"),
            ("b", "concept", "B"),
        ],
    );
    let edges = "  { \"source\": \"a\", \"target\": \"b\", \"relation\": \"contains\" }\n\n";
    std::fs::write(kg_dir.join("edges.ndjson"), edges).unwrap();
    let mut args = fix_report_args(&tmp);

    let run = run_validation(&args).unwrap();

    assert!(
        run.report.summary.passed,
        "strict validation must use the fixed files"
    );
    assert_eq!(run.report.summary.warnings, 0);
    let fixes = run.fixes.as_ref().unwrap();
    assert_eq!(fixes.files.len(), 1);
    assert_eq!(fixes.files[0].file, "entities.ndjson");
    assert_eq!(fixes.files[0].moved, 3);
    assert_eq!(fixes.fixed.warnings, 1);
    assert_eq!(
        std::fs::read_to_string(kg_dir.join("edges.ndjson")).unwrap(),
        edges
    );
    let (text, stderr) = render_fix_report(&run, &args);
    assert!(text.contains("~ sort-order: re-sorted entities.ndjson (3 lines moved)"));
    assert!(text.contains("1 warning(s) fixed, 0 warning(s) unfixable"));
    assert!(!text.contains("is not sorted"));
    assert!(stderr.is_empty());

    args.format = OutputFormat::Json;
    let (json, details) = render_fix_report(&run, &args);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 2);
    assert_eq!(value["summary"]["passed"], true);
    assert_eq!(value["summary"]["warnings"], 0);
    assert!(
        value.get("fixes").is_none(),
        "keep the public report schema"
    );
    assert!(details.contains("entities.ndjson (3 lines moved)"));
    args.format = OutputFormat::Github;
    let (annotations, details) = render_fix_report(&run, &args);
    assert!(
        annotations.is_empty(),
        "fixed warnings must not emit stale annotations"
    );
    assert!(details.contains("entities.ndjson (3 lines moved)"));

    let entities = std::fs::read(kg_dir.join("entities.ndjson")).unwrap();
    let second = run_validation(&args).unwrap();
    assert!(second.fixes.as_ref().unwrap().files.is_empty());
    assert_eq!(second.fixes.as_ref().unwrap().fixed.warnings, 0);
    assert_eq!(
        std::fs::read(kg_dir.join("entities.ndjson")).unwrap(),
        entities
    );
    assert_eq!(
        std::fs::read_to_string(kg_dir.join("edges.ndjson")).unwrap(),
        edges
    );
}

#[test]
fn validate_fix_reports_stable_edge_positions_and_preserves_sorted_entities() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    let entities = " { \"id\": \"a\", \"kind\": \"concept\", \"name\": \"A\" }\n\n { \"id\": \"b\", \"kind\": \"concept\", \"name\": \"B\" }\n { \"id\": \"c\", \"kind\": \"concept\", \"name\": \"C\" }";
    std::fs::write(kg_dir.join("entities.ndjson"), entities).unwrap();
    let edges = concat!(
        "{\"source\":\"b\",\"source_id\":\"a\",\"target\":\"c\",\"relation\":\"contains\",\"marker\":\"last\"}\n",
        "{\"source_id\":\"a\",\"target_id\":\"b\",\"relation\":\"contains\",\"marker\":\"first-tie\"}\n",
        "{\"source\":\"a\",\"source_id\":\"c\",\"target\":\"b\",\"target_id\":\"c\",\"relation\":\"contains\",\"marker\":\"second-tie\"}\n",
    );
    std::fs::write(kg_dir.join("edges.ndjson"), edges).unwrap();
    let args = fix_report_args(&tmp);

    let run = run_validation(&args).unwrap();

    assert!(run.report.summary.passed);
    let fixes = run.fixes.as_ref().unwrap();
    assert_eq!(fixes.files.len(), 1);
    assert_eq!(fixes.files[0].file, "edges.ndjson");
    assert_eq!(fixes.files[0].moved, 3);
    assert_eq!(
        std::fs::read_to_string(kg_dir.join("entities.ndjson")).unwrap(),
        entities
    );
    let sorted = std::fs::read_to_string(kg_dir.join("edges.ndjson")).unwrap();
    let markers: Vec<String> = sorted
        .lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            value["marker"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(markers, ["first-tie", "second-tie", "last"]);
    let (text, _) = render_fix_report(&run, &args);
    assert!(text.contains("edges.ndjson (3 lines moved)"));
    assert!(!text.contains("re-sorted entities.ndjson"));
}

#[test]
fn validate_fix_rechecks_configurable_rules_and_reports_unfixable_failures() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("b", "concept", "B"),
            ("a", "concept", "A"),
            ("c", "invalid-kind", "C"),
        ],
    );
    write_edges(&kg_dir, &[]);
    let rules_path = tmp.path().join("custom-rules.toml");
    std::fs::write(
        &rules_path,
        r#"
[[rules]]
id = "concept-description"
severity = "warning"
kind = "entity"
condition = "kind=concept"
require_field = "description"
message = "Concept {id} missing description"
"#,
    )
    .unwrap();
    // The explicit --rules path must remain authoritative.
    std::fs::write(kg_dir.join("rules.toml"), "not = [valid TOML").unwrap();
    let mut args = fix_report_args(&tmp);
    args.no_rules = false;
    args.rules = Some(rules_path);

    let run = run_validation(&args).unwrap();

    assert!(!run.report.summary.passed);
    assert_eq!(run.report.summary.errors, 1);
    assert_eq!(run.report.summary.warnings, 1);
    assert_eq!(run.fixes.as_ref().unwrap().fixed.warnings, 1);
    let configurable = run
        .report
        .rules
        .iter()
        .find(|rule| rule.id == "concept-description")
        .unwrap();
    let ids: Vec<_> = configurable
        .violations
        .iter()
        .map(|violation| violation.entity_id.as_deref())
        .collect();
    assert_eq!(
        ids,
        [Some("a"), Some("b")],
        "the configurable pass must see sorted, final file contents"
    );
    let (text, _) = render_fix_report(&run, &args);
    assert!(text.contains("0 error(s) fixed, 1 error(s) unfixable"));
    assert!(text.contains("1 warning(s) fixed, 1 warning(s) unfixable"));
    assert!(text.contains("Concept a missing description"));
}

#[test]
fn validate_without_fix_preserves_files_strict_status_and_report_formats() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(&kg_dir, &[("b", "concept", "B"), ("a", "concept", "A")]);
    write_edges(&kg_dir, &[]);
    std::fs::write(
        kg_dir.join("rules.toml"),
        "malformed rules ignored by --no-rules",
    )
    .unwrap();
    let before = std::fs::read(kg_dir.join("entities.ndjson")).unwrap();
    let mut args = fix_report_args(&tmp);
    args.fix = false;
    args.strict = false;
    args.quiet = true;

    let run = run_validation(&args).unwrap();

    assert!(run.report.summary.passed);
    assert_eq!(run.report.summary.warnings, 1);
    assert!(run.fixes.is_none());
    assert_eq!(
        std::fs::read(kg_dir.join("entities.ndjson")).unwrap(),
        before
    );
    let (text, stderr) = render_fix_report(&run, &args);
    assert_eq!(
        text,
        "\nSummary: 0 error(s), 1 warning(s), 2 entities, 0 edges\n"
    );
    assert!(stderr.is_empty());
    args.format = OutputFormat::Json;
    let (json, stderr) = render_fix_report(&run, &args);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 2);
    assert_eq!(
        value["summary"],
        serde_json::json!({
            "errors": 0, "warnings": 1, "info": 0, "entities": 2,
            "edges": 0, "empty": false, "passed": true,
        })
    );
    assert_eq!(value["rules"].as_array().unwrap().len(), 7);
    assert!(json.ends_with('\n'));
    assert!(stderr.is_empty());
    args.format = OutputFormat::Github;
    let (annotations, stderr) = render_fix_report(&run, &args);
    assert_eq!(
        annotations,
        "::warning ::entities.ndjson is not sorted by UUID; run `kkernel kg validate --fix`\n"
    );
    assert!(stderr.is_empty());
    args.strict = true;
    let strict = run_validation(&args).unwrap();
    assert!(!strict.report.summary.passed);
    assert_eq!(
        std::fs::read(kg_dir.join("entities.ndjson")).unwrap(),
        before
    );
}

#[test]
fn validate_fix_without_fixable_failures_preserves_bytes_and_quiet_summary() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    let entities = " { \"id\": \"a\", \"kind\": \"invalid-kind\", \"name\": \"A\" }\n\n";
    let edges = "\n  \n";
    std::fs::write(kg_dir.join("entities.ndjson"), entities).unwrap();
    std::fs::write(kg_dir.join("edges.ndjson"), edges).unwrap();
    let mut args = fix_report_args(&tmp);
    args.quiet = true;

    let run = run_validation(&args).unwrap();

    assert!(!run.report.summary.passed);
    assert!(run.fixes.as_ref().unwrap().files.is_empty());
    assert_eq!(
        std::fs::read_to_string(kg_dir.join("entities.ndjson")).unwrap(),
        entities
    );
    assert_eq!(
        std::fs::read_to_string(kg_dir.join("edges.ndjson")).unwrap(),
        edges
    );
    let (text, stderr) = render_fix_report(&run, &args);
    assert_eq!(text, "\nSummary: 0 error(s) fixed, 1 error(s) unfixable, 0 warning(s) fixed, 0 warning(s) unfixable, 0 info(s) fixed, 0 info(s) unfixable, 1 entities, 0 edges\n");
    assert!(stderr.is_empty());
}

#[test]
fn validate_fix_propagates_malformed_peer_without_successful_report() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(&kg_dir, &[("b", "concept", "B"), ("a", "concept", "A")]);
    let edges = concat!(
        "{\"source\":\"b\",\"target\":\"a\",\"relation\":\"contains\"}\n",
        "not-valid-json\n",
    );
    std::fs::write(kg_dir.join("edges.ndjson"), edges).unwrap();
    let args = fix_report_args(&tmp);

    let error = run_validation(&args).expect_err("a malformed fix input must propagate refusal");

    assert!(error.to_string().contains("edges.ndjson line 2"));
    assert!(error
        .to_string()
        .contains("refusing to apply --fix over malformed JSON"));
    assert_eq!(
        std::fs::read_to_string(kg_dir.join("edges.ndjson")).unwrap(),
        edges
    );
    // Existing entities-before-edges side effects remain; no atomic rollback is promised.
    let entities = std::fs::read_to_string(kg_dir.join("entities.ndjson")).unwrap();
    let first: serde_json::Value = serde_json::from_str(entities.lines().next().unwrap()).unwrap();
    assert_eq!(first["id"], "a");
}

#[test]
fn fix_sort_order_keeps_canonical_serialization_when_reordered() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    let path = kg_dir.join("entities.ndjson");
    std::fs::write(&path, " { \"id\": \"b\" }\n\n { \"id\": \"a\" }\n").unwrap();

    let moved = fix_sort_order(&path, "id").unwrap();

    assert_eq!(moved, 2);
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "{\"id\":\"a\"}\n{\"id\":\"b\"}\n"
    );
}

#[test]
fn validate_fix_renders_configurable_sort_order_row_after_single_repair_detail() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    std::fs::write(
        kg_dir.join("entities.ndjson"),
        concat!(
            "{\"id\":\"b\",\"kind\":\"concept\",\"name\":\"B\",\"description\":\"present\"}\n",
            "{\"id\":\"a\",\"kind\":\"concept\",\"name\":\"A\",\"description\":\"present\"}\n",
        ),
    )
    .unwrap();
    write_edges(&kg_dir, &[]);
    std::fs::write(
        kg_dir.join("rules.toml"),
        r#"
[[rules]]
id = "sort-order"
severity = "warning"
kind = "entity"
condition = "kind=concept"
require_field = "description"
message = "Concept {id} missing description"
"#,
    )
    .unwrap();
    let mut args = fix_report_args(&tmp);
    args.no_rules = false;

    let run = run_validation(&args).unwrap();

    assert!(run.report.summary.passed);
    let sort_rows: Vec<_> = run
        .report
        .rules
        .iter()
        .filter(|rule| rule.id == "sort-order")
        .collect();
    assert_eq!(
        sort_rows.len(),
        2,
        "the configured ID is accepted alongside the built-in rule"
    );
    assert!(sort_rows.iter().all(|rule| rule.passed));
    let (text, stderr) = render_fix_report(&run, &args);
    assert_eq!(text.matches("~ sort-order: re-sorted ").count(), 1);
    assert!(text.contains("entities.ndjson (2 lines moved)"));
    assert_eq!(
        text.matches("  \u{2713} sort-order\n").count(),
        1,
        "retain the passed configured rule row"
    );
    assert!(stderr.is_empty());
}
