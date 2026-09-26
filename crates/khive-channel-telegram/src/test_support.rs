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
