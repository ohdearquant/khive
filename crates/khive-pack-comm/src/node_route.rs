//! Node-address routing seam for `comm.send`.
//!
//! A recipient written as a node address (the `khive1:` form) is not delivered through the local
//! mailbox or email. The send path hands it to a [`NodeSendRoute`], which parses the address,
//! looks up the recipient's pinned key and seals the message. The handler then stores the sealed
//! envelope as a sender transport row. The node channel client later reads that envelope back and
//! never seals a message a second time.

use std::future::Future;
use std::pin::Pin;

use khive_runtime::comm_transport::SenderEnvelope;
use khive_runtime::RuntimeError;
use uuid::Uuid;

/// Prefix that marks a recipient as a node address.
pub const NODE_ADDRESS_PREFIX: &str = "khive1:";

/// Everything a route needs to seal one outgoing message.
pub struct NodeSealRequest<'a> {
    /// Recipient node address exactly as the caller wrote it (trimmed).
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
