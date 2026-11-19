use super::*;

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn external_list_admission_requires_its_own_available_footer() {
    for reason in [
        "KHIVE_EMAIL_LIST_FOOTER_FILE is not set",
        "KHIVE_EMAIL_LIST_FOOTER_FILE could not be read",
        "KHIVE_EMAIL_LIST_FOOTER_FILE does not contain {unsubscribe}",
    ] {
        let policy = external_policy(&["actor:sender"], 5);
        let policy = if reason == "KHIVE_EMAIL_LIST_FOOTER_FILE is not set" {
            policy
        } else {
            policy.with_list_footer_for_tests(Err(reason))
        };
        assert_eq!(policy.list_footer(), Some(Err(reason)));
        let runtime = runtime().with_outbound_email_policy(policy);
        let token = runtime.authorize(Namespace::local()).unwrap();
        let signal = InboxSignal::new();
        let before = snapshot(&runtime).await;
        let mut request = external_request("external@example.net", "list-unavailable");
        request["tags"] = json!(["traffic:list"]);
        external_denied(
            send(&runtime, &signal, &token, request).await.unwrap_err(),
            "comm.send",
            reason,
        );
        assert_eq!(snapshot(&runtime).await, before);
        assert_eq!(signal.snapshot(), 0);

        // The separate list-footer failure does not change an untagged external send.
        send(
            &runtime,
            &signal,
            &token,
            external_request("external@example.net", "untagged-default-footer"),
        )
        .await
        .unwrap();
        assert_eq!(signal.snapshot(), 1);

        // A tagged policy-recipient send still does not need an external footer.
        send(
            &runtime,
            &signal,
            &token,
            json!({
                "to": "email:allowed@example.com",
                "content": "policy recipient",
                "tags": ["traffic:list"]
            }),
        )
        .await
        .unwrap();
        assert_eq!(signal.snapshot(), 2);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn external_list_admission_uses_the_list_footer_without_requiring_the_default() {
    let policy = external_policy(&["actor:sender"], 5)
        .with_list_footer_for_tests(Ok("List notice: {unsubscribe}\n"))
        .with_external_footer_for_tests(Err("KHIVE_EMAIL_EXTERNAL_FOOTER_FILE is not set"));
    let runtime = runtime().with_outbound_email_policy(policy);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let signal = InboxSignal::new();
    let mut request = external_request("external@example.net", "list-available");
    request["tags"] = json!(["topic:news", "traffic:list"]);
    let sent = send(&runtime, &signal, &token, request).await.unwrap();
    assert_eq!(sent["replayed"], false);
    assert_eq!(signal.snapshot(), 1);

    let before = snapshot(&runtime).await;
    external_denied(
        send(
            &runtime,
            &signal,
            &token,
            external_request("external@example.net", "untagged-default-unavailable"),
        )
        .await
        .unwrap_err(),
        "comm.send",
        "KHIVE_EMAIL_EXTERNAL_FOOTER_FILE is not set",
    );
    assert_eq!(snapshot(&runtime).await, before);
    assert_eq!(signal.snapshot(), 1);
}
