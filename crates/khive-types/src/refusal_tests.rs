use super::*;

#[test]
fn token_vocabulary_is_exact_and_append_only() {
    let tokens = RefusalReason::ALL.map(RefusalReason::as_str);
    assert_eq!(
        tokens,
        [
            "anonymous-actor",
            "expect-actor-mismatch",
            "gate-refusal",
            "strict-op-failure",
            "parse-error",
            "verb-refused",
            "policy-refusal",
        ]
    );
    for (reason, token) in RefusalReason::ALL.into_iter().zip(tokens) {
        assert_eq!(RefusalReason::from_token(token), Some(reason));
        assert_eq!(token.parse::<RefusalReason>().unwrap(), reason);
    }
    assert_eq!(RefusalReason::from_token("Gate-Refusal"), None);
    assert_eq!(RefusalReason::from_token("gate_refusal"), None);
    assert_eq!(RefusalReason::from_token("Policy-Refusal"), None);
    assert_eq!(RefusalReason::from_token("policy_refusal"), None);
}

#[cfg(feature = "serde")]
#[test]
fn serde_uses_the_machine_token() {
    for reason in RefusalReason::ALL {
        let encoded = serde_json::to_string(&reason).unwrap();
        assert_eq!(encoded, alloc::format!("\"{}\"", reason.as_str()));
        assert_eq!(
            serde_json::from_str::<RefusalReason>(&encoded).unwrap(),
            reason
        );
    }
}
