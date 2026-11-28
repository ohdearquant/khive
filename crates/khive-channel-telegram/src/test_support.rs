//! Opt-in fixture for coordinator tests; no live connector or copied offset algorithm.

use super::{TelegramChannel, TelegramChannelConfig, TelegramConnector, TelegramUpdate};
use crate::connector::{TelegramChat, TelegramMessage, TelegramUser};
use async_trait::async_trait;
use khive_channel::ChannelError;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

struct ReadyConnector {
    pages: Mutex<VecDeque<Vec<TelegramUpdate>>>,
    requested: Mutex<Vec<Option<i64>>>,
}

#[async_trait]
impl TelegramConnector for ReadyConnector {
    async fn send_message(&self, _: i64, _: &str) -> Result<(), ChannelError> {
        panic!("inbound timing fixture must not send");
    }

    async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>, ChannelError> {
        self.requested.lock().unwrap().push(offset);
        self.pages
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ChannelError::Transport("timing fixture has no remaining page".into()))
    }
}

/// Three immediately ready pages for actual `TelegramChannel` state ownership.
/// Poll/commit update 6, poll update 8, then let the coordinator select update 10.
pub struct TelegramTimingFixture {
    channel: Arc<TelegramChannel>,
    connector: Arc<ReadyConnector>,
}

impl TelegramTimingFixture {
    pub fn ready() -> Self {
        let pages = [6, 8, 10]
            .into_iter()
            .map(|update_id| {
                vec![TelegramUpdate {
                    update_id,
                    message: Some(TelegramMessage {
                        message_id: update_id,
                        date: 1_700_000_000,
                        chat: TelegramChat { id: 555 },
                        from: Some(TelegramUser { id: 555 }),
                        text: Some(format!("timing fixture update {update_id}")),
                    }),
                }]
            })
            .collect();
        let connector = Arc::new(ReadyConnector {
            pages: Mutex::new(pages),
            requested: Mutex::new(Vec::new()),
        });
        let config = TelegramChannelConfig {
            bot_token: "timing-fixture".into(),
            maintainer_chat_id: 555,
            authorized_sender_id: 555,
            maintainer_slug: "maintainer".into(),
            ingest_namespace: "local".into(),
        };
        let channel = Arc::new(TelegramChannel::with_connector(
            config,
            Box::new(connector.clone()),
        ));
        Self { channel, connector }
    }

    pub fn channel(&self) -> Arc<TelegramChannel> {
        self.channel.clone()
    }

    pub fn offsets(&self) -> (Option<i64>, Option<i64>) {
        (
            *self.channel.offset.lock().unwrap(),
            *self.channel.pending_offset.lock().unwrap(),
        )
    }

    pub fn requested_offsets(&self) -> Vec<Option<i64>> {
        self.connector.requested.lock().unwrap().clone()
    }

    pub fn remaining_pages(&self) -> usize {
        self.connector.pages.lock().unwrap().len()
    }
}

/// One released Bot API response for a real adapter/coordinator fixture.
/// JSON uses the actual TelegramUpdate deserializer, including sender filtering.
pub struct TelegramPollStep {
    response: Result<Vec<serde_json::Value>, ChannelError>,
    after_response: Option<Box<dyn FnOnce() + Send>>,
}

impl TelegramPollStep {
    pub fn updates(updates: Vec<serde_json::Value>) -> Self {
        Self {
            response: Ok(updates),
            after_response: None,
        }
    }

    pub fn failure(error: ChannelError) -> Self {
        Self {
            response: Err(error),
            after_response: None,
        }
    }

    /// Run after this response was selected, before it returns to the adapter.
    pub fn after_response(mut self, action: impl FnOnce() + Send + 'static) -> Self {
        self.after_response = Some(Box::new(action));
        self
    }
}

struct ScriptedState {
    steps: VecDeque<TelegramPollStep>,
    permits: usize,
    requested: Vec<Option<i64>>,
    dropped_pending: usize,
    waker: Option<std::task::Waker>,
    allow_outbound: bool,
    sent: Vec<(i64, String)>,
}

struct ScriptedConnector {
    state: Mutex<ScriptedState>,
}

struct PendingPoll<'a> {
    connector: &'a ScriptedConnector,
    completed: bool,
}

impl Drop for PendingPoll<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.connector.state.lock().unwrap().dropped_pending += 1;
        }
    }
}

#[async_trait]
impl TelegramConnector for ScriptedConnector {
    async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), ChannelError> {
        let mut state = self.state.lock().unwrap();
        assert!(
            state.allow_outbound,
            "inbound protocol fixture must not send"
        );
        state.sent.push((chat_id, text.to_string()));
        Ok(())
    }

    async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>, ChannelError> {
        self.state.lock().unwrap().requested.push(offset);
        let mut pending = PendingPoll {
            connector: self,
            completed: false,
        };
        let mut step = std::future::poll_fn(|cx| {
            let mut state = self.state.lock().unwrap();
            if state.permits > 0 && !state.steps.is_empty() {
                state.permits -= 1;
                std::task::Poll::Ready(state.steps.pop_front().unwrap())
            } else {
                state.waker = Some(cx.waker().clone());
                std::task::Poll::Pending
            }
        })
        .await;
        pending.completed = true;
        if let Some(action) = step.after_response.take() {
            action();
        }
        step.response.map(|updates| {
            updates
                .into_iter()
                .map(|update| {
                    serde_json::from_value(update).expect("valid Telegram fixture update")
                })
                .collect()
        })
    }
}

/// Recording, explicitly released transport with the production offset logic.
/// Uses only std synchronization so downstream test-support needs no Tokio dep.
pub struct TelegramScriptedFixture {
    channel: Arc<TelegramChannel>,
    connector: Arc<ScriptedConnector>,
}

impl TelegramScriptedFixture {
    pub fn new(slug: &str, steps: Vec<TelegramPollStep>) -> Self {
        let connector = Arc::new(ScriptedConnector {
            state: Mutex::new(ScriptedState {
                steps: steps.into(),
                permits: 0,
                requested: Vec::new(),
                dropped_pending: 0,
                waker: None,
                allow_outbound: false,
                sent: Vec::new(),
            }),
        });
        let config = TelegramChannelConfig {
            bot_token: "heartbeat-fixture".into(),
            maintainer_chat_id: 555,
            authorized_sender_id: 555,
            maintainer_slug: slug.into(),
            ingest_namespace: "local".into(),
        };
        let channel = Arc::new(TelegramChannel::with_connector(
            config,
            Box::new(connector.clone()),
        ));
        Self { channel, connector }
    }

    pub fn channel(&self) -> Arc<TelegramChannel> {
        self.channel.clone()
    }

    pub fn release_next(&self) {
        let waker = {
            let mut state = self.connector.state.lock().unwrap();
            assert!(!state.steps.is_empty(), "fixture has a response to release");
            state.permits += 1;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub fn requested_offsets(&self) -> Vec<Option<i64>> {
        self.connector.state.lock().unwrap().requested.clone()
    }

    pub fn dropped_pending(&self) -> usize {
        self.connector.state.lock().unwrap().dropped_pending
    }

    pub fn allow_outbound(&self) {
        self.connector.state.lock().unwrap().allow_outbound = true;
    }

    pub fn sent_messages(&self) -> Vec<(i64, String)> {
        self.connector.state.lock().unwrap().sent.clone()
    }

    pub fn offsets(&self) -> (Option<i64>, Option<i64>) {
        (
            *self.channel.offset.lock().unwrap(),
            *self.channel.pending_offset.lock().unwrap(),
        )
    }
}

#[cfg(test)]
#[test]
fn manually_configured_blank_slug_uses_channel_kind() {
    use khive_channel::Channel;

    for slug in ["", " \t\n", "\u{2003}"] {
        let fixture = TelegramScriptedFixture::new(slug, vec![]);
        assert_eq!(fixture.channel().slug(), "telegram");
    }
    for slug in ["maintainer-alpha", " maintainer-alpha "] {
        let fixture = TelegramScriptedFixture::new(slug, vec![]);
        assert_eq!(fixture.channel().slug(), slug);
    }
}
