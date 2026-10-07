//! MCP server adapter for the runtime daemon dispatch contract.

use super::{async_trait, daemon, RequestParams};

#[async_trait]
impl daemon::DaemonDispatch for crate::server::KhiveMcpServer {
    fn plan(&self, ops: &str) -> String {
        self.plan_ops(ops)
    }

    fn request_read_timeout(&self, ops: &str) -> std::time::Duration {
        crate::request_policy::read_timeout(ops, khive_storage::request_read_timeout_from_env())
    }

    async fn dispatch(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
    ) -> Result<String, String> {
        self.dispatch_with_error_detail(
            ops,
            presentation,
            presentation_per_op,
            format,
            format_per_op,
            from_wire,
            identity,
        )
        .await
        .map_err(|error| error.message)
    }

    async fn dispatch_with_error_detail(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
    ) -> Result<String, daemon::DaemonDispatchError> {
        if khive_request::parse_request(&ops)
            .is_ok_and(|parsed| parsed.ops.iter().any(|op| op.tool == "bridge.diagnostics"))
        {
            return Err(daemon::DaemonDispatchError::new(
                "bridge.diagnostics is available only on the stdio bridge".to_string(),
                Some(serde_json::json!({
                    "kind": "invalid_input",
                    "message": "bridge.diagnostics is available only on the stdio bridge",
                })),
            ));
        }
        let params = RequestParams {
            plan: None,
            ops,
            presentation,
            presentation_per_op,
            save_to: None,
            format,
            format_per_op,
            request_id: None,
        };
        // Honor the frame's origin: a wire-origin request enforces verb
        // visibility even when served by the daemon; an operator request does not.
        // `identity` (ADR-096 Fork 1) is the caller's per-request identity
        // context, built by `handle_conn` from the frame — threaded straight
        // through so this call serves under the CALLER's namespace/actor
        // rather than this server's own construction-baked identity.
        self.dispatch_request_inner(
            params,
            from_wire,
            identity,
            crate::server::DispatchOrigin::Daemon,
        )
        .await
        .map_err(|error| daemon::DaemonDispatchError::new(error.message.to_string(), error.data))
    }

    async fn warm_all(&self) {
        crate::server::KhiveMcpServer::warm_all(self).await;
    }

    fn namespace(&self) -> &str {
        self.default_namespace()
    }

    fn config_id(&self) -> &str {
        crate::server::KhiveMcpServer::config_id(self)
    }

    fn pool_for_checkpoint(&self) -> Option<std::sync::Arc<khive_db::ConnectionPool>> {
        self.pool()
    }

    fn idle_retirement_blockers(&self) -> Vec<String> {
        let main = self.pool();
        let mut blockers: Vec<String> = main
            .clone()
            .into_iter()
            .chain(self.secondary_pools())
            .enumerate()
            .filter_map(|(index, pool)| {
                (pool.retirement_writer_holds() != 0)
                    .then(|| format!("backend:{index}:held_writer"))
            })
            .collect();
        if main.is_none() {
            blockers.push("main_backend_pool_inventory_unavailable".to_owned());
        }
        blockers
    }

    fn secondary_pools_for_checkpoint(&self) -> Vec<std::sync::Arc<khive_db::ConnectionPool>> {
        self.secondary_pools()
    }

    fn event_store_for_checkpoint(&self) -> Option<std::sync::Arc<dyn khive_storage::EventStore>> {
        self.event_store()
    }
}
