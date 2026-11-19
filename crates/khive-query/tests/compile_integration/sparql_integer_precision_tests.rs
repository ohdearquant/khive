mod sparql_integer_precision {
    use super::*;
    use khive_query::ast::{CompareOp, ConditionValue, PropertyRef};

    fn number_parameter<'a>(compiled: &'a CompiledQuery, alias: &str) -> &'a QueryValue {
        let marker = format!("json_extract({alias}.properties, '$.number') = ?");
        assert_eq!(compiled.sql.matches(&marker).count(), 1);
        let after = compiled
            .sql
            .split_once(&marker)
            .expect("number predicate")
            .1;
        let index: String = after.chars().take_while(char::is_ascii_digit).collect();
        let index: usize = index.parse().expect("bound parameter index");
        &compiled.params[index - 1]
    }

    #[test]
    fn signed_integer_spellings_keep_exact_values_through_public_parse_and_compile() {
        for (relation, subject, alias) in [
            ("extends", "a", "n0"),
            ("extends", "b", "n1"),
            ("extends+", "a", "s"),
            ("extends+", "b", "r"),
        ] {
            for (literal, expected) in [
                ("9007199254740993", 9_007_199_254_740_993_i64),
                ("-9007199254740993", -9_007_199_254_740_993_i64),
                ("9223372036854775807", i64::MAX),
                ("-9223372036854775808", i64::MIN),
                ("0", 0),
                ("-0", 0),
                ("00054", 54),
                ("-00054", -54),
            ] {
                let source = format!(
                    "SELECT ?a ?b WHERE {{ ?{subject} :number {literal} . ?a :{relation} ?b . }}"
                );
                for query in [
                    parse(QueryLanguage::Sparql, &source).expect("selected parser"),
                    parse_auto(&source).expect("automatic parser"),
                ] {
                    let conditions: Vec<_> = query.where_clause.conditions().collect();
                    assert_eq!(conditions.len(), 1);
                    assert_eq!(conditions[0].variable, subject);
                    assert_eq!(
                        conditions[0].property,
                        PropertyRef::JsonPath(vec!["number".into()])
                    );
                    assert_eq!(conditions[0].op, CompareOp::Eq);
                    assert_eq!(conditions[0].value, ConditionValue::Integer(expected));
                    let compiled = compile(&query, &scoped("precision-fixture")).expect("compile");
                    assert!(
                        matches!(number_parameter(&compiled, alias), QueryValue::Integer(n) if *n == expected),
                        "{source}: {:?}",
                        compiled.params
                    );
                }
            }
        }
    }

    #[test]
    fn decimal_and_out_of_i64_spellings_keep_the_existing_float_path() {
        // Compatibility witnesses, not a new numeric-range or decimal grammar policy.
        for (literal, expected) in [
            ("54.0", 54.0_f64),
            ("9007199254740993.0", 9_007_199_254_740_992.0),
            ("-0.0", -0.0),
            ("1.", 1.0),
            ("9007199254740993.", 9_007_199_254_740_992.0),
            ("-.5", -0.5),
            ("9223372036854775808", 9_223_372_036_854_775_808.0),
            ("-9223372036854775809", -9_223_372_036_854_775_808.0),
        ] {
            let source =
                format!("SELECT ?a ?b WHERE {{ ?a :number {literal} . ?a :extends ?b . }}");
            let query = parse(QueryLanguage::Sparql, &source).expect("existing float spelling");
            let condition = query
                .where_clause
                .conditions()
                .next()
                .expect("number condition");
            assert!(
                matches!(condition.value, ConditionValue::Number(n) if n.to_bits() == expected.to_bits())
            );
            let compiled = compile(&query, &opts()).expect("finite float compiles");
            assert!(
                matches!(number_parameter(&compiled, "n0"), QueryValue::Float(n) if n.to_bits() == expected.to_bits())
            );
        }
    }

    #[test]
    fn quoted_numbers_stay_text_and_unsupported_sign_exponent_forms_stay_errors() {
        let query = parse(
            QueryLanguage::Sparql,
            "SELECT ?a WHERE { ?a :number '9007199254740993' . ?a :extends ?b . }",
        )
        .expect("quoted number");
        assert_eq!(
            query.pattern.nodes().next().expect("start node").properties["number"],
            ConditionValue::String("9007199254740993".into())
        );
        let compiled = compile(&query, &opts()).expect("string compiles");
        assert!(
            matches!(number_parameter(&compiled, "n0"), QueryValue::Text(s) if s == "9007199254740993")
        );
        assert!(compiled.sql.contains("COLLATE NOCASE"));
        // `?a :number +1` reads as the recursive predicate `:number+` with the object 1, so the
        // predicate's own marker is written first and the second plus reaches the literal.
        for object in [":number+ +1", ":number 1e3", ":number 1E3", ":number -1e3"] {
            let source = format!("SELECT ?a WHERE {{ ?a {object} . ?a :extends ?b . }}");
            assert!(matches!(
                parse(QueryLanguage::Sparql, &source),
                Err(QueryError::Parse { .. })
            ));
        }
    }
}
