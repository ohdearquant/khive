use super::{AtomicEventRows, COUNTED_EVENT_INSERT_LABELS};
use crate::stores::event::{event_insert_statements, hard_delete_lineage_warning_statements};
use khive_storage::event::Event;
use khive_storage::types::SqlStatement;
use khive_types::{EventKind, SubstrateKind};
use std::collections::BTreeSet;
use uuid::Uuid;

fn canonical_builder_statements() -> Vec<(&'static str, Vec<SqlStatement>)> {
    let event = Event::new(
        "local",
        "search",
        EventKind::SearchExecuted,
        SubstrateKind::Note,
        "agent:test",
    )
    .with_payload(serde_json::json!({
        "result_kind": "note",
        "selected": [Uuid::new_v4().to_string()],
    }));
    vec![
        (
            "event_insert_statements",
            event_insert_statements(&event).unwrap(),
        ),
        (
            "hard_delete_lineage_warning_statements",
            hard_delete_lineage_warning_statements(
                "local",
                "agent:test",
                Uuid::new_v4(),
                SubstrateKind::Note,
            ),
        ),
    ]
}

fn inserts_events(sql: &str) -> bool {
    let mut words = sql.split_whitespace();
    words.next().is_some_and(|word| word == "INSERT")
        && words.next().is_some_and(|word| word == "INTO")
        && words.next().is_some_and(|word| word == "events")
}

fn uncounted_event_inserts(counted: &[&str]) -> Vec<(&'static str, Option<String>)> {
    canonical_builder_statements()
        .into_iter()
        .flat_map(|(builder, statements)| {
            statements.into_iter().filter_map(move |statement| {
                (inserts_events(&statement.sql)
                    && !statement
                        .label
                        .as_deref()
                        .is_some_and(|label| counted.contains(&label)))
                .then_some((builder, statement.label))
            })
        })
        .collect()
}

fn event_statement_builder_names(source: &str) -> BTreeSet<&str> {
    let mut names = BTreeSet::new();
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let mut declaration = line.trim_start();
        if let Some(rest) = declaration.strip_prefix("pub") {
            declaration = rest.trim_start();
            if declaration.starts_with('(') {
                let Some(end) = declaration.find(')') else {
                    continue;
                };
                declaration = declaration[end + 1..].trim_start();
            }
        }
        if let Some(rest) = declaration.strip_prefix("async") {
            declaration = rest.trim_start();
        }
        let Some(after_fn) = declaration.strip_prefix("fn") else {
            continue;
        };
        if !after_fn.starts_with(char::is_whitespace) {
            continue;
        }
        let function = &source[line_start + line.len() - after_fn.len()..];
        let Some(name) = function
            .trim_start()
            .split(|character: char| {
                character == '(' || character == '<' || character.is_whitespace()
            })
            .next()
        else {
            continue;
        };
        if function
            .split('{')
            .next()
            .is_some_and(|signature| signature.contains("SqlStatement"))
        {
            names.insert(name);
        }
    }
    names
}

#[test]
fn canonical_event_insert_labels_are_counted() {
    assert_eq!(
        event_statement_builder_names(include_str!("stores/event.rs")),
        canonical_builder_statements()
            .iter()
            .map(|(builder, _)| *builder)
            .collect(),
        "canonical event insert builders changed; sample every builder"
    );
    assert_eq!(
        uncounted_event_inserts(COUNTED_EVENT_INSERT_LABELS),
        vec![],
        "canonical event insert builder emits an uncounted label"
    );

    let mut emitted = BTreeSet::new();
    for (builder, statements) in canonical_builder_statements() {
        let rows = AtomicEventRows::default();
        let expected = statements
            .iter()
            .filter(|statement| inserts_events(&statement.sql))
            .count() as u64;
        for statement in statements {
            let before = rows.committed_rows();
            rows.observe(&statement, 0);
            assert_eq!(rows.committed_rows(), before);
            rows.observe(&statement, 1);
            if inserts_events(&statement.sql) {
                emitted.insert(statement.label.unwrap());
            }
        }
        assert_eq!(
            rows.committed_rows(),
            expected,
            "{builder}: observations must not count as event rows"
        );
    }
    assert_eq!(
        emitted,
        COUNTED_EVENT_INSERT_LABELS
            .iter()
            .map(|label| (*label).to_string())
            .collect()
    );
}

#[test]
fn missing_counted_label_names_its_builder() {
    for removed in COUNTED_EVENT_INSERT_LABELS {
        let counted: Vec<_> = COUNTED_EVENT_INSERT_LABELS
            .iter()
            .copied()
            .filter(|label| label != removed)
            .collect();
        let builder = if *removed == "event_insert_on_writer" {
            "event_insert_statements"
        } else {
            "hard_delete_lineage_warning_statements"
        };
        assert_eq!(
            uncounted_event_inserts(&counted),
            vec![(builder, Some((*removed).to_string()))]
        );
    }
}

#[test]
fn new_event_insert_builder_requires_a_sample() {
    let source = include_str!("stores/event.rs");
    let probe = format!(
        "{source}\nfn new_event_builder() -> Vec<SqlStatement> {{\n\
         let sql = \"INSERT INTO events VALUES (?1)\";\n}}\n"
    );
    let mut expected = event_statement_builder_names(source);
    assert!(expected.insert("new_event_builder"));
    assert_eq!(event_statement_builder_names(&probe), expected);
}

#[test]
fn builder_census_recognizes_visibility_and_multiline_signatures() {
    let source = "pub(super) fn restricted() -> Vec<SqlStatement> {}\n\
                  pub(in crate::stores) async fn asynchronous() -> SqlStatement {}\n\
                  fn\nmultiline(\n) -> Result<Vec<SqlStatement>, Error> {}\n\
                  /// fn documentation() -> Vec<SqlStatement> {}\n\
                  fn unrelated() -> Event {}\n";
    assert_eq!(
        event_statement_builder_names(source),
        BTreeSet::from(["restricted", "asynchronous", "multiline"])
    );
}
