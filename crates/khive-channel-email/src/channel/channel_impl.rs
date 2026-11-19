use super::*;

#[async_trait]
impl Channel for EmailChannel {
    fn kind(&self) -> &'static str {
        "email"
    }

    /// khive #606: the mailbox address distinguishes multiple configured
    /// email accounts (all `kind() == "email"`) so their channel health
    /// rows never collapse into one.
    fn slug(&self) -> String {
        self.config.mailbox.clone()
    }

    fn quarantine_retention_limit(&self) -> Option<usize> {
        Some(self.config.quarantine_max_retained)
    }

    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        let from = strip_kind_prefix(&envelope.from, "email");
        let to = strip_kind_prefix(&envelope.to, "email");
        let subject = envelope.subject.as_deref().unwrap_or("(no subject)");
        let thread_id = envelope.correlation_external_id.as_deref();
        let message_id = envelope.message_id.as_deref();
        let in_reply_to = envelope.in_reply_to.as_deref();
        let references = envelope.references.as_deref();
        let list_unsubscribe = envelope.list_unsubscribe.as_deref();

        debug!(from, to, subject, "email send");
        self.smtp
            .send(
                from,
                to,
                subject,
                &envelope.content,
                thread_id,
                message_id,
                in_reply_to,
                references,
                list_unsubscribe,
            )
            .await
    }

    async fn poll(&self, since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        let raw = self.imap.fetch_since(since, IMAP_PAGE_LIMIT).await?;
        let inbound = self.load_inbound_suppression().await?;
        let (envelopes, pending_unsubscribes) = self.disposition(raw, inbound.as_ref()).await?;
        self.record_unsubscribes(&pending_unsubscribes).await?;
        Ok(envelopes)
    }

    /// Checkpointed poll (issue #449): resolves the durable IMAP
    /// UIDVALIDITY/high-water progress from `checkpoint`, fetches strictly
    /// above it (or bootstraps by date on no/mismatched/reset progress), and
    /// returns a checkpoint candidate for the poll coordinator to persist
    /// only after every envelope is durably ingested.
    async fn poll_page(
        &self,
        since: DateTime<Utc>,
        checkpoint: Option<&StoredChannelCheckpoint>,
    ) -> Result<ChannelPollPage, ChannelError> {
        let (progress, committed_at) = self.decode_progress(checkpoint)?;
        let recovery_since = match committed_at {
            Some(committed_at) => since.min(committed_at),
            None => since,
        };

        let page = self
            .imap
            .fetch_page(recovery_since, IMAP_PAGE_LIMIT, progress)
            .await?;
        let inbound = self.load_inbound_suppression().await?;
        let (envelopes, pending_unsubscribes) =
            self.disposition(page.emails, inbound.as_ref()).await?;
        // §5.4 entry point 1's machine record must commit before the cursor advances past the UID:
        // a store failure here surfaces as a channel error, so the poll coordinator holds progress
        // and the opt-out is not silently dropped.
        self.record_unsubscribes(&pending_unsubscribes).await?;
        let next_checkpoint = if page.next_progress == progress {
            None
        } else {
            Some(self.encode_checkpoint(page.next_progress))
        };

        Ok(ChannelPollPage {
            envelopes,
            next_checkpoint,
        })
    }
}

pub(super) fn screening_placeholder(disposition: &str, outcome: &ScreeningOutcome) -> String {
    format!(
        "(khive: external email body {disposition}; screening labels: {})",
        outcome.rule_ids.join(", ")
    )
}

/// Strip a `"kind:"` prefix from an address string.
pub(super) fn strip_kind_prefix<'a>(addr: &'a str, kind: &str) -> &'a str {
    let prefix = format!("{kind}:");
    addr.strip_prefix(prefix.as_str()).unwrap_or(addr)
}
