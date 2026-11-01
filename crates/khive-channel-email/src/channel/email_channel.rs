use super::*;

impl EmailChannel {
    /// Build from environment variables. Fails if any required env var is absent.
    pub fn from_env() -> Result<Self, ChannelError> {
        let config = EmailChannelConfig::from_env()?;

        let (smtp, imap) = match &config.auth {
            EmailAuth::Basic { password } => {
                let smtp = SmtpSender::new(
                    &config.smtp_host,
                    config.smtp_port,
                    &config.username,
                    password,
                );
                let imap = ImapFetcher::new_with_limits(
                    &config.imap_host,
                    config.imap_port,
                    &config.username,
                    password,
                    config.imap_max_message_bytes,
                    config.imap_max_page_bytes,
                );
                (smtp, imap)
            }
            EmailAuth::OAuth {
                tenant_id,
                client_id,
                client_secret,
            } => {
                let token_provider = Arc::new(TokenProvider::new(
                    tenant_id.clone(),
                    client_id.clone(),
                    client_secret.clone(),
                ));
                let smtp = SmtpSender::new_oauth(
                    &config.smtp_host,
                    config.smtp_port,
                    &config.mailbox,
                    Arc::clone(&token_provider),
                );
                let imap = ImapFetcher::new_oauth_with_limits(
                    &config.imap_host,
                    config.imap_port,
                    &config.mailbox,
                    Arc::clone(&token_provider),
                    config.imap_max_message_bytes,
                    config.imap_max_page_bytes,
                );
                (smtp, imap)
            }
        };

        Ok(Self {
            config,
            smtp,
            imap,
            external_host: None,
            external_inbound: None,
        })
    }

    /// Wire the host that supplies the external-email services the channel cannot own.
    ///
    /// The channel otherwise holds no store or policy handle, so the daemon start-up calls this
    /// with the same comm-routed runtime it already builds for the outbox loop.
    pub fn with_external_host(mut self, host: Arc<dyn ExternalEmailHost>) -> Self {
        self.external_host = Some(host);
        self
    }

    /// Attach the boot-resolved external recipient routes and fallback actor.
    pub fn with_external_inbound_config(mut self, config: ExternalInboundConfig) -> Self {
        self.external_inbound = Some(config);
        self
    }

    /// Whether this channel admits authenticated mail from senders other than the maintainer as
    /// external inbound: a host is wired that reports external inbound enabled, and inbound
    /// routing is attached (ADR-200 sections 7 and 8.2).
    pub fn external_inbound_enabled(&self) -> bool {
        self.external_inbound.is_some()
            && self
                .external_host
                .as_deref()
                .is_some_and(|host| host.external_inbound_enabled())
    }

    /// The configured mailbox address (e.g. `mailbox@example.com`).
    ///
    /// Used by the outbox delivery loop to derive the sender address and
    /// the domain component of the RFC 822 Message-ID header.
    pub fn mailbox(&self) -> &str {
        &self.config.mailbox
    }

    /// The configured maintainer address as a string.
    ///
    /// Used by the outbox delivery loop to build the default allowlist when
    /// `KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS` is not set.
    pub fn maintainer_address(&self) -> &str {
        // Primary maintainer: from_env guarantees `maintainer_addresses` is non-empty.
        self.config.maintainer_addresses[0].as_str()
    }

    /// Build from pre-constructed connectors (for testing).
    #[cfg(test)]
    pub(crate) fn with_connectors(
        config: EmailChannelConfig,
        smtp: SmtpSender,
        imap: ImapFetcher,
    ) -> Self {
        Self {
            config,
            smtp,
            imap,
            external_host: None,
            external_inbound: None,
        }
    }

    /// Validate that the message sender is authorized.
    ///
    /// Rules:
    /// - `from_addrs` must contain exactly one entry (multi-From is rejected).
    /// - That single From address must match one of the authorized maintainers
    ///   (Gmail-aware: dots/`+tag`/`googlemail.com` are insignificant for Gmail).
    /// - If `sender_addr` is present, it must also match an authorized maintainer.
    ///
    /// Returns `Err(ChannelError::UnauthorizedSender)` on any violation.
    /// Error messages intentionally omit the actual addresses to avoid leaking them to logs.
    pub(super) fn check_sender(
        &self,
        from_addrs: &[String],
        sender_addr: Option<&str>,
    ) -> Result<(), ChannelError> {
        // Exactly one From address is required. Zero or more-than-one is an unauthorized state.
        if from_addrs.len() != 1 {
            return Err(ChannelError::UnauthorizedSender(format!(
                "expected exactly 1 From address, got {}",
                from_addrs.len()
            )));
        }
        let from = MailAddress::parse(&from_addrs[0]).ok_or_else(|| {
            ChannelError::UnauthorizedSender("From field does not contain a valid addr-spec".into())
        })?;
        if !self
            .config
            .maintainer_addresses
            .iter()
            .any(|m| m.matches(&from))
        {
            return Err(ChannelError::UnauthorizedSender(
                "From address is not an authorized maintainer".into(),
            ));
        }
        // Sender header, when present, must also match an authorized maintainer.
        if let Some(s) = sender_addr {
            let sender = MailAddress::parse(s).ok_or_else(|| {
                ChannelError::UnauthorizedSender(
                    "Sender field does not contain a valid addr-spec".into(),
                )
            })?;
            if !self
                .config
                .maintainer_addresses
                .iter()
                .any(|m| m.matches(&sender))
            {
                return Err(ChannelError::UnauthorizedSender(
                    "Sender address is not an authorized maintainer".into(),
                ));
            }
        }
        Ok(())
    }

    /// Evaluate the domain-authentication half of the attribution gate
    /// (ADR-056 Amendment 2026-07-02, refined 2026-07-03): trust only the
    /// header selected per this deployment's configured [`TrustAnchor`]
    /// (topmost matching `authserv-id`, or the topmost no-authserv-id header
    /// for an EXO-shaped boundary), and require `dmarc=pass` aligned to the
    /// From domain, or `spf=pass` with an aligned envelope-from, or
    /// `dkim=pass` with an aligned `d=` domain. Absence of any trusted header
    /// fails closed.
    pub(super) fn evaluate_auth(&self, email: &RawEmail) -> Result<(), QuarantineReason> {
        let selected =
            auth_results::select_trusted(&email.authentication_results, &self.config.trust_anchor)
                .ok_or_else(|| {
                    if auth_results::observed_auth_header(
                        &email.authentication_results,
                        &email.headers,
                        &email.raw_bytes,
                    )
                    .is_some()
                    {
                        QuarantineReason::AuthUntrusted
                    } else {
                        QuarantineReason::AuthAbsent
                    }
                })?;

        let from_domain = email
            .from_addrs
            .first()
            .and_then(|addr| addr.rsplit_once('@'))
            .map(|(_, domain)| domain.to_lowercase())
            .unwrap_or_default();

        if selected.dmarc_pass_aligned(&from_domain)
            || selected.spf_pass_aligned(&from_domain)
            || selected.dkim_pass_aligned(&from_domain)
        {
            return Ok(());
        }
        if selected.has_unaligned_pass(&from_domain) {
            return Err(QuarantineReason::Unaligned);
        }
        Err(QuarantineReason::DmarcFail)
    }

    /// Run the domain-authentication-then-allowlist gate: ADR-200 §7's steps 3 through 7.
    ///
    /// Steps 1 and 2 (malformed, and the mailto unsubscribe) are decided before this is reached.
    /// Step 6: once a sender authenticates and external inbound is enabled,
    /// an `inbound` suppression entry matching the sender's address or its exact domain quarantines
    /// the message as `blocked`. Both must pass for a message to be attributed to the maintainer;
    /// any failing step quarantines it (never partially attributed).
    ///
    /// Step 3 has one opt-in exception (ADR-056 Amendment 2026-11-01, Exchange Online intra-tenant
    /// anchor). With `trust_exchange_internal` set, a message whose domain authentication is
    /// `auth-absent` is attributed to the maintainer when `exchange_internal_maintainer` holds:
    /// exactly one `From:` address on the configured mailbox's domain, exactly one
    /// `X-MS-Exchange-Organization-AuthAs` field reading `Internal`, and the existing `From:` and
    /// `Sender:` maintainer checks passing. Any failed condition stays `auth-absent`. This arm
    /// never attributes external mail and never evaluates inbound suppression.
    ///
    pub(super) fn gate(
        &self,
        email: &RawEmail,
        inbound: Option<&InboundSuppression>,
    ) -> Result<EmailAttribution, QuarantineReason> {
        match self.evaluate_auth(email) {
            Ok(()) => {}
            Err(QuarantineReason::AuthAbsent) if self.config.trust_exchange_internal => {
                if self.exchange_internal_maintainer(email) {
                    return Ok(EmailAttribution::Maintainer);
                }
                return Err(QuarantineReason::AuthAbsent);
            }
            Err(reason) => return Err(reason),
        }
        if self
            .check_sender(&email.from_addrs, email.sender_addr.as_deref())
            .is_ok()
        {
            return Ok(EmailAttribution::Maintainer);
        }
        // Step 4 (maintainer) is the allowlist passing above and is unchanged. The sender is not
        // the maintainer, so it is external: step 5 keeps today's outcome while external inbound
        // is disabled, and step 6 consults the inbound list only once it is enabled.
        if self
            .external_host
            .as_deref()
            .is_some_and(|host| host.external_inbound_enabled())
        {
            // "the authenticated `From:` address" is a single well-formed From: a message with
            // zero or several is unauthorized on its face, not a candidate for the block list.
            if let [only_from] = email.from_addrs.as_slice() {
                if let Some(sender) =
                    khive_types::email_address::normalize_email_recipient(only_from)
                {
                    let address =
                        MailAddress::parse(&sender).ok_or(QuarantineReason::OffAllowlist)?;
                    if self
                        .config
                        .maintainer_addresses
                        .iter()
                        .any(|maintainer| maintainer.matches(&address))
                    {
                        // A maintainer From with a rejected Sender must not become external.
                        return Err(QuarantineReason::OffAllowlist);
                    }
                    if inbound.is_some_and(|check| check(&sender)) {
                        return Err(QuarantineReason::Blocked);
                    }
                    if self.external_inbound.is_some() {
                        return Ok(EmailAttribution::External);
                    }
                }
            }
        }
        Err(QuarantineReason::OffAllowlist)
    }

    fn exchange_internal_maintainer(&self, email: &RawEmail) -> bool {
        let [from] = email.from_addrs.as_slice() else {
            return false;
        };
        let Some((_, domain)) = from.rsplit_once('@') else {
            return false;
        };
        let Some((_, mailbox_domain)) = self.config.mailbox.rsplit_once('@') else {
            return false;
        };
        if !domain.eq_ignore_ascii_case(mailbox_domain)
            || self
                .check_sender(&email.from_addrs, email.sender_addr.as_deref())
                .is_err()
        {
            return false;
        }
        let values =
            auth_results::raw_header_values(&email.raw_bytes, b"X-MS-Exchange-Organization-AuthAs");
        values.len() == 1 && values[0].trim().eq_ignore_ascii_case("Internal")
    }

    /// Build the envelope recorded for a message that failed the attribution
    /// gate. Per ADR-056 Amendment 2026-07-02, a quarantined message must
    /// never be attributed to (or look like) the maintainer: `from` is the
    /// fixed `email:quarantine` marker rather than the message's own
    /// (possibly forged) From address, and no correlation key is set --  an
    /// unauthenticated message claiming to be a reply must not inherit a
    /// legitimate thread's context.
    pub(super) fn quarantine_envelope(
        &self,
        email: &RawEmail,
        reason: QuarantineReason,
    ) -> ChannelEnvelope {
        let to = format!("email:{}", self.maintainer_address());
        let mut env = ChannelEnvelope::new(EMAIL_QUARANTINE_SENDER, to.clone(), email.best_body())
            .with_quarantine_replay(email.raw_bytes.clone(), to);

        if !email.subject.is_empty() {
            env = env.with_subject(&email.subject);
        }
        if let Some(date) = email.date {
            env = env.with_sent_at(date);
        }
        env = env
            .with_external_id(account_scoped_imap_id(
                &email.imap_external_id,
                &self.config.mailbox,
            ))
            .with_legacy_external_id(&email.imap_external_id);

        env.metadata
            .insert("quarantined".to_string(), "true".to_string());
        env.metadata
            .insert("quarantine_reason".to_string(), reason.to_string());
        if reason == QuarantineReason::AuthUntrusted {
            if let Some((family, raw)) = auth_results::observed_auth_header(
                &email.authentication_results,
                &email.headers,
                &email.raw_bytes,
            ) {
                let mode = match &self.config.trust_anchor {
                    crate::config::TrustAnchor::AuthservId(_) => "authserv-id",
                    crate::config::TrustAnchor::TopmostNoAuthservId => "topmost-no-authserv-id",
                    crate::config::TrustAnchor::TopmostWithAuthservId(_) => {
                        "topmost-with-authserv-id"
                    }
                };
                env.metadata
                    .insert("quarantine_auth_anchor_mode".into(), mode.into());
                env.metadata
                    .insert("quarantine_auth_header_family".into(), family.into());
                let (status, truncated) = match auth_results::observed_authserv_id(
                    raw,
                    family == "arc-authentication-results",
                ) {
                    auth_results::ObservedAuthservId::Present { value, truncated } => {
                        env.metadata.insert("quarantine_auth_id".into(), value);
                        ("present", truncated)
                    }
                    auth_results::ObservedAuthservId::NoAuthservId => ("no-authserv-id", false),
                    auth_results::ObservedAuthservId::Unparseable => ("unparseable", false),
                };
                env.metadata
                    .insert("quarantine_auth_id_status".into(), status.into());
                env.metadata
                    .insert("quarantine_auth_id_truncated".into(), truncated.to_string());
            }
        }
        if let Some(claimed_from) = email.from_addrs.first() {
            env.metadata
                .insert("quarantine_claimed_from".to_string(), claimed_from.clone());
        }
        if !email.to.is_empty() {
            // Informational parsed To claims only; quarantine routing stays with the maintainer.
            env.metadata.insert(
                "quarantine_claimed_to".into(),
                serde_json::json!(&email.to).to_string(),
            );
        }

        env
    }

    /// Build the envelope recorded for a selected UID that could not be
    /// ingested because it is too large or lacks a parseable RFC 822 body.
    /// Unlike [`Self::quarantine_envelope`],
    /// this is never gated by `quarantine_store` -- a data-integrity failure
    /// must always leave a queryable record, never a silent drop, since
    /// dropping it here is the only way this UID's disposition could be lost
    /// before the cursor advances past it.
    /// No parsed `RawEmail` is available, so claimed recipients are omitted even
    /// when replay bytes exist; malformed or oversized input is not parsed again.
    pub(super) fn malformed_quarantine_envelope(
        &self,
        uid: u32,
        imap_external_id: &str,
        reason: QuarantineReason,
        raw_bytes: Option<Vec<u8>>,
    ) -> ChannelEnvelope {
        let to = format!("email:{}", self.maintainer_address());
        let body = format!("(khive: IMAP message UID {uid} was quarantined: {reason})");
        let mut env = ChannelEnvelope::new(EMAIL_QUARANTINE_SENDER, to.clone(), body)
            .with_quarantine_replay(raw_bytes.unwrap_or_default(), to);
        env = env
            .with_external_id(account_scoped_imap_id(
                imap_external_id,
                &self.config.mailbox,
            ))
            .with_legacy_external_id(imap_external_id);
        env.metadata
            .insert("quarantined".to_string(), "true".to_string());
        env.metadata
            .insert("quarantine_reason".to_string(), reason.to_string());
        env
    }

    /// This channel's stable, non-secret checkpoint identity.
    ///
    /// Compared verbatim (never parsed) against a stored checkpoint's
    /// `source` to detect a host/port/mailbox/folder configuration change —
    /// in which case the stored generation/high-water must not be applied to
    /// the current (different) source.
    pub(super) fn checkpoint_source(&self) -> String {
        format!(
            "imap+tls:{}:{}:{}:{}",
            self.config.imap_host.to_lowercase(),
            self.config.imap_port,
            self.config.mailbox,
            IMAP_FOLDER
        )
    }

    /// Decode a stored checkpoint into the connector-level [`ImapProgress`],
    /// plus the `committed_at` recovery floor (only when the source matches).
    ///
    /// A source mismatch or absent checkpoint yields default (empty)
    /// progress and no recovery floor — a different account's high-water or
    /// timestamp must never apply to this one. A persisted value that does
    /// not fit a nonzero `u32` fails closed with `ChannelError::Config`
    /// rather than silently resetting.
    pub(super) fn decode_progress(
        &self,
        checkpoint: Option<&StoredChannelCheckpoint>,
    ) -> Result<(ImapProgress, Option<DateTime<Utc>>), ChannelError> {
        let Some(stored) = checkpoint else {
            return Ok((ImapProgress::default(), None));
        };
        if stored.checkpoint.source != self.checkpoint_source() {
            return Ok((ImapProgress::default(), None));
        }

        let uid_validity = u32::try_from(stored.checkpoint.generation)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| {
                ChannelError::Config(format!(
                    "persisted IMAP checkpoint generation {} is not a valid nonzero u32",
                    stored.checkpoint.generation
                ))
            })?;
        let last_seen_uid =
            match stored.checkpoint.high_water {
                None => None,
                Some(h) => Some(u32::try_from(h).ok().and_then(NonZeroU32::new).ok_or_else(
                    || {
                        ChannelError::Config(format!(
                            "persisted IMAP checkpoint high_water {h} is not a valid nonzero u32"
                        ))
                    },
                )?),
            };

        Ok((
            ImapProgress {
                uid_validity: Some(uid_validity),
                last_seen_uid,
            },
            Some(stored.committed_at),
        ))
    }

    /// Encode connector-level progress into the transport-neutral checkpoint
    /// the poll coordinator persists.
    pub(super) fn encode_checkpoint(&self, progress: ImapProgress) -> ChannelCheckpoint {
        ChannelCheckpoint {
            source: self.checkpoint_source(),
            generation: progress
                .uid_validity
                .map(|v| u64::from(v.get()))
                .unwrap_or(0),
            high_water: progress.last_seen_uid.map(|v| u64::from(v.get())),
        }
    }

    /// Run ADR-200 §7's ordered disposition over a fetched batch, producing the envelopes ready
    /// for `comm.ingest` plus any deferred mailto-unsubscribe suppression writes.
    ///
    /// Extracted from `poll` so `poll_page` shares the exact same per-message logic. Every
    /// [`SelectedMessage`] produces exactly one disposition here (khive #449 High fix): a parsed
    /// email reaches step 2 (mailto unsubscribe) before step 3 (domain authentication) and then
    /// the gate, and a [`SelectedMessage::Malformed`] entry is step 1 and always becomes a
    /// quarantine envelope, regardless of `quarantine_store` -- see
    /// [`Self::malformed_quarantine_envelope`].
    ///
    /// A message at step 2 produces no envelope: the suppression entry is its machine record, and
    /// the poll cursor advancing past its UID is how the poller marks it processed, so it is never
    /// delivered to an actor, never quarantined and never screened. The write itself is deferred to
    /// the async caller so a store failure can hold the poll rather than silently drop the opt-out.
    pub(super) async fn disposition(
        &self,
        raw: Vec<SelectedMessage>,
        inbound: Option<&InboundSuppression>,
    ) -> Result<(Vec<ChannelEnvelope>, Vec<PendingUnsubscribe>), ChannelError> {
        let mut envelopes = Vec::new();
        let mut pending_unsubscribes = Vec::new();
        for message in raw {
            match message {
                SelectedMessage::Email(email) => {
                    let uid = email.uid;
                    // §7 step 2 precedes §7 step 3: the token is the authority, so a verified one
                    // is honoured even when the message would fail domain authentication. A token
                    // that does not verify (tampered, unknown key id, wrong length, garbage) is
                    // ignored and the message continues below as ordinary mail.
                    if let Some(recipient_key) = self.verified_unsubscribe_key(&email) {
                        pending_unsubscribes.push(PendingUnsubscribe { uid, recipient_key });
                        continue;
                    }
                    match self.gate(&email, inbound) {
                        Ok(EmailAttribution::Maintainer) => {
                            envelopes.push(self.to_envelope(*email))
                        }
                        Ok(EmailAttribution::External) => {
                            envelopes.push(self.external_envelope(*email).await?);
                        }
                        Err(reason) => {
                            if self.config.quarantine_store {
                                envelopes.push(self.quarantine_envelope(&email, reason));
                            } else {
                                // Address-free: only the IMAP UID and the quarantine reason
                                // are logged, never the (possibly forged) From address.
                                warn!(
                                    uid,
                                    reason = %reason,
                                    "quarantine-store disabled: dropping unattributed message"
                                );
                            }
                        }
                    }
                }
                SelectedMessage::Malformed {
                    uid,
                    imap_external_id,
                    reason,
                    raw_bytes,
                } => {
                    let reason: QuarantineReason = reason.into();
                    warn!(
                        uid,
                        reason = %reason,
                        "quarantining IMAP message"
                    );
                    envelopes.push(self.malformed_quarantine_envelope(
                        uid,
                        &imap_external_id,
                        reason,
                        raw_bytes,
                    ));
                }
            }
        }
        Ok((envelopes, pending_unsubscribes))
    }

    pub(super) async fn external_envelope(
        &self,
        email: RawEmail,
    ) -> Result<ChannelEnvelope, ChannelError> {
        let Some(message) = MessageParser::default().parse(&email.raw_bytes) else {
            let mut envelope = self.quarantine_envelope(&email, QuarantineReason::ParseFailure);
            envelope.content = "(khive: external email could not be screened as MIME)".into();
            return Ok(envelope);
        };
        let outcome = match screen_message(&message) {
            Ok(outcome) => outcome,
            Err(_) => {
                let mut envelope = self.quarantine_envelope(&email, QuarantineReason::Screened);
                envelope.content = "(khive: external email quarantined because screening rules could not be loaded)".into();
                envelope.metadata.insert("labels".into(), "[]".into());
                return Ok(envelope);
            }
        };
        if outcome.disposition == Disposition::QuarantineScreened {
            let mut envelope = self.quarantine_envelope(&email, QuarantineReason::Screened);
            envelope.content = screening_placeholder("quarantined", &outcome);
            envelope.metadata.insert(
                "labels".into(),
                serde_json::to_string(&outcome.rule_ids)
                    .map_err(|error| ChannelError::Config(error.to_string()))?,
            );
            // ADR-200 Amendment 1 Decision 5: an opt-out from an authenticated sender is recorded
            // whatever the screening outcome, so the quarantine carries it to ingest.
            if outcome.opt_out {
                envelope
                    .metadata
                    .insert("screening_opt_out".into(), "true".into());
            }
            return Ok(envelope);
        }
        let config = self.external_inbound.as_ref().ok_or_else(|| {
            ChannelError::Config("external inbound routing is not configured".into())
        })?;
        let host = self.external_host.as_deref().ok_or_else(|| {
            ChannelError::Config("external inbound host is not configured".into())
        })?;
        let mut verified = std::collections::HashMap::new();
        let candidates = owner_candidates(&email);
        let mut pending_owner_candidates = None;
        for candidate in &candidates {
            match host.lookup_thread_owner(candidate).await {
                Ok(Some(owner)) => {
                    if uuid::Uuid::parse_str(&owner.thread_id).is_err() {
                        pending_owner_candidates = Some(candidates.clone());
                    } else {
                        verified.insert(candidate.clone(), owner);
                    }
                    break;
                }
                Ok(None) => {}
                Err(_) => {
                    // Ingest retains the UID and the original typed store failure on retry.
                    pending_owner_candidates = Some(candidates.clone());
                    break;
                }
            }
        }
        let route = route_inbound(&email, &config.routes, &config.actor, |candidate| {
            verified.get(candidate).cloned()
        });
        let (actor, thread_id) = match route {
            InboundRoute::Owner { actor, thread_id } => {
                let thread_id = uuid::Uuid::parse_str(&thread_id).map_err(|_| {
                    ChannelError::Config("verified email owner has an invalid thread UUID".into())
                })?;
                (actor, Some(thread_id))
            }
            InboundRoute::RoutedAlias { actor, .. } | InboundRoute::ExternalInbound { actor } => {
                (actor, None)
            }
        };
        let labels: Vec<&str> = outcome.rule_ids.iter().map(String::as_str).collect();
        let marking = external_delivery_marking(
            &email.from_addrs[0],
            &labels,
            &format!("email:{}", self.maintainer_address()),
            self.maintainer_address(),
        )
        .map_err(|error| ChannelError::Config(error.to_string()))?;
        let mut envelope = self.to_envelope(email);
        envelope.from = marking.sender;
        envelope.correlation_external_id = None;
        envelope
            .metadata
            .insert("origin".into(), marking.origin.into());
        envelope
            .metadata
            .insert("trust".into(), marking.trust.into());
        envelope.metadata.insert(
            "labels".into(),
            serde_json::to_string(&marking.labels)
                .map_err(|error| ChannelError::Config(error.to_string()))?,
        );
        let body_withheld = outcome.withheld();
        if body_withheld {
            envelope.content = screening_placeholder("withheld", &outcome);
        }
        envelope.inbound_delivery = Some(InboundChannelDelivery {
            actor,
            thread_id,
            labels: marking.labels,
            body_withheld,
            reply_opt_out: outcome.opt_out,
            pending_owner_candidates,
        });
        Ok(envelope)
    }

    /// The verified recipient key when the message's subject carries a mailto unsubscribe token,
    /// or `None` (no token, a non-verifying token, or no configured host).
    ///
    /// The token verifier comes from the host's outbound policy; the key variables are never
    /// parsed a second time here. Only the subject and the host are read, so this never observes
    /// or logs the `From:` address.
    pub(super) fn verified_unsubscribe_key(&self, email: &RawEmail) -> Option<String> {
        let candidate = extract_unsubscribe_token(&email.subject)?;
        self.external_host
            .as_deref()?
            .verify_unsubscribe_token(candidate)
    }

    /// Record the deferred mailto-unsubscribe entries (§5.4 entry point 1) through the host.
    ///
    /// A second copy of the same unsubscribe is not a failure (uniqueness covers the entry). A
    /// write failure becomes a retryable channel error so the poll is not allowed to advance: the
    /// opt-out is never silently dropped.
    pub(super) async fn record_unsubscribes(
        &self,
        pending: &[PendingUnsubscribe],
    ) -> Result<(), ChannelError> {
        if pending.is_empty() {
            return Ok(());
        }
        let Some(host) = self.external_host.as_deref() else {
            warn!(
                "mailto unsubscribe: no comm store is wired to this channel; suppression entry not recorded"
            );
            return Ok(());
        };
        for entry in pending {
            match host.record_mailto_unsubscribe(&entry.recipient_key).await {
                Ok(recorded) => {
                    debug!(
                        uid = entry.uid,
                        recorded, "mailto unsubscribe suppression entry processed"
                    );
                }
                Err(error) => {
                    return Err(ChannelError::Transport(format!(
                        "mailto unsubscribe suppression write failed: {error}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Load the `inbound` suppression check for one poll.
    ///
    /// Loaded once per poll (ADR-200 §7 step 6) and shared across the page's messages. When the
    /// channel has no host, or external inbound is disabled, no read happens.
    pub(super) async fn load_inbound_suppression(
        &self,
    ) -> Result<Option<InboundSuppression>, ChannelError> {
        let host = self
            .external_host
            .as_deref()
            .filter(|host| host.external_inbound_enabled());
        match host {
            Some(host) => host.inbound_suppression().await.map(Some),
            None => Ok(None),
        }
    }

    /// Convert a `RawEmail` that has already passed `gate` into a `ChannelEnvelope`.
    pub(super) fn to_envelope(&self, email: RawEmail) -> ChannelEnvelope {
        // Safe: gate() verified exactly one From entry before this is called.
        let from_addr = &email.from_addrs[0];
        let from = format!("email:{from_addr}");
        let to = email
            .to
            .first()
            .map(|a| format!("email:{a}"))
            .unwrap_or_else(|| format!("email:{}", self.maintainer_address()));

        let mut env = ChannelEnvelope::new(from, to, email.best_body());

        if !email.subject.is_empty() {
            env = env.with_subject(&email.subject);
        }
        if let Some(date) = email.date {
            env = env.with_sent_at(date);
        }
        // Always set external_id from the stable IMAP-based dedup key. Never derive it
        // from Message-ID, which is optional and could be absent or absent-by-design.
        env = env
            .with_external_id(account_scoped_imap_id(
                &email.imap_external_id,
                &self.config.mailbox,
            ))
            .with_legacy_external_id(&email.imap_external_id);
        if let Some(corr) = email.correlation() {
            env = env.with_correlation(corr);
        }
        // Capture this email's own Message-ID so a future reply to it can set
        // In-Reply-To/References for native MUA threading (distinct from external_id).
        if let Some(mid) = email.message_id() {
            env = env.with_wire_message_id(mid);
        }
        // Capture this email's own References chain so a future reply can extend
        // the full ancestor chain instead of truncating it to the immediate parent.
        if let Some(refs) = email.references() {
            env = env.with_wire_references(refs);
        }

        env.with_quarantine_replay(
            email.raw_bytes,
            format!("email:{}", self.maintainer_address()),
        )
    }
}
