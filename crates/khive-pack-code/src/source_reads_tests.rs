use super::*;

fn observations(scan: &SourceReadScan) -> Vec<(&str, &str, &str)> {
    scan.reads
        .iter()
        .map(|read| (read.kind.as_str(), read.path.as_str(), read.base.as_str()))
        .collect()
}

fn has_residual(scan: &SourceReadScan, text: &str) -> bool {
    scan.unresolved.iter().any(|reason| reason.contains(text))
}

#[test]
fn rust_include_family_decodes_literal_values_and_raw_strings() {
    let scan = extract_source_reads(
        "rust",
        r##"
        include!("fixtures/module.rs",);
        const TEXT: &str = include_str!("fixtures/\u{03bb}.txt");
        const BYTES: &[u8] = include_bytes!(r#"fixtures/raw.bin"#);
        "##,
    );
    assert_eq!(
        observations(&scan),
        vec![
            ("include", "fixtures/module.rs", "file"),
            ("include_str", "fixtures/λ.txt", "file"),
            ("include_bytes", "fixtures/raw.bin", "file"),
        ]
    );
    assert!(scan.unresolved.is_empty());
}

#[test]
fn rust_external_module_path_is_file_relative() {
    let scan = extract_source_reads("rust", r#"#[path = "fixtures/model.rs"] mod model;"#);
    assert_eq!(
        observations(&scan),
        vec![("path_attribute", "fixtures/model.rs", "file")]
    );
    assert!(scan.unresolved.is_empty());
}

#[test]
fn rust_parent_relative_paths_are_left_for_the_contained_resolver() {
    let scan = extract_source_reads(
        "rust",
        r#"
        include!("../sibling.rs");
        const TEXT: &str = include_str!("../../outside-candidate.txt");
        #[path = "./shared.rs"] mod shared;
        "#,
    );
    assert_eq!(
        observations(&scan),
        vec![
            ("include", "../sibling.rs", "file"),
            ("include_str", "../../outside-candidate.txt", "file"),
            ("path_attribute", "./shared.rs", "file"),
        ]
    );
    // No file is opened and no claim that either traversal stays in the
    // repository is made by this context-free extractor.
    assert!(scan.unresolved.is_empty());
}

#[test]
fn rust_repository_literals_are_observations_not_runtime_read_proofs() {
    let scan = extract_source_reads(
        "rust",
        r##"
        const FIRST: &str = "crates/\x6b/src/lib.rs";
        const SECOND: &str = r#"scripts/check.py"#;
        "##,
    );
    assert_eq!(
        observations(&scan),
        vec![
            ("repo_literal", "crates/k/src/lib.rs", "repository"),
            ("repo_literal", "scripts/check.py", "repository"),
        ]
    );
}

#[test]
fn rust_absolute_glob_query_and_platform_paths_stay_unresolved() {
    for path in [
        "/etc/passwd",
        "C:/secrets.txt",
        r"fixtures\input.txt",
        "fixtures/*.rs",
        "fixtures/[ab].rs",
        "fixtures/input.rs?version=1",
        "fixtures//input.rs",
        "",
    ] {
        let content = format!("const DATA: &str = include_str!({path:?});");
        let scan = extract_source_reads("rust", &content);
        assert!(scan.reads.is_empty(), "unexpected path observation: {path}");
        assert!(has_residual(&scan, "path literal"), "{path}");
    }
}

#[test]
fn rust_dynamic_include_and_non_string_arguments_have_explicit_residuals() {
    for content in [
        r#"const X: &str = include_str!(concat!("crates/", "input.rs"));"#,
        r#"const X: &[u8] = include_bytes!(env!("INPUT_FILE"));"#,
        r#"include!(chosen_path);"#,
        r#"include!(b"input.rs");"#,
        r#"include!("first.rs", "second.rs");"#,
    ] {
        let scan = extract_source_reads("rust", content);
        assert!(scan.reads.is_empty(), "{content}");
        assert!(has_residual(&scan, "nonliteral"), "{content}");
    }
}

#[test]
fn rust_nonliteral_and_conditional_path_attributes_cannot_resolve_modules() {
    for content in [
        r#"#[path = selected_path] mod input;"#,
        r#"#[path = concat!("fixtures/", "input.rs")] mod input;"#,
        r#"#[cfg_attr(test, path = "fixtures/input.rs")] mod input;"#,
    ] {
        let scan = extract_source_reads("rust", content);
        assert!(scan.reads.is_empty(), "{content}");
        assert!(!scan.unresolved.is_empty(), "{content}");
    }
}

#[test]
fn rust_inline_module_path_context_is_unresolved_but_include_uses_its_file() {
    let scan = extract_source_reads(
        "rust",
        r#"
        mod outer {
            #[path = "nested.rs"] mod nested;
            const INPUT: &str = include_str!("fixtures/value.txt");
        }
        #[path = "directory"] mod inline { mod child; }
        "#,
    );
    assert_eq!(
        observations(&scan),
        vec![("include_str", "fixtures/value.txt", "file")]
    );
    assert!(has_residual(&scan, "inline module context"));
}

#[test]
fn rust_comments_and_string_fixtures_do_not_invent_reader_invocations() {
    let scan = extract_source_reads(
        "rust",
        r##"
        // include_str!("crates/comment.rs");
        /* std::fs::read_to_string("crates/comment.rs"); */
        /// crates/doc-comment.rs
        const FIXTURE: &str = r#"include_str!("crates/fixture.rs")"#;
        const BYTE_FIXTURE: &[u8] = b"crates/not-a-unicode-path.rs";
        "##,
    );
    assert!(scan.reads.is_empty());
    assert!(scan.unresolved.is_empty());
}

#[test]
fn rust_explicit_documentation_include_is_a_real_source_input() {
    let scan = extract_source_reads(
        "rust",
        r#"#[doc = include_str!("README.md")] pub struct Documented;"#,
    );
    assert_eq!(
        observations(&scan),
        vec![("include_str", "README.md", "file")]
    );
}

#[test]
fn rust_known_filesystem_calls_keep_literal_observations_and_unknown_semantics() {
    let scan = extract_source_reads(
        "rust",
        r#"
        fn inspect(path: &str) {
            let _ = std::fs::read_to_string("crates/leaf/src/lib.rs");
            let _ = std::fs::File::open(path);
            let _ = path.read_bytes();
        }
        "#,
    );
    assert_eq!(
        observations(&scan),
        vec![("repo_literal", "crates/leaf/src/lib.rs", "repository")]
    );
    assert!(has_residual(&scan, "read call"));
    assert!(has_residual(&scan, "read method"));
}

#[test]
fn rust_opaque_macros_preserve_unknowns_and_nested_expression_readers() {
    let opaque = extract_source_reads(
        "rust",
        r#"macro_rules! hidden { () => { include!("hidden.rs"); }; }"#,
    );
    assert!(has_residual(&opaque, "macro expansion"));
    let nested = extract_source_reads(
        "rust",
        r#"const INPUT: &str = concat!("prefix", include_str!("body.txt"));"#,
    );
    assert_eq!(
        observations(&nested),
        vec![("include_str", "body.txt", "file")]
    );
    assert!(has_residual(&nested, "macro expansion"));
}

#[test]
fn rust_parse_failure_and_unsupported_language_are_not_empty_successes() {
    let invalid = extract_source_reads("rust", "fn unfinished( {");
    assert!(invalid.reads.is_empty());
    assert!(has_residual(&invalid, "could not be parsed"));
    let unsupported = extract_source_reads("typescript", r#"readFile("crates/input.rs")"#);
    assert!(unsupported.reads.is_empty());
    assert!(has_residual(&unsupported, "unsupported"));
}

#[test]
fn python_plain_raw_and_triple_quoted_repo_literals_preserve_values() {
    let scan = extract_source_reads(
        "python",
        "first = 'crates/leaf/src/lib.rs'\nsecond = r\"scripts/check.py\"\nthird = '''scripts/other.py'''\n",
    );
    assert_eq!(
        observations(&scan),
        vec![
            ("repo_literal", "crates/leaf/src/lib.rs", "repository"),
            ("repo_literal", "scripts/check.py", "repository"),
            ("repo_literal", "scripts/other.py", "repository"),
        ]
    );
    assert!(has_residual(&scan, "lexical"));
}

#[test]
fn python_comments_and_string_fixture_contents_do_not_create_reads_or_calls() {
    let scan = extract_source_reads(
        "python",
        r###"
        # open("crates/comment.rs")
        fixture = 'open("crates/string-fixture.rs")'
        multiline = '''example:
        Path("scripts/fixture.py").read_text()
        '''
        "###,
    );
    assert!(scan.reads.is_empty());
    assert!(!has_residual(&scan, "filesystem/read call"));
}

#[test]
fn python_known_readers_keep_dynamic_residuals() {
    let scan = extract_source_reads(
        "python",
        "text = Path('crates/leaf/src/lib.rs').read_text()\nwith open(chosen) as file:\n    text = file.read()\n",
    );
    assert_eq!(
        observations(&scan),
        vec![("repo_literal", "crates/leaf/src/lib.rs", "repository")]
    );
    assert!(has_residual(&scan, "filesystem/read call"));
}

#[test]
fn python_dynamic_byte_and_escaped_literals_are_not_guessed_unicode_paths() {
    for (content, reason) in [
        (r#"path = f"crates/{name}.rs""#, "formatted string"),
        (r#"path = b"crates/file.rs""#, "byte string"),
        (r#"path = "crates/\x66ile.rs""#, "literal decoding"),
    ] {
        let scan = extract_source_reads("python", content);
        assert!(scan.reads.is_empty(), "{content}");
        assert!(has_residual(&scan, reason), "{content}");
    }
}

#[test]
fn python_glob_and_unsupported_repository_normalization_are_explicit() {
    for content in [
        "path = 'crates/**/*.rs'",
        "path = '../crates/input.rs'",
        "path = './scripts/check.py'",
        r#"path = r"crates\input.rs""#,
    ] {
        let scan = extract_source_reads("python", content);
        assert!(scan.reads.is_empty(), "{content}");
        assert!(scan.unresolved.len() > 1, "{content}");
    }
}

#[test]
fn python_unterminated_strings_are_explicit_and_do_not_invent_literals() {
    let scan = extract_source_reads("python", "path = 'crates/incomplete.rs\n");
    assert!(scan.reads.is_empty());
    assert!(has_residual(&scan, "unterminated"));
}

#[test]
fn observation_deduplication_and_serialized_field_names_are_stable() {
    let scan = extract_source_reads(
        "rust",
        r#"const A: &str = include_str!("shared.txt"); const B: &str = include_str!("shared.txt");"#,
    );
    let value = serde_json::to_value(&scan).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "reads": [{"kind": "include_str", "path": "shared.txt", "base": "file"}],
            "unresolved": []
        })
    );
}
