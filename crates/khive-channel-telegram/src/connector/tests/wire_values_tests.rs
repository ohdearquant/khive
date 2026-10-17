#[test]
fn api_url_embeds_token_and_method() {
    let connector = LiveTelegramConnector::new("123:ABC".to_string());
    assert_eq!(
        connector.api_url("sendMessage"),
        "https://api.telegram.org/bot123:ABC/sendMessage"
    );
}

#[test]
fn telegram_update_deserializes_text_message() {
    let raw = json!({
        "update_id": 42,
        "message": {
            "message_id": 7,
            "date": 1_700_000_000,
            "chat": { "id": 555 },
            "from": { "id": 42 },
            "text": "hello"
        }
    });
    let update: TelegramUpdate = serde_json::from_value(raw).unwrap();
    assert_eq!(update.update_id, 42);
    let message = update.message.unwrap();
    assert_eq!(message.chat.id, 555);
    assert_eq!(message.from.unwrap().id, 42);
    assert_eq!(message.text.as_deref(), Some("hello"));
}

#[test]
fn telegram_update_without_message_deserializes() {
    let raw = json!({ "update_id": 43 });
    let update: TelegramUpdate = serde_json::from_value(raw).unwrap();
    assert_eq!(update.update_id, 43);
    assert!(update.message.is_none());
}

#[test]
fn telegram_message_without_text_deserializes_none() {
    let raw = json!({
        "message_id": 8,
        "date": 1_700_000_000,
        "chat": { "id": 555 }
    });
    let message: TelegramMessage = serde_json::from_value(raw).unwrap();
    assert!(message.from.is_none());
    assert!(message.text.is_none());
}

#[test]
fn api_response_ok_false_carries_description() {
    let raw = json!({ "ok": false, "description": "Unauthorized" });
    let parsed: TelegramApiResponse<serde_json::Value> = serde_json::from_value(raw).unwrap();
    assert!(!parsed.ok);
    assert_eq!(parsed.description.as_deref(), Some("Unauthorized"));
}
