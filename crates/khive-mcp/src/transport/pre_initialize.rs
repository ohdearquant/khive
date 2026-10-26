// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Reply to method probes without ending a fresh stdio handshake.

use rmcp::{
    model::{ClientRequest, ErrorCode, ErrorData, JsonRpcMessage, ServerJsonRpcMessage},
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
    RoleServer,
};

/// Reject requests other than initialize and ping until initialize is forwarded.
///
/// Rejections are written through the inner transport before another message is
/// read. Wrap the stdio EOF/deadline transport so these replies retain its
/// request accounting and write limits. Notifications and other non-request
/// messages pass through unchanged, and the first initialize permanently ends
/// filtering. An already-initialized, resumed connection starts in pass-through
/// mode instead.
pub struct PreInitializeRequestTransport<T> {
    inner: T,
    initialize_forwarded: bool,
}

impl<T> PreInitializeRequestTransport<T> {
    /// Start filtering a fresh connection until its initialize request arrives.
    #[must_use]
    pub fn new(inner: T) -> Self {
        Self::with_initialize_forwarded(inner, false)
    }

    pub(crate) fn with_initialize_forwarded(inner: T, initialize_forwarded: bool) -> Self {
        Self {
            inner,
            initialize_forwarded,
        }
    }
}

impl<T> Transport<RoleServer> for PreInitializeRequestTransport<T>
where
    T: Transport<RoleServer>,
{
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        loop {
            let message = self.inner.receive().await?;
            if self.initialize_forwarded {
                return Some(message);
            }
            let JsonRpcMessage::Request(request) = &message else {
                return Some(message);
            };
            match &request.request {
                ClientRequest::InitializeRequest(_) => {
                    self.initialize_forwarded = true;
                    return Some(message);
                }
                ClientRequest::PingRequest(_) => return Some(message),
                _ => {}
            }
            let error = match &request.request {
                ClientRequest::CustomRequest(custom) if custom.method.as_str() == "initialize" => {
                    ErrorData::new(
                        ErrorCode::INVALID_PARAMS,
                        "Invalid parameters for initialize",
                        None,
                    )
                }
                _ => ErrorData::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!(
                        "Method not found before initialize: {}",
                        request.request.method()
                    ),
                    None,
                ),
            };
            let response = ServerJsonRpcMessage::error(error, Some(request.id.clone()));
            if self.inner.send(response).await.is_err() {
                // receive cannot return a send error. End this connection;
                // stdio's inner transport also cancels its root on failure.
                return None;
            }
        }
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}
