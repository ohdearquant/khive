#[test]
fn bareword_value_names_the_quoting_fix() {
    // `id=abc` is the most common invalid-value mistake: a string typed
    // without its surrounding quotes. The message must say it is a
    // bareword, that string values need double quotes, and — since the
    // argument name is known here — show the exact corrected call.
    let err = parse_request("get(id=abc)").unwrap_err();
    assert!(matches!(err, DslError::InvalidValue { .. }));
    let msg = err.to_string();
    assert!(
        msg.contains("bareword") && msg.contains("double-quoted"),
        "must name the bareword and the quoting fix, got: {msg}"
    );
    assert!(
        msg.contains(r#"id="abc""#),
        "must show the corrected call, got: {msg}"
    );
}

#[test]
fn bareword_value_in_object_literal_uses_the_key_as_hint() {
    // Same mistake, inside a JSON-object-shaped argument value: the key is
    // known there too, so the reconstructed call still names it.
    let err = parse_request(r#"update(patch={"name": abc})"#).unwrap_err();
    assert!(matches!(err, DslError::InvalidValue { .. }));
    let msg = err.to_string();
    assert!(
        msg.contains(r#"name="abc""#),
        "must show the corrected key:value, got: {msg}"
    );
}

#[test]
fn bareword_value_in_array_element_has_no_reconstruction_to_guess() {
    // Same mistake, as a bare array element: there is no argument name to
    // anchor a reconstruction to, so the message must describe the fix
    // without inventing a call it cannot verify.
    let err = parse_request("get(id=[abc])").unwrap_err();
    assert!(matches!(err, DslError::InvalidValue { .. }));
    let msg = err.to_string();
    assert!(
        msg.contains("bareword") && msg.contains("double-quoted"),
        "must still name the bareword mistake, got: {msg}"
    );
}

#[test]
fn invalid_value_byte_and_message_no_longer_disagree() {
    // Before this fix, a non-bareword invalid value (e.g. `1.2.3`) leaked
    // serde_json's own "at line 1 column N" — always relative to that
    // single value's isolated slice, so it disagreed with the DSL-absolute
    // "at byte N" this crate reports for the same failure. The
    // descriptive part of the message stays; the contradicting position
    // clause must be gone.
    let err = parse_request("get(id=1.2.3)").unwrap_err();
    assert!(matches!(err, DslError::InvalidValue { .. }));
    let msg = err.to_string();
    assert!(
        msg.starts_with("at byte 7:"),
        "expected the DSL-absolute position, got: {msg}"
    );
    assert!(
        !msg.contains("line 1 column"),
        "must not leak serde's own, disagreeing position, got: {msg}"
    );
    assert!(
        msg.contains("trailing characters"),
        "must keep serde's descriptive text, got: {msg}"
    );
}

#[test]
fn json_request_form_keeps_its_own_serde_position() {
    // The JSON request form (an object/array of objects sent as `ops`) is
    // genuinely JSON end to end — its serde `line`/`column` IS the real
    // location of the problem, so it must NOT be stripped the way the
    // function-call form's per-value serde fragment is.
    let err = parse_request(r#"{"tool": "get", "args": {"id": }}"#).unwrap_err();
    assert!(matches!(err, DslError::InvalidJson { .. }));
    let msg = err.to_string();
    assert!(
        msg.contains("line 1 column"),
        "JSON form's own serde position is meaningful and must be kept, got: {msg}"
    );
}
