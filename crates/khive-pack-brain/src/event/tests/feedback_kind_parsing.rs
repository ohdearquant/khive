#[test]
fn feedback_event_kind_from_signal_str_all_variants() {
    assert_eq!(
        FeedbackEventKind::from_signal_str("explicit_positive"),
        Some(FeedbackEventKind::ExplicitPositive)
    );
    assert_eq!(
        FeedbackEventKind::from_signal_str("explicit_negative"),
        Some(FeedbackEventKind::ExplicitNegative)
    );
    assert_eq!(
        FeedbackEventKind::from_signal_str("implicit_positive"),
        Some(FeedbackEventKind::ImplicitPositive)
    );
    assert_eq!(
        FeedbackEventKind::from_signal_str("implicit_negative"),
        Some(FeedbackEventKind::ImplicitNegative)
    );
    assert_eq!(
        FeedbackEventKind::from_signal_str("correction"),
        Some(FeedbackEventKind::Correction)
    );
}

#[test]
fn feedback_event_kind_from_signal_str_unknown_returns_none() {
    assert_eq!(FeedbackEventKind::from_signal_str("useful"), None);
    assert_eq!(FeedbackEventKind::from_signal_str("not_useful"), None);
    assert_eq!(FeedbackEventKind::from_signal_str(""), None);
    assert_eq!(FeedbackEventKind::from_signal_str("ExplicitPositive"), None);
}
