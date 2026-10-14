//! Shared, configuration-bound outbound ownership resolution.

use super::super::{
    message_id_match_candidates, outbound_email_message, thread_id_query_spellings,
    verified_outbound_email_external_id, EmailMessageIdDomains, FilterOp, KhiveRuntime,
    NamespaceToken, NoteFilter, PageRequest, PropertyFilter, RuntimeError, SqlValue, Uuid, Value,
};

/// A thread and sending actor selected by the existing verified outbound lookup.
#[derive(Clone, Debug)]
pub struct ResolvedInboundThread {
    pub thread_id: Uuid,
    pub actor: String,
}

/// Resolve one routing candidate without trusting copied Message-IDs or changing
/// the independent UUID-root fallback. Shared by ingest and the email host.
pub async fn resolve_ingest_correlation(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    email_domains: &Result<Option<EmailMessageIdDomains>, String>,
    corr: &str,
    email_reply: bool,
) -> Result<Option<ResolvedInboundThread>, RuntimeError> {
    let ns = token.namespace().as_str();
    let store = runtime.notes(token)?;
    let resolved: Option<(String, String)> = if !corr.is_empty() {
        // Pass 1: match by $.external_id (RFC 822 Message-ID, standard In-Reply-To path).
        let mut pass1 = None;
        for candidate in message_id_match_candidates(corr) {
            let corr_filter = NoteFilter {
                kind: Some("message".to_string()),
                property_filters: vec![
                    PropertyFilter {
                        json_path: "$.external_id".to_string(),
                        op: FilterOp::Eq,
                        value: SqlValue::Text(candidate),
                    },
                    PropertyFilter {
                        json_path: "$.direction".to_string(),
                        op: FilterOp::Eq,
                        value: SqlValue::Text("outbound".to_string()),
                    },
                ],
                ..Default::default()
            };
            let mut offset = 0;
            loop {
                let corr_page = store
                    .query_notes_filtered_count_free(
                        ns,
                        &corr_filter,
                        PageRequest { limit: 100, offset },
                    )
                    .await?;
                let count = corr_page.items.len();
                if let Some(n) = corr_page.items.iter().find(|n| {
                    if email_reply || outbound_email_message(n) {
                        verified_outbound_email_external_id(n, email_domains)
                    } else {
                        true
                    }
                }) {
                    // A copied Message-ID can sort first. Only the row that
                    // owns its UUID may supply the thread and actor.
                    let thread_id = n
                        .properties
                        .as_ref()
                        .and_then(|props| props.get("thread_id"))
                        .and_then(Value::as_str)
                        .and_then(|s| s.parse::<Uuid>().ok())
                        .map(|id| id.as_hyphenated().to_string())
                        .unwrap_or_else(|| n.id.as_hyphenated().to_string());
                    let from_actor = n
                        .properties
                        .as_ref()
                        .and_then(|props| props.get("from_actor"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    pass1 = Some((thread_id, from_actor));
                    break;
                }
                if count < 100 {
                    break;
                }
                offset = offset.checked_add(count as u64).ok_or_else(|| {
                    RuntimeError::Internal(
                        "ingest: correlation pagination offset overflowed".into(),
                    )
                })?;
            }
            if pass1.is_some() {
                break;
            }
        }

        if pass1.is_some() {
            pass1
        } else if let Ok(correlation_root) = corr.trim().parse::<Uuid>() {
            // Pass 2: `corr` is a UUID — may be a thread UUID from X-Khive-Thread-ID.
            // Match the canonical spelling written by v1 against $.thread_id on an
            // outbound note to recover from_actor. The selected root stays canonical
            // even when the transport supplied a compact or braced UUID.
            let canonical_correlation_root = correlation_root.as_hyphenated().to_string();
            // No backfill rewrites pre-v1 rows, so probe every spelling older
            // handlers could have stored (canonical, compact, braced, URN, and
            // upper-hex) as well as the canonical v1 form. Whatever spelling
            // matched, the selected root returned below is always canonical.
            let candidates = thread_id_query_spellings(correlation_root, Some(corr.trim()));

            let mut thread_match = None;
            for candidate in candidates {
                let thread_filter = NoteFilter {
                    kind: Some("message".to_string()),
                    property_filters: vec![
                        PropertyFilter {
                            json_path: "$.thread_id".to_string(),
                            op: FilterOp::Eq,
                            value: SqlValue::Text(candidate),
                        },
                        PropertyFilter {
                            json_path: "$.direction".to_string(),
                            op: FilterOp::Eq,
                            value: SqlValue::Text("outbound".to_string()),
                        },
                    ],
                    ..Default::default()
                };
                let thread_page = store
                    .query_notes_filtered_count_free(
                        ns,
                        &thread_filter,
                        PageRequest {
                            limit: 1,
                            offset: 0,
                        },
                    )
                    .await?;
                if let Some(note) = thread_page.items.first() {
                    let from_actor = note
                        .properties
                        .as_ref()
                        .and_then(|props| props.get("from_actor"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    thread_match = Some((canonical_correlation_root.clone(), from_actor));
                    break;
                }
            }
            thread_match
        } else {
            None
        }
    } else {
        None
    };
    resolved
        .map(|(thread_id, actor)| {
            let thread_id = thread_id.parse::<Uuid>().map_err(|error| {
                RuntimeError::Internal(format!("ingest: resolved thread is not a UUID: {error}"))
            })?;
            Ok(ResolvedInboundThread { thread_id, actor })
        })
        .transpose()
}
