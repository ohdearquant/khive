use khive_query::ast::{CompareOp, ConditionValue, PropertyRef};
use khive_query::{
    compile, parse, parse_auto, CompileOptions, CompiledQuery, QueryError, QueryLanguage,
    QueryValue,
};

fn query(relation: &str, endpoint: &str, token: &str) -> String {
    format!("SELECT ?a ?b WHERE {{ ?a :{relation} ?b . ?{endpoint} :score {token} . }} LIMIT 7")
}

fn property_parameter<'a>(
    compiled: &'a CompiledQuery,
    relation: &str,
    endpoint: &str,
) -> &'a QueryValue {
    let alias = match (relation, endpoint) {
        ("extends", "a") => "n0",
        ("extends", "b") => "n1",
        ("extends+", "a") => "s",
        ("extends+", "b") => "r",
        _ => panic!("unexpected fixture path"),
    };
    assert_eq!(
        compiled.sql.contains("WITH RECURSIVE"),
        relation == "extends+"
    );
    let marker = format!("json_extract({alias}.properties, '$.score') = ?");
    assert_eq!(compiled.sql.matches(&marker).count(), 1, "{}", compiled.sql);
    let (_, after) = compiled.sql.split_once(&marker).unwrap();
    let digits: String = after.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    let parameter: usize = digits.parse().expect("numbered property parameter");
    compiled
        .params
        .get(parameter.checked_sub(1).expect("one-based parameter"))
        .expect("property parameter is bound")
}

#[test]
fn nonfinite_property_literals_fail_on_both_public_parser_paths() {
    let huge = "9".repeat(400);
    let tokens = [
        huge.clone(),
        format!("-{huge}"),
        format!("{huge}.0"),
        format!("-{huge}.0"),
    ];
    for relation in ["extends", "extends+"] {
        for endpoint in ["a", "b"] {
            for token in &tokens {
                let input = query(relation, endpoint, token);
                for (entry, result) in [
                    ("explicit", parse(QueryLanguage::Sparql, &input)),
                    ("automatic", parse_auto(&input)),
                ] {
                    let error = result.expect_err("overflow must fail while parsing");
                    assert!(
                        matches!(error, QueryError::Parse { ref message, .. } if message.contains("not finite")),
                        "{entry} {relation} {endpoint}: {error:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn finite_fallback_values_keep_their_ast_and_property_parameter() {
    let cases = [
        ("9223372036854775808".to_owned(), 9223372036854775808.0_f64),
        (
            "-9223372036854775809".to_owned(),
            -9223372036854775808.0_f64,
        ),
        (format!("1{}", "0".repeat(300)), 1e300_f64),
        ("2.5".to_owned(), 2.5_f64),
        ("1.".to_owned(), 1.0_f64),
        ("-0.0".to_owned(), -0.0_f64),
    ];
    let options = CompileOptions {
        scopes: vec!["finite-control".to_owned()],
        ..CompileOptions::default()
    };
    for relation in ["extends", "extends+"] {
        for endpoint in ["a", "b"] {
            for (token, expected) in &cases {
                let input = query(relation, endpoint, token);
                for result in [parse(QueryLanguage::Sparql, &input), parse_auto(&input)] {
                    let ast = result.expect("finite fallback remains accepted");
                    assert_eq!(ast.limit, Some(7));
                    let conditions: Vec<_> = ast.where_clause.conditions().collect();
                    assert_eq!(conditions.len(), 1);
                    let condition = conditions[0];
                    assert_eq!(condition.variable, endpoint);
                    assert_eq!(
                        condition.property,
                        PropertyRef::JsonPath(vec!["score".to_owned()])
                    );
                    assert_eq!(condition.op, CompareOp::Eq);
                    match &condition.value {
                        ConditionValue::Number(actual) => {
                            assert_eq!(actual.to_bits(), expected.to_bits())
                        }
                        other => panic!("expected floating fallback, got {other:?}"),
                    }
                    let compiled = compile(&ast, &options).expect("finite AST compiles");
                    match property_parameter(&compiled, relation, endpoint) {
                        QueryValue::Float(actual) => {
                            assert_eq!(actual.to_bits(), expected.to_bits())
                        }
                        other => panic!("expected floating property parameter, got {other:?}"),
                    }
                }
            }
        }
    }
}

#[test]
fn quoted_overflow_spellings_remain_text_parameters() {
    let huge = "9".repeat(400);
    let texts = [
        huge.clone(),
        format!("-{huge}"),
        format!("{huge}.0"),
        format!("-{huge}.0"),
    ];
    let options = CompileOptions {
        scopes: vec!["quoted-control".to_owned()],
        ..CompileOptions::default()
    };
    for relation in ["extends", "extends+"] {
        for endpoint in ["a", "b"] {
            for text in &texts {
                let input = query(relation, endpoint, &format!("\"{text}\""));
                for result in [parse(QueryLanguage::Sparql, &input), parse_auto(&input)] {
                    let ast = result.expect("quoted digits remain text");
                    assert!(ast.where_clause.is_true());
                    let node = ast
                        .pattern
                        .nodes()
                        .find(|node| node.variable.as_deref() == Some(endpoint))
                        .unwrap();
                    assert!(
                        matches!(node.properties.get("score"), Some(ConditionValue::String(value)) if value == text)
                    );
                    let compiled = compile(&ast, &options).expect("text AST compiles");
                    match property_parameter(&compiled, relation, endpoint) {
                        QueryValue::Text(actual) => assert_eq!(actual, text),
                        other => panic!("expected text property parameter, got {other:?}"),
                    }
                }
            }
        }
    }
}
