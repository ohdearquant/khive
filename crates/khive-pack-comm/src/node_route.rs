//! Node-address routing seam for `comm.send`.
//!
//! A recipient written as a node address (the `khive1:` form) is not delivered through the local
//! mailbox or email. When a route is installed, the send path hands it to the [`NodeSendRoute`],
//! which parses the address, looks up the recipient's pinned key and seals the message, and the
//! handler stores the sealed envelope as a sender transport row. Without an installed route, a
//! node-addressed send is refused before anything is written.

use std::future::Future;
use std::pin::Pin;

use khive_runtime::comm_transport::SenderEnvelope;
use khive_runtime::RuntimeError;
use uuid::Uuid;

/// Prefix that marks a recipient as a node address. The scheme is matched case-insensitively.
pub const NODE_ADDRESS_PREFIX: &str = "khive1:";

/// How a raw `to` value relates to the node-address form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipientForm {
    /// Not a node address; the local actor-label and email paths apply.
    Local,
    /// A node address exactly as written, with no surrounding whitespace.
    Node,
    /// A node address with leading or trailing whitespace. It is refused, never trimmed.
    PaddedNode,
}

/// Classify the raw recipient exactly as the caller wrote it. This is the only place the send
/// path decides whether a recipient is a node address.
pub fn classify_recipient(raw: &str) -> RecipientForm {
    let trimmed = raw.trim();
    let has_scheme = trimmed
        .get(..NODE_ADDRESS_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(NODE_ADDRESS_PREFIX));
    if !has_scheme {
        RecipientForm::Local
    } else if trimmed.len() != raw.len() {
        RecipientForm::PaddedNode
    } else {
        RecipientForm::Node
    }
}

/// The refusal owed to a recipient before anything is written, or `None` when the recipient is
/// not a node address or the send may go on to sealing. A node-form recipient with no route
/// installed is always refused as disabled; it never falls through to the local path.
pub fn refusal_before_write(form: RecipientForm, route_installed: bool) -> Option<NodeSendRefusal> {
    match form {
        RecipientForm::Local => None,
        _ if !route_installed => Some(NodeSendRefusal::Disabled),
        RecipientForm::PaddedNode => Some(NodeSendRefusal::UnparseableAddress(
            "surrounding whitespace is not allowed".to_string(),
        )),
        RecipientForm::Node => None,
    }
}

/// Everything a route needs to seal one outgoing message.
pub struct NodeSealRequest<'a> {
    /// Recipient node address exactly as the caller wrote it; surrounding whitespace is refused
    /// before sealing, so this never carries any.
    pub address: &'a str,
    /// Durable identity chosen for this message before any write.
    pub logical_message_id: Uuid,
    /// Actor label of the sender.
    pub from_actor: &'a str,
    /// Message body.
    pub content: &'a str,
    /// Optional subject line.
    pub subject: Option<&'a str>,
    /// Optional canonical thread id.
    pub thread_id: Option<&'a str>,
    /// Namespace the send runs in; a route bound to one namespace refuses any other.
    pub namespace: &'a str,
    /// Send time as an RFC 3339 string, taken once by the handler so the sealed envelope and the
    /// stored message agree.
    pub sent_at: &'a str,
}

/// Why a node-addressed send was refused. Each variant maps to a distinct error text, and every
/// refusal happens before any message row is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeSendRefusal {
    /// The node channel is not enabled for this deployment.
    Disabled,
    /// The recipient string is not a valid node address; carries the parser's detail.
    UnparseableAddress(String),
    /// No pinned key is known for the recipient address.
    UnknownRecipientPin(String),
    /// The node configuration is invalid; carries a bounded detail naming the field.
    InvalidConfiguration(String),
    /// The persisted node state could not be read; retrying may succeed.
    SourceUnavailable,
    /// The key facility could not provide the sender key.
    KeyUnavailable,
    /// The recipient contact lookup failed; retrying may succeed.
    ContactUnavailable,
    /// The sender actor, namespace or realm does not match the configured node binding.
    SenderBindingMismatch,
    /// Sealing the envelope failed.
    SealFailed,
    /// Pair policy is set to `shadow` or `enforce`, and evaluation is not available.
    PairPolicyUnavailable,
}

impl NodeSendRefusal {
    /// Convert the refusal into the error returned to the caller.
    pub fn into_runtime_error(self) -> RuntimeError {
        RuntimeError::InvalidInput(match self {
            Self::Disabled => {
                "send: the node channel is disabled; node addresses cannot be sent to".to_string()
            }
            Self::UnparseableAddress(detail) => {
                format!("send: `to` is not a valid node address: {detail}")
            }
            Self::UnknownRecipientPin(address) => format!(
                "send: no pinned key for recipient {address}; pin the recipient before sending"
            ),
            Self::InvalidConfiguration(detail) => {
                format!("send: node configuration is invalid: {detail}")
            }
            Self::SourceUnavailable => {
                "send: node state is unavailable; retry later".to_string()
            }
            Self::KeyUnavailable => "send: node key facility is unavailable".to_string(),
            Self::ContactUnavailable => {
                "send: node contact lookup is unavailable; retry later".to_string()
            }
            Self::SenderBindingMismatch => {
                "send: sender does not match the configured node binding".to_string()
            }
            Self::SealFailed => "send: node envelope could not be sealed".to_string(),
            Self::PairPolicyUnavailable => {
                "send: pair policy evaluation is unavailable; set pair_policy_mode to off to send node-addressed messages"
                    .to_string()
            }
        })
    }
}

/// Future returned by [`NodeSendRoute::seal`].
pub type SealFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SenderEnvelope, NodeSendRefusal>> + Send + 'a>>;

/// Seals a message for a node recipient. Implementations own address parsing, the pinned-key
/// lookup and the key facility. The returned envelope carries a placeholder `outbound_note_id`
/// that the send handler overwrites once the outbound note exists.
pub trait NodeSendRoute: Send + Sync {
    /// Seal `request` for the recipient's pinned key, or refuse without side effects.
    fn seal<'a>(&'a self, request: NodeSealRequest<'a>) -> SealFuture<'a>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_matches_the_scheme_in_any_case() {
        for raw in ["khive1:abc", "KHIVE1:abc", "Khive1:abc"] {
            assert_eq!(classify_recipient(raw), RecipientForm::Node, "{raw}");
        }
    }

    #[test]
    fn classify_flags_padding_instead_of_trimming() {
        for raw in [" khive1:abc", "khive1:abc ", "\tKHIVE1:abc\n"] {
            assert_eq!(
                classify_recipient(raw),
                RecipientForm::PaddedNode,
                "{raw:?}"
            );
        }
    }

    #[test]
    fn classify_leaves_other_recipients_local() {
        for raw in [
            "actor:recipient",
            "telegram:recipient",
            "khive",
            "khive2:abc",
            "",
            "é",
        ] {
            assert_eq!(classify_recipient(raw), RecipientForm::Local, "{raw:?}");
        }
    }

    #[test]
    fn node_form_without_a_route_is_refused_as_disabled() {
        for form in [RecipientForm::Node, RecipientForm::PaddedNode] {
            assert_eq!(
                refusal_before_write(form, false),
                Some(NodeSendRefusal::Disabled)
            );
        }
        assert_eq!(refusal_before_write(RecipientForm::Local, false), None);
    }

    #[test]
    fn padded_node_form_with_a_route_is_unparseable() {
        assert!(matches!(
            refusal_before_write(RecipientForm::PaddedNode, true),
            Some(NodeSendRefusal::UnparseableAddress(_))
        ));
        assert_eq!(refusal_before_write(RecipientForm::Node, true), None);
    }
}
