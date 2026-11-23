//! The bridge's side of ADR-220: the caller's computer as this process holds
//! it, and the request path that sends routed operations there.
//!
//! Only the process that resolved the caller's configuration routes. A server
//! built for the daemon drops its computer before it serves
//! ([`KhiveMcpServer::without_computer`]), so the daemon holds no routing rule
//! and no key.
#![cfg(unix)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use khive_request::{parse_request, ParsedRequest};
use khive_runtime::{ConfigError, DomainDisposition, KhiveConfig};
use rmcp::ErrorData as McpError;
use serde_json::{json, Value};
use tokio::sync::OnceCell;

use super::bridge::ForwardFnPtr;
use super::computer_catalog::{RoutedCatalog, CATALOG_READ_TIMEOUT, VERBS_VERB};
use super::computer_client::{ComputerClient, Connector, ForwardedOptions, HttpConnector};
use super::computer_routing::{
    execute_split, failed_envelope, ComputerRouting, Home, HomeOutcome, HomeSender, Placement,
    RoutePlan, RUNS_ON_COMPUTER,
};
use super::{dsl_err_to_mcp, invalid_request_error, serialize_response_value, KhiveMcpServer};
use crate::tools::request::RequestParams;

/// The caller's computer, as the process that resolved its configuration
/// holds it.
pub(super) struct ComputerBridge {
    pub(super) routing: ComputerRouting,
    pub(super) client: ComputerClient,
    call_timeout: Duration,
    /// How long one read of the computer's catalog may take: the smaller of
    /// `call_timeout` and [`CATALOG_READ_TIMEOUT`] (section 8).
    pub(super) catalog_timeout: Duration,
    routed_requests: AtomicU64,
    /// The computer's catalog as read when the bridge started (section 8).
    pub(super) catalog_at_start: OnceCell<Option<RoutedCatalog>>,
}

impl ComputerBridge {
    /// The computer `config` names, or `None` when it has no `[computer]`
    /// table. Nothing is read from `key_file` and nothing is connected here.
    pub(super) fn from_config(
        config: &KhiveConfig,
        connector: Arc<dyn Connector>,
    ) -> Result<Option<Self>, ConfigError> {
        let Some(computer) = &config.computer else {
            return Ok(None);
        };
        let actor = config
            .actor
            .id
            .clone()
            .ok_or_else(|| ConfigError::InvalidActorComputer {
                reason: "needs [actor].id beside it: the computer's whoami is checked against \
                         that id"
                    .into(),
            })?;
        let routing = ComputerRouting::new(computer)?;
        let client = ComputerClient::new(computer, routing.label().to_owned(), actor, connector);
        Ok(Some(Self {
            routing,
            client,
            call_timeout: Duration::from_secs(computer.call_timeout_s),
            catalog_timeout: Duration::from_secs(computer.call_timeout_s).min(CATALOG_READ_TIMEOUT),
            routed_requests: AtomicU64::new(0),
            catalog_at_start: OnceCell::new(),
        }))
    }

    /// The same bridge with `timeout` as the bound of a catalog read.
    #[cfg(test)]
    pub(super) fn with_catalog_timeout(mut self, timeout: Duration) -> Self {
        self.catalog_timeout = timeout;
        self
    }

    /// `name (host:port)`, the way every message names the computer.
    pub(super) fn label(&self) -> &str {
        self.routing.label()
    }

    /// How many requests with an operation for the computer this process has
    /// handled, refused ones included.
    pub(super) fn routed_requests(&self) -> u64 {
        self.routed_requests.load(Ordering::Relaxed)
    }

    /// The deadline of a request that sends an operation to the computer, or
    /// reads its catalog for `verbs`, which replaces the bridge's own (ADR-220
    /// section 2). `None` for a request with neither, or one that does not
    /// parse.
    pub(super) fn deadline_for(&self, ops: &str) -> Option<Duration> {
        let parsed = parse_request(ops).ok()?;
        let reads_catalog = parsed.ops.iter().any(|op| op.tool == VERBS_VERB);
        (reads_catalog
            || RoutePlan::new(&self.routing, &parsed)
                .units
                .iter()
                .flat_map(|unit| &unit.segments)
                .any(|segment| segment.placement == Placement::Home(Home::Computer)))
        .then_some(self.call_timeout)
    }

    /// `plan`, the answer to `plan=true` for `ops`, with each stage's home:
    /// `served_by` is `store` or `computer`, and a stage the bridge would
    /// refuse has `served_by: null` and `refused: runs_on_computer`. Nothing is
    /// sent anywhere.
    pub(super) fn plan_with_homes(&self, plan: String, ops: &str) -> String {
        let (Ok(mut value), Ok(parsed)) =
            (serde_json::from_str::<Value>(&plan), parse_request(ops))
        else {
            return plan;
        };
        let Some(stages) = value.get_mut("stages").and_then(Value::as_array_mut) else {
            return plan;
        };
        if stages.len() != parsed.ops.len() {
            return plan;
        }
        for (stage, op) in stages.iter_mut().zip(&parsed.ops) {
            match self.routing.place(op) {
                Placement::Home(Home::Store) => stage["served_by"] = Value::from("store"),
                Placement::Home(Home::Computer) => stage["served_by"] = Value::from("computer"),
                Placement::RunsOnComputer => {
                    stage["served_by"] = Value::Null;
                    stage["refused"] = Value::from(RUNS_ON_COMPUTER);
                }
            }
        }
        value.to_string()
    }
}

impl KhiveMcpServer {
    /// Attach the caller's computer from the configuration this process
    /// resolved (ADR-220). A configuration without a `[computer]` table leaves
    /// the server as it was.
    pub(crate) fn with_computer(mut self, config: &KhiveConfig) -> Result<Self, ConfigError> {
        self.computer = ComputerBridge::from_config(config, Arc::new(HttpConnector))?.map(Arc::new);
        Ok(self)
    }

    /// This server without its computer. The daemon serves many callers and
    /// never routes, so a server that becomes the daemon drops it first.
    pub(crate) fn without_computer(mut self) -> Self {
        self.computer = None;
        self
    }

    /// The computer and the plan for `parsed` when at least one of its
    /// operations is not the store's; `None` sends the request to the store
    /// whole, as before ADR-220.
    pub(super) fn computer_route(
        &self,
        parsed: &ParsedRequest,
    ) -> Option<(Arc<ComputerBridge>, RoutePlan)> {
        let computer = self.computer.clone()?;
        let plan = RoutePlan::new(&computer.routing, parsed);
        (plan.whole_home() != Some(Home::Store)).then_some((computer, plan))
    }

    /// Runs a request that has an operation for the computer or one refused
    /// with `runs_on_computer`. A request whose operations are all the
    /// computer's is sent to it whole and its answer returned unchanged; any
    /// other is split by home (ADR-220 section 7).
    pub(super) async fn request_routed(
        &self,
        computer: &ComputerBridge,
        plan: &RoutePlan,
        parsed: &ParsedRequest,
        p: RequestParams,
        forward_fn: ForwardFnPtr,
    ) -> Result<String, McpError> {
        computer.routed_requests.fetch_add(1, Ordering::Relaxed);
        if p.save_to.is_some() {
            return Err(invalid_request_error(format!(
                "save_to is not available for a request with an operation for the computer {}; \
                 nothing was run",
                computer.label()
            )));
        }
        if plan.whole_home() == Some(Home::Computer) {
            let options = forwarded_options(&p, None);
            return Ok(match computer.client.send(p.ops.clone(), &options).await {
                Ok(text) => text,
                Err(failure) => serialize_response_value(&failed_envelope(
                    parsed,
                    failure.error,
                    failure.disposition,
                )),
            });
        }
        let sender = BridgeSender {
            server: self,
            computer,
            request: &p,
            forward_fn,
        };
        let envelope = execute_split(&computer.routing, plan, parsed, &sender).await;
        Ok(serialize_response_value(&envelope))
    }
}

/// The caller's options for a request carrying the operations at `indices`
/// of the caller's request, or all of them.
fn forwarded_options(p: &RequestParams, indices: Option<&[usize]>) -> ForwardedOptions {
    ForwardedOptions {
        presentation: p.presentation.clone(),
        presentation_per_op: cut(&p.presentation_per_op, indices),
        format: p.format.clone(),
        format_per_op: cut(&p.format_per_op, indices),
        request_id: p.request_id,
    }
}

fn cut(
    per_op: &Option<Vec<Option<String>>>,
    indices: Option<&[usize]>,
) -> Option<Vec<Option<String>>> {
    let per_op = per_op.as_ref()?;
    let Some(indices) = indices else {
        return Some(per_op.clone());
    };
    let cut: Vec<Option<String>> = indices
        .iter()
        .map(|&index| per_op.get(index).cloned().flatten())
        .collect();
    cut.iter().any(Option::is_some).then_some(cut)
}

/// Carries each per-home part of a split request: the computer's to the
/// computer, the store's down the store path every request took before.
struct BridgeSender<'a> {
    server: &'a KhiveMcpServer,
    computer: &'a ComputerBridge,
    request: &'a RequestParams,
    forward_fn: ForwardFnPtr,
}

#[async_trait]
impl HomeSender for BridgeSender<'_> {
    async fn send(&self, home: Home, ops: String, indices: Vec<usize>) -> HomeOutcome {
        let options = forwarded_options(self.request, Some(&indices));
        let answer = match home {
            Home::Computer => self
                .computer
                .client
                .send(ops, &options)
                .await
                .map_err(|failure| (failure.error, failure.disposition)),
            Home::Store => self.send_to_store(ops, options).await,
        };
        match answer {
            // A text that is not JSON fails the envelope check as malformed.
            Ok(text) => HomeOutcome::Envelope(serde_json::from_str(&text).unwrap_or(Value::Null)),
            Err((error, disposition)) => HomeOutcome::Failed { error, disposition },
        }
    }
}

impl BridgeSender<'_> {
    async fn send_to_store(
        &self,
        ops: String,
        options: ForwardedOptions,
    ) -> Result<String, (Value, DomainDisposition)> {
        let parsed = parse_request(&ops).map_err(|error| mcp_failure(&dsl_err_to_mcp(error)))?;
        let params = RequestParams {
            plan: None,
            ops,
            presentation: options.presentation,
            presentation_per_op: options.presentation_per_op,
            save_to: None,
            format: options.format,
            format_per_op: options.format_per_op,
            request_id: options.request_id,
        };
        self.server
            .request_on_store(params, &parsed, self.forward_fn)
            .await
            .map_err(|error| mcp_failure(&error))
    }
}

/// The per-operation error and disposition of a store part that failed as a
/// whole.
fn mcp_failure(error: &McpError) -> (Value, DomainDisposition) {
    let data = error.data.clone().unwrap_or(Value::Null);
    let disposition = match data.get("domain_disposition").and_then(Value::as_str) {
        Some("not_committed") => DomainDisposition::NotCommitted,
        Some("committed") => DomainDisposition::Committed,
        _ => DomainDisposition::Unknown,
    };
    let error = if data.get("kind").and_then(Value::as_str).is_some() {
        data
    } else {
        json!({ "kind": "internal_error", "message": error.message })
    };
    (error, disposition)
}

#[cfg(all(test, unix))]
#[path = "computer_bridge_tests.rs"]
mod tests;
