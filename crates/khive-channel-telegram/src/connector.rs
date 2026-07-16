//! Telegram Bot API connector: a thin, injectable seam over `sendMessage` /
//! `getUpdates` so `TelegramChannel` can be unit-tested against a mock
//! implementation (no live network calls in tests), mirroring the
//! `SmtpConnector`/`ImapConnector` split in `khive-channel-email`.
//!
//! Plain HTTPS + JSON only — no Telegram SDK (ADR-056 Amendment 2026-07-05).

use async_trait::async_trait;
use khive_channel::ChannelError;
use serde::Deserialize;
use serde_json::json;

/// One `getUpdates` result entry.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TelegramUpdate {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<TelegramMessage>,
}

/// The `message` field of a Telegram update. Only the fields this adapter
/// needs are modeled; unknown fields are ignored by serde's default
/// (non-`deny_unknown_fields`) behavior.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TelegramMessage {
    #[allow(dead_code)]
    pub message_id: i64,
    /// Unix timestamp (seconds) the message was sent.
    pub date: i64,
    pub chat: TelegramChat,
    /// Absent for non-text messages (media, stickers, etc. — out of scope
    /// for v1, ADR-056 "Out of scope (v1)").
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TelegramChat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramApiResponse<T> {
    ok: bool,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
}

/// Injectable seam for the two Bot API methods this adapter uses.
#[async_trait]
pub(crate) trait TelegramConnector: Send + Sync {
    async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), ChannelError>;
    async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>, ChannelError>;
}

/// A [`TelegramConnector`] implementation shared through an `Arc`, so tests
/// can retain an observable handle to a mock connector after handing a copy
/// to [`crate::channel::TelegramChannel`] (which owns a `Box<dyn
/// TelegramConnector>`).
#[async_trait]
impl<T: TelegramConnector + ?Sized> TelegramConnector for std::sync::Arc<T> {
    async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), ChannelError> {
        (**self).send_message(chat_id, text).await
    }

    async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>, ChannelError> {
        (**self).get_updates(offset).await
    }
}

/// Default Bot API host. Overridable only in tests via
/// [`LiveTelegramConnector::with_base_url`], so production always targets the
/// real Telegram host.
const DEFAULT_API_BASE: &str = "https://api.telegram.org";

/// `getUpdates` long-poll timeout, in seconds, sent to the Bot API (ADR-056
/// Amendment 2026-07-05 requires long polling, not short polling).
const LONG_POLL_TIMEOUT_SECS: i64 = 25;

/// HTTP client request timeout. Must exceed [`LONG_POLL_TIMEOUT_SECS`] so a
/// `getUpdates` call that legitimately blocks server-side for the full
/// long-poll window is not cut off by the client itself.
const HTTP_CLIENT_TIMEOUT_SECS: u64 = 35;

/// Live Bot API connector: plain HTTPS POST + JSON, reusing the workspace
/// `reqwest` client (rustls-tls + json), same as `khive-channel-email`'s
/// OAuth token fetch. No Telegram SDK.
pub(crate) struct LiveTelegramConnector {
    http: reqwest::Client,
    bot_token: String,
    api_base: String,
}

impl LiveTelegramConnector {
    pub fn new(bot_token: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(HTTP_CLIENT_TIMEOUT_SECS))
            .build()
            .expect("reqwest client with static, valid config must build");
        Self {
            http,
            bot_token,
            api_base: DEFAULT_API_BASE.to_string(),
        }
    }

    /// Same as [`Self::new`] but pointed at a caller-supplied base URL
    /// instead of the real Bot API host. Test-only seam so connector error
    /// paths can be exercised against a local socket without ever reaching
    /// `api.telegram.org`.
    #[cfg(test)]
    fn with_base_url(bot_token: String, api_base: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(HTTP_CLIENT_TIMEOUT_SECS))
            .build()
            .expect("reqwest client with static, valid config must build");
        Self {
            http,
            bot_token,
            api_base,
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.api_base, self.bot_token)
    }
}

#[async_trait]
impl TelegramConnector for LiveTelegramConnector {
    async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), ChannelError> {
        let resp = self
            .http
            .post(self.api_url("sendMessage"))
            .json(&json!({ "chat_id": chat_id, "text": text }))
            .send()
            .await
            .map_err(|e| {
                ChannelError::Transport(format!("sendMessage request failed: {}", e.without_url()))
            })?;

        let status = resp.status();
        let parsed: TelegramApiResponse<serde_json::Value> = resp.json().await.map_err(|e| {
            ChannelError::Transport(format!(
                "sendMessage: malformed response: {}",
                e.without_url()
            ))
        })?;

        if !status.is_success() || !parsed.ok {
            return Err(ChannelError::Transport(format!(
                "sendMessage failed: status={status}, description={:?}",
                parsed.description
            )));
        }
        Ok(())
    }

    async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>, ChannelError> {
        let mut body = json!({ "timeout": LONG_POLL_TIMEOUT_SECS });
        if let Some(off) = offset {
            body["offset"] = json!(off);
        }

        let resp = self
            .http
            .post(self.api_url("getUpdates"))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                ChannelError::Transport(format!("getUpdates request failed: {}", e.without_url()))
            })?;

        let status = resp.status();
        let parsed: TelegramApiResponse<Vec<TelegramUpdate>> = resp.json().await.map_err(|e| {
            ChannelError::Transport(format!(
                "getUpdates: malformed response: {}",
                e.without_url()
            ))
        })?;

        if !status.is_success() || !parsed.ok {
            return Err(ChannelError::Transport(format!(
                "getUpdates failed: status={status}, description={:?}",
                parsed.description
            )));
        }
        Ok(parsed.result.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                "text": "hello"
            }
        });
        let update: TelegramUpdate = serde_json::from_value(raw).unwrap();
        assert_eq!(update.update_id, 42);
        let message = update.message.unwrap();
        assert_eq!(message.chat.id, 555);
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
        assert!(message.text.is_none());
    }

    #[test]
    fn api_response_ok_false_carries_description() {
        let raw = json!({ "ok": false, "description": "Unauthorized" });
        let parsed: TelegramApiResponse<serde_json::Value> = serde_json::from_value(raw).unwrap();
        assert!(!parsed.ok);
        assert_eq!(parsed.description.as_deref(), Some("Unauthorized"));
    }

    const SENTINEL_TOKEN: &str = "SENTINEL-TOKEN-DO-NOT-LEAK-abcdef123456";

    /// A local address nothing is listening on: the bound listener is
    /// dropped immediately, so a subsequent connect attempt gets a prompt
    /// connection-refused instead of hanging.
    fn refused_port_base_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}")
    }

    /// A minimal one-shot raw-socket HTTP server: accepts a single
    /// connection, drains the request, and writes back a fixed response
    /// body with `200 OK` and a matching `Content-Length`. No HTTP
    /// framework dependency needed for these connector-level tests.
    fn spawn_one_shot_server(response_body: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    /// Same as [`spawn_one_shot_server`], but also captures the request
    /// body it received back to the caller via a channel, so the test can
    /// assert on the JSON this connector actually sent.
    fn spawn_capturing_server(
        response_body: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = request
                    .split("\r\n\r\n")
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let _ = tx.send(body);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn send_message_request_failure_does_not_leak_token() {
        let connector = LiveTelegramConnector::with_base_url(
            SENTINEL_TOKEN.to_string(),
            refused_port_base_url(),
        );
        let err = connector.send_message(555, "hi").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains(SENTINEL_TOKEN),
            "error must not leak the bot token: {msg}"
        );
    }

    #[tokio::test]
    async fn send_message_decode_failure_does_not_leak_token() {
        let base_url = spawn_one_shot_server("not valid json");
        let connector = LiveTelegramConnector::with_base_url(SENTINEL_TOKEN.to_string(), base_url);
        let err = connector.send_message(555, "hi").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains(SENTINEL_TOKEN),
            "error must not leak the bot token: {msg}"
        );
    }

    #[tokio::test]
    async fn get_updates_request_failure_does_not_leak_token() {
        let connector = LiveTelegramConnector::with_base_url(
            SENTINEL_TOKEN.to_string(),
            refused_port_base_url(),
        );
        let err = connector.get_updates(None).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains(SENTINEL_TOKEN),
            "error must not leak the bot token: {msg}"
        );
    }

    #[tokio::test]
    async fn get_updates_decode_failure_does_not_leak_token() {
        let base_url = spawn_one_shot_server("not valid json");
        let connector = LiveTelegramConnector::with_base_url(SENTINEL_TOKEN.to_string(), base_url);
        let err = connector.get_updates(None).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains(SENTINEL_TOKEN),
            "error must not leak the bot token: {msg}"
        );
    }

    #[tokio::test]
    async fn get_updates_sends_positive_long_poll_timeout_and_offset() {
        let (base_url, request_rx) = spawn_capturing_server(r#"{"ok":true,"result":[]}"#);
        let connector = LiveTelegramConnector::with_base_url("token".to_string(), base_url);

        let updates = connector.get_updates(Some(42)).await.unwrap();
        assert!(updates.is_empty());

        let captured_body = request_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("server must capture a request body");
        let parsed: serde_json::Value = serde_json::from_str(&captured_body).unwrap();
        assert_eq!(parsed["offset"], 42);
        let timeout = parsed["timeout"]
            .as_i64()
            .expect("timeout field must be present and numeric");
        assert!(
            timeout > 0,
            "getUpdates must use long polling, not timeout=0"
        );
    }
}
