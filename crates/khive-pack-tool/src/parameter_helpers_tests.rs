use super::*;

#[test]
fn tool_optional_string_trims_and_refuses_blank() {
    assert_eq!(opt_str(&json!({}), "name").unwrap(), None);
    assert_eq!(opt_str(&json!({"name": null}), "name").unwrap(), None);
    let error = opt_str(&json!({"name": " \t "}), "name")
        .expect_err("tool opt_str refuses whitespace-only");
    assert!(matches!(error, RuntimeError::InvalidInput(message)
        if message == "name must be a non-empty string when provided"));
    assert!(matches!(opt_str(&json!({"name": ""}), "name"),
        Err(RuntimeError::InvalidInput(message))
        if message == "name must be a non-empty string when provided"));
    assert_eq!(
        opt_str(&json!({"name": "  hello  "}), "name").unwrap(),
        Some("hello".to_owned())
    );
}

#[test]
fn tool_required_string_trims_and_refuses_absence() {
    for params in [json!({}), json!({"name": null})] {
        assert!(matches!(req_str(&params, "name"),
            Err(RuntimeError::InvalidInput(message)) if message == "name is required"));
    }
    assert!(matches!(req_str(&json!({"name": " \t "}), "name"),
        Err(RuntimeError::InvalidInput(message))
        if message == "name must be a non-empty string when provided"));
    assert_eq!(
        req_str(&json!({"name": "  hello  "}), "name").unwrap(),
        "hello"
    );
}

#[test]
fn tool_string_list_trims_and_refuses_blank_items() {
    for params in [json!({}), json!({"names": null})] {
        assert_eq!(
            opt_str_list(&params, "names").unwrap(),
            Vec::<String>::new()
        );
    }
    assert_eq!(
        opt_str_list(&json!({"names": ["  hello  ", "world"]}), "names").unwrap(),
        vec!["hello".to_owned(), "world".to_owned()]
    );
    for params in [
        json!({"names": [" "]}),
        json!({"names": [""]}),
        json!({"names": [1]}),
    ] {
        assert!(matches!(opt_str_list(&params, "names"),
            Err(RuntimeError::InvalidInput(message))
            if message == "names must be an array of non-empty strings"));
    }
    assert!(matches!(opt_str_list(&json!({"names": "hello"}), "names"),
        Err(RuntimeError::InvalidInput(message)) if message == "names must be an array of strings"));
}

#[test]
fn tool_limit_accepts_zero_and_refuses_non_integer() {
    assert_eq!(opt_u32(&json!({"limit": 0}), "limit", 10, 50).unwrap(), 1);
    assert_eq!(opt_u32(&json!({}), "limit", 60, 50).unwrap(), 60);
    assert_eq!(
        opt_u32(&json!({"limit": null}), "limit", 10, 50).unwrap(),
        10
    );
    assert_eq!(
        opt_u32(&json!({"limit": u64::MAX}), "limit", 10, 50).unwrap(),
        50
    );
    for value in [json!(-1), json!(1.5), json!("1"), json!(false)] {
        assert!(matches!(opt_u32(&json!({"limit": value}), "limit", 10, 50),
            Err(RuntimeError::InvalidInput(message))
            if message == "limit must be a non-negative integer"));
    }
}
