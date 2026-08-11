//! Registerable MCP serving transports.
//!
//! A [`Transport`] owns the wire protocol used to serve the `request` surface.
//! Built-ins are registered via [`TransportRegistry::with_builtins`]; additional
//! transports (e.g. Streamable HTTP) register with [`TransportRegistry::register`]
//! before serving, so the serve path never hard-codes a transport enum.

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::server::KhiveMcpServer;

/// Cancels rmcp's root service token before reporting transport EOF.
///
/// rmcp otherwise classifies EOF as a graceful close and drains in-flight
/// handlers for five seconds without cancelling their per-request child
/// tokens. The same token is passed to `serve_with_ct`/`serve_directly_with_ct`,
/// so cancelling it here reaches every admitted handler before that drain
/// begins. Send and close remain transparent, allowing this wrapper to sit
/// outside the Unix post-flush self-heal transport without changing its flush
/// ordering.
pub(crate) struct CancelOnEofTransport<T> {
    inner: T,
    root: tokio_util::sync::CancellationToken,
}

impl<T> CancelOnEofTransport<T> {
    pub(crate) fn new(inner: T, root: tokio_util::sync::CancellationToken) -> Self {
        Self { inner, root }
    }
}

impl<T> rmcp::transport::Transport<rmcp::RoleServer> for CancelOnEofTransport<T>
where
    T: rmcp::transport::Transport<rmcp::RoleServer>,
{
    type Error = T::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>>>
           + Send {
        let receive = self.inner.receive();
        let root = self.root.clone();
        async move {
            let message = receive.await;
            if message.is_none() {
                root.cancel();
            }
            message
        }
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

/// Options passed to a transport at serve time.
#[derive(Debug, Default, Clone)]
pub struct ServeOptions {
    /// Bind address for network transports (e.g. `0.0.0.0:8080`). Ignored by stdio.
    pub bind: Option<String>,
}

/// A way to serve the MCP `request` surface.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Name selected via `--transport <name>`.
    fn name(&self) -> &'static str;

    /// One-line description for help and listing.
    fn about(&self) -> &'static str;

    /// Serve until the connection closes. Consumes the server.
    async fn serve(&self, server: KhiveMcpServer, opts: &ServeOptions) -> anyhow::Result<()>;
}

/// MCP over stdio — the default transport, used by the deno/npm wrapper.
pub struct StdioTransport;

#[async_trait]
impl Transport for StdioTransport {
    fn name(&self) -> &'static str {
        "stdio"
    }

    fn about(&self) -> &'static str {
        "MCP over stdio (default)"
    }

    async fn serve(&self, server: KhiveMcpServer, _opts: &ServeOptions) -> anyhow::Result<()> {
        server.serve_stdio().await
    }
}

/// Named registry of serving transports.
pub struct TransportRegistry {
    transports: BTreeMap<&'static str, Box<dyn Transport>>,
}

impl TransportRegistry {
    /// Empty registry — no transports.
    pub fn new() -> Self {
        Self {
            transports: BTreeMap::new(),
        }
    }

    /// Registry pre-populated with the built-in transports (`stdio`).
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(StdioTransport));
        registry
    }

    /// Add (or replace) a transport. Keyed by [`Transport::name`].
    pub fn register(&mut self, transport: Box<dyn Transport>) {
        self.transports.insert(transport.name(), transport);
    }

    /// Look up a transport by name.
    pub fn get(&self, name: &str) -> Option<&dyn Transport> {
        self.transports.get(name).map(|t| t.as_ref())
    }

    /// All registered transport names, sorted.
    pub fn names(&self) -> Vec<&'static str> {
        self.transports.keys().copied().collect()
    }
}

impl Default for TransportRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}
