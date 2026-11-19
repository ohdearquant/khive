#[tokio::test]
async fn poll_message_without_text_is_dropped() {
    let update = TelegramUpdate {
        update_id: 5,
        message: Some(TelegramMessage {
            message_id: 5,
            date: 1_700_000_000,
            chat: TelegramChat { id: 555 },
            from: Some(TelegramUser { id: 555 }),
            text: None,
        }),
    };
    let ch = TelegramChannel::with_connector(
        make_config(),
        Box::new(MockConnector::new(vec![vec![update]])),
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert!(
        envs.is_empty(),
        "a non-text update must be dropped, not errored"
    );
}

#[tokio::test]
async fn poll_update_without_message_is_dropped() {
    let update = TelegramUpdate {
        update_id: 6,
        message: None,
    };
    let ch = TelegramChannel::with_connector(
        make_config(),
        Box::new(MockConnector::new(vec![vec![update]])),
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert!(envs.is_empty());
}

#[test]
fn is_configured_true_with_nonempty_token() {
    let ch = TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(vec![])));
    assert!(ch.is_configured());
}

#[test]
fn strip_kind_prefix_removes_prefix() {
    assert_eq!(
        strip_kind_prefix("telegram:maintainer", "telegram"),
        "maintainer"
    );
    assert_eq!(strip_kind_prefix("maintainer", "telegram"), "maintainer");
}

#[test]
fn maintainer_slug_accessor() {
    let ch = TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(vec![])));
    assert_eq!(ch.maintainer_slug(), "maintainer");
}
