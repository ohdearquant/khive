//! Public parser regressions for hop bounds on SPARQL property predicates.

use khive_query::ast::{CompareOp, ConditionValue, GqlQuery, PropertyRef, QueryValue};
use khive_query::{compile, parse, parse_auto, CompileOptions, QueryError, QueryLanguage};

fn parse_sparql(input: &str, auto: bool) -> Result<GqlQuery, QueryError> {
    if auto {
        parse_auto(input)
    } else {
        parse(QueryLanguage::Sparql, input)
    }
}

#[test]
fn nondefault_hop_bounds_on_every_literal_object_are_rejected() {
    for object in ["'literal'", "42", "1.5", ":literal"] {
        for bounds in ["+", "{1,2}", "{2,2}", "{0,1}", "{2,1}", "{1,11}"] {
            for subject in ["a", "b"] {
                let input = format!(
                    "SELECT ?a ?b WHERE {{ ?a :extends ?b . ?{subject} :value{bounds} {object} . }}"
                );
                for auto in [false, true] {
                    let error = parse_sparql(&input, auto)
                        .expect_err("literal path bounds must not disappear");
                    assert!(
                        matches!(&error, QueryError::Unsupported(message) if message == "SPARQL property predicates require one-hop bounds (1,1); use a variable object for path traversal"),
                        "{input}: {error:?}"
                    );
                }
            }
        }
    }
    // Whitespace does not make this a signed number: the predicate consumes '+'.
    for input in [
        "SELECT ?a WHERE { ?a :extends ?b . ?a :value +1 . }",
        "SELECT ?a WHERE { ?a :extends ?b . ?a :value+1 . }",
    ] {
        for auto in [false, true] {
            let error = parse_sparql(input, auto).expect_err("plus remains a path marker");
            assert!(
                matches!(error, QueryError::Unsupported(_)),
                "{input}: {error:?}"
            );
        }
    }
}

#[test]
fn implicit_and_explicit_one_hop_property_constraints_remain_bound() {
    for (object, expected) in [
        ("'literal'", ConditionValue::String("literal".into())),
        ("1.5", ConditionValue::Number(1.5)),
        (":literal", ConditionValue::String("literal".into())),
    ] {
        for (subject, alias) in [("a", "n0"), ("b", "n1")] {
            for bounds in ["", "{1,1}"] {
                let input = format!(
                    "SELECT ?a ?b WHERE {{ ?a :extends ?b . ?{subject} :value{bounds} {object} . }}"
                );
                for auto in [false, true] {
                    let query =
                        parse_sparql(&input, auto).expect("one-hop property remains supported");
                    if matches!(&expected, ConditionValue::Number(_)) {
                        let conditions: Vec<_> = query.where_clause.conditions().collect();
                        assert_eq!(conditions.len(), 1);
                        assert_eq!(conditions[0].variable, subject);
                        assert_eq!(
                            conditions[0].property,
                            PropertyRef::JsonPath(vec!["value".into()])
                        );
                        assert_eq!(conditions[0].op, CompareOp::Eq);
                        assert_eq!(conditions[0].value, expected);
                    } else {
                        let node = query
                            .pattern
                            .nodes()
                            .find(|node| node.variable.as_deref() == Some(subject))
                            .unwrap();
                        assert_eq!(node.properties.get("value"), Some(&expected));
                    }
                    let compiled = compile(&query, &CompileOptions::default())
                        .expect("one-hop property compiles");
                    let needle = format!("json_extract({alias}.properties, '$.value') = ?");
                    assert_eq!(
                        compiled.sql.matches(&needle).count(),
                        1,
                        "{input}: {}",
                        compiled.sql
                    );
                    let suffix = compiled.sql.split_once(&needle).unwrap().1;
                    let index: usize = suffix
                        .chars()
                        .take_while(char::is_ascii_digit)
                        .collect::<String>()
                        .parse()
                        .unwrap();
                    assert!(
                        match (&compiled.params[index - 1], &expected) {
                            (QueryValue::Text(actual), ConditionValue::String(expected)) =>
                                actual == expected,
                            (QueryValue::Float(actual), ConditionValue::Number(expected)) =>
                                actual == expected,
                            _ => false,
                        },
                        "{input}: wrong property parameter"
                    );
                }
            }
        }
    }
}

#[test]
fn variable_objects_keep_their_hop_bounds() {
    for (bounds, min_hops, max_hops) in [
        ("", 1, 1),
        ("{1,1}", 1, 1),
        ("+", 1, 5),
        ("{1,3}", 1, 3),
        ("{2,2}", 2, 2),
    ] {
        let input = format!("SELECT ?a ?b WHERE {{ ?a :extends{bounds} ?b . }}");
        for auto in [false, true] {
            let query = parse_sparql(&input, auto).expect("variable path remains supported");
            let edges: Vec<_> = query.pattern.edges().collect();
            assert_eq!(edges.len(), 1);
            assert_eq!((edges[0].min_hops, edges[0].max_hops), (min_hops, max_hops));
            compile(&query, &CompileOptions::default()).expect("variable path still compiles");
        }
    }
}
