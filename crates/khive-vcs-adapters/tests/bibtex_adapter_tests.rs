use std::io::{self, BufReader, Cursor, Read};

use khive_vcs_adapters::{BibtexFormatAdapter, BibtexImportStats, FormatAdapter};
use serde_json::{json, Value};

fn records(
    source: &[u8],
    chunk: usize,
) -> (Vec<Value>, Vec<Value>, BibtexImportStats, Vec<String>) {
    let mut adapter =
        BibtexFormatAdapter::from_reader(BufReader::with_capacity(chunk, source)).unwrap();
    let stats = adapter.stats();
    let warnings = adapter.warnings().to_vec();
    let entities: Vec<_> = adapter
        .entities()
        .map(|e| serde_json::to_value(e.unwrap()).unwrap())
        .collect();
    let edges = adapter
        .edges()
        .map(|e| serde_json::to_value(e.unwrap()).unwrap())
        .collect();
    (entities, edges, stats, warnings)
}

#[test]
fn maps_fixed_fields_and_resolves_forward_and_backward_crossrefs() {
    let source = br#"
@string{venue = "Journal " # {of Examples}}
@article{first, title={First}, abstract={An abstract}, author={Doe, Jane and Smith, John},
 year=2026, journal=venue, booktitle={Ignored fallback}, doi={10.1/first},
 url={https://example.org/first}, crossref={second}}
@book(second, author={Doe, Jane and Smith, John}, year={2025}, booktitle={Proceedings},
 archivePrefix={arXiv}, eprint={2601.00001}, url={https://ignored.example/}, crossref={first})
"#;
    let (entities, edges, stats, warnings) = records(source, 7);
    assert_eq!(
        stats,
        BibtexImportStats {
            entries: 2,
            skipped: 0,
            warnings: 0
        }
    );
    assert!(warnings.is_empty());
    assert_eq!(entities.len(), 2);
    assert!(entities
        .iter()
        .all(|e| e["kind"] == "document" && e["entity_type"] == "paper"));
    assert_eq!(entities[0]["name"], "First");
    assert_eq!(entities[0]["description"], "An abstract");
    assert_eq!(
        entities[0]["properties"],
        json!({
            "authors":"Doe, Jane and Smith, John", "year":"2026", "venue":"Journal of Examples",
            "doi":"10.1/first", "source":"url:https://example.org/first"
        })
    );
    assert_eq!(entities[1]["name"], "second");
    assert_eq!(
        entities[1]["properties"],
        json!({
            "authors":"Doe, Jane and Smith, John", "year":"2025", "venue":"Proceedings",
            "source":"arxiv:2601.00001"
        })
    );
    assert_eq!(edges.len(), 2);
    for (edge, from, to) in [(&edges[0], 0, 1), (&edges[1], 1, 0)] {
        assert_eq!(edge["source"], entities[from]["id"]);
        assert_eq!(edge["target"], entities[to]["id"]);
        assert_eq!(edge["relation"], "depends_on");
        assert_eq!(edge["weight"], 0.7);
    }
}

#[test]
fn every_chunk_size_preserves_multiline_protected_text_and_comments() {
    let source = r#"% @article{not_an_entry}
@comment(quoted " text and % percent {with ) inside} @article)
@preamble{"preamble " # {not an entity}}
@string{v = {前文}}
@article{k,
 title = "Quoted {nested "quote"} 東京",
 abstract = {First line
@article{this is literal content}
100% literal and \{balanced\} braces},
 journal = v # " 続き" % syntax comment hiding } @other{
}
"#;
    let normalize = |mut entities: Vec<Value>| {
        for entity in &mut entities {
            entity.as_object_mut().unwrap().remove("id");
        }
        entities
    };
    let (expected, edges, stats, warnings) = records(source.as_bytes(), source.len());
    assert_eq!(expected.len(), 1);
    assert!(edges.is_empty());
    assert!(warnings.is_empty());
    assert_eq!(expected[0]["name"], "Quoted {nested \"quote\"} 東京");
    assert!(expected[0]["description"]
        .as_str()
        .unwrap()
        .contains("@article{this is literal content}"));
    assert_eq!(expected[0]["properties"]["venue"], "前文 続き");
    let expected = normalize(expected);
    for chunk in 1..=source.len() {
        let (entities, edges, actual_stats, warnings) = records(source.as_bytes(), chunk);
        assert_eq!(normalize(entities), expected, "chunk={chunk}");
        assert!(edges.is_empty());
        assert_eq!(actual_stats, stats);
        assert!(warnings.is_empty());
    }
}

#[test]
fn balanced_syntax_errors_skip_only_the_bad_entry_and_macros_survive() {
    let source = br#"@string{v={kept}}
@book{one,title=v}
@article{bad,title {missing equals}}
@string{v=unknown}
@book{two,title=v}
@article{undefined,title=missing}
@string{V={replacement}}
@book{three,title=v}
"#;
    let (entities, edges, stats, warnings) = records(source, 1);
    assert_eq!(
        stats,
        BibtexImportStats {
            entries: 5,
            skipped: 2,
            warnings: 3
        }
    );
    assert_eq!(
        entities
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["kept", "kept", "replacement"]
    );
    assert!(edges.is_empty());
    assert!(warnings.iter().any(|w| w.contains("line 3")));
    assert!(warnings.iter().any(|w| w.contains("undefined macro")));
}

#[test]
fn unfinished_tail_never_resynchronizes_on_an_embedded_entry() {
    for tail in [
        "@book{bad,title={unfinished\n@book{fake,title={not imported}}",
        "@book{bad,title=\"unfinished\n@book{fake,title={not imported}}",
    ] {
        let source = format!("@book{{good,title={{Kept}}}}\n{tail}");
        let (entities, _, stats, warnings) = records(source.as_bytes(), 3);
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0]["name"], "Kept");
        assert_eq!(
            stats,
            BibtexImportStats {
                entries: 2,
                skipped: 1,
                warnings: 1
            }
        );
        assert!(warnings[0].contains("unfinished entry"));
    }
}

#[test]
fn duplicate_keys_and_missing_crossrefs_are_fatal() {
    for source in [
        "@book{k} @article{k}",
        "@book{k,title=undefined} @article{k}",
        "@book{k,crossref={missing}}",
        "@book{k,crossref={bad}} @article{bad,title {invalid}}",
    ] {
        assert!(
            BibtexFormatAdapter::from_reader(source.as_bytes()).is_err(),
            "{source}"
        );
    }
}

#[test]
fn input_io_and_utf8_failures_remain_fatal_after_valid_entries() {
    struct FailsAfter(Cursor<Vec<u8>>);
    impl Read for FailsAfter {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.0.read(buffer)?;
            if count == 0 {
                Err(io::Error::other("fixture read failure"))
            } else {
                Ok(count)
            }
        }
    }
    let input = FailsAfter(Cursor::new(b"@book{good}".to_vec()));
    let error = BibtexFormatAdapter::from_reader(BufReader::with_capacity(2, input))
        .err()
        .unwrap();
    assert!(error.to_string().contains("fixture read failure"));
    for suffix in [
        vec![0xff],
        vec![0xc0, 0x80],
        vec![0xed, 0xa0, 0x80],
        vec![0xf4, 0x90, 0x80, 0x80],
        vec![0xe2, 0x82],
    ] {
        let mut input = b"@book{good}\n% junk ".to_vec();
        input.extend(suffix);
        let error = BibtexFormatAdapter::from_reader(BufReader::with_capacity(1, input.as_slice()))
            .err()
            .unwrap();
        assert!(error.to_string().contains("UTF-8"));
    }
}

#[test]
fn default_depth_limit_refuses_the_next_level() {
    for (nesting, accepted) in [(255, true), (256, false)] {
        let source = format!(
            "@book{{k,title={}x{}}}",
            "{".repeat(nesting),
            "}".repeat(nesting)
        );
        assert_eq!(
            BibtexFormatAdapter::from_reader(source.as_bytes()).is_ok(),
            accepted
        );
    }
}

#[test]
fn empty_and_comment_only_sources_have_no_records() {
    for source in [
        "",
        "% no entries",
        "@comment{\"% @literal}",
        "@preamble{{text}} @string{}",
    ] {
        let (entities, edges, stats, warnings) = records(source.as_bytes(), 1);
        assert!(entities.is_empty());
        assert!(edges.is_empty());
        assert_eq!(stats, BibtexImportStats::default());
        assert!(warnings.is_empty());
    }
}
