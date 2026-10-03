use super::*;

#[test]
fn exec_optional_string_preserves_whitespace() {
    assert_eq!(opt_str(&json!({}), "name").unwrap(), None);
    assert_eq!(opt_str(&json!({"name": null}), "name").unwrap(), None);
    for value in ["  hello  ", " \t ", ""] {
        assert_eq!(
            opt_str(&json!({"name": value}), "name").unwrap(),
            Some(value.to_owned())
        );
    }
}

#[test]
fn exec_required_string_preserves_padding_and_refuses_blank() {
    for params in [
        json!({}),
        json!({"name": null}),
        json!({"name": ""}),
        json!({"name": " \t "}),
    ] {
        assert!(matches!(req_str(&params, "name"),
            Err(RuntimeError::InvalidInput(message)) if message == "name is required"));
    }
    assert_eq!(
        req_str(&json!({"name": "  hello  "}), "name").unwrap(),
        "  hello  "
    );
}

#[test]
fn exec_string_list_preserves_blanks_and_absence() {
    for params in [json!({}), json!({"names": null})] {
        assert_eq!(opt_str_list(&params, "names").unwrap(), None);
    }
    assert_eq!(
        opt_str_list(&json!({"names": []}), "names").unwrap(),
        Some(Vec::<String>::new())
    );
    assert_eq!(
        opt_str_list(&json!({"names": ["  hello  ", "", " \t "]}), "names").unwrap(),
        Some(vec![
            "  hello  ".to_owned(),
            "".to_owned(),
            " \t ".to_owned()
        ])
    );
    for params in [json!({"names": [1]}), json!({"names": "hello"})] {
        assert!(matches!(opt_str_list(&params, "names"),
            Err(RuntimeError::InvalidInput(message)) if message == "names must be an array of strings"));
    }
}

#[test]
fn exec_limit_accepts_zero_and_uses_non_negative_error() {
    assert_eq!(
        opt_limit(&json!({"limit": 0}), "limit", 20, 500).unwrap(),
        1
    );
    assert_eq!(opt_limit(&json!({}), "limit", 600, 500).unwrap(), 600);
    assert_eq!(
        opt_limit(&json!({"limit": null}), "limit", 20, 500).unwrap(),
        20
    );
    assert_eq!(
        opt_limit(&json!({"limit": u64::MAX}), "limit", 20, 500).unwrap(),
        500
    );
    for value in [json!(-1), json!(1.5), json!("1"), json!(false)] {
        assert!(
            matches!(opt_limit(&json!({"limit": value}), "limit", 20, 500),
            Err(RuntimeError::InvalidInput(message))
            if message == "limit must be a non-negative integer"),
            "exec limit refuses with non-negative integer message"
        );
    }
}
