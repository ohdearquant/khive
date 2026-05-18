//! KhiveMcpServer — rmcp-based MCP server exposing a single `request` tool.
//!
//! The MCP surface is intentionally minimal: one tool (`request`) that accepts
//! a function-call DSL or JSON form (ADR-020) and dispatches each parsed
//! operation through the [`VerbRegistry`] built from the packs declared in
//! [`khive_runtime::RuntimeConfig::packs`].
//!
//! ## Why a single tool
//!
//! As of v0.2 the verb-flat surface (ADR-023) is folded behind `request`. The
//! verb catalog lives in:
//!
//! - the `request` tool's description (terse list, per-pack);
//! - each pack's marketplace plugin SKILL.md files (rich usage guides).
//!
//! Tool discovery happens once per session anyway, so collapsing 16+ flat tools
//! into one keeps tool-list latency low and frees agent context budget while
//! preserving expressiveness through the DSL.

use std::collections::HashMap;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde_json::{json, Value};

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_request::{parse_request, DslError, ParsedOp};
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};

use crate::tools::request::RequestParams;

/// MCP server that dispatches all verbs through a [`VerbRegistry`].
#[derive(Clone)]
pub struct KhiveMcpServer {
    registry: VerbRegistry,
}

/// Returned by [`KhiveMcpServer::with_packs`] when a name in the requested pack
/// list doesn't map to a known built-in pack. The original runtime is returned
/// so the caller can recover (e.g. retry with a smaller list).
pub struct PackRegError {
    pub unknown: String,
    pub runtime: KhiveRuntime,
}

impl std::fmt::Debug for PackRegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackRegError")
            .field("unknown", &self.unknown)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for PackRegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown pack name {:?} — built-in packs: kg, gtd",
            self.unknown
        )
    }
}

impl std::error::Error for PackRegError {}

/// Built-in pack names known to this binary. Kept in sync with
/// [`KhiveMcpServer::with_packs`] so error messages and CLI help stay accurate.
pub const BUILTIN_PACKS: &[&str] = &["kg", "gtd"];

impl KhiveMcpServer {
    /// Build a server using the pack list from `runtime.config().packs`.
    ///
    /// Always returns a server. Unknown pack names are logged via `tracing::warn!`
    /// rather than rejected — startup must remain robust if a future binary drops
    /// a pack that an older config still names. Use [`Self::with_packs`] for
    /// strict validation in tests / programmatic callers.
    pub fn new(runtime: KhiveRuntime) -> Self {
        let packs: Vec<String> = runtime.config().packs.clone();
        Self::with_packs(runtime, &packs).unwrap_or_else(|err| {
            tracing::warn!("pack registration: {err}; falling back to kg only");
            let mut builder = VerbRegistryBuilder::new();
            builder.register(KgPack::new(err.runtime));
            Self {
                registry: builder.build(),
            }
        })
    }

    /// Build a server with an explicit pack list (strict — fails on unknown names).
    // The error variant intentionally carries the runtime by value so callers
    // can recover and retry. Boxing would force every recovery path through a
    // deref for no real benefit.
    #[allow(clippy::result_large_err)]
    pub fn with_packs(runtime: KhiveRuntime, packs: &[String]) -> Result<Self, PackRegError> {
        let mut builder = VerbRegistryBuilder::new();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for name in packs {
            if !seen.insert(name.as_str()) {
                continue;
            }
            match name.as_str() {
                "kg" => {
                    builder.register(KgPack::new(runtime.clone()));
                }
                "gtd" => {
                    builder.register(GtdPack::new(runtime.clone()));
                }
                other => {
                    return Err(PackRegError {
                        unknown: other.to_string(),
                        runtime,
                    });
                }
            }
        }
        Ok(Self {
            registry: builder.build(),
        })
    }

    /// Serve over stdio (blocks until the connection closes).
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        use rmcp::transport::stdio;
        let service = self.serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    }

    /// Build the textual verb catalog included in the request tool's description.
    ///
    /// The list is rebuilt from the runtime registry so it always reflects which
    /// packs are actually loaded.
    fn verb_catalog(&self) -> String {
        let mut by_verb: HashMap<&str, &str> = HashMap::new();
        for v in self.registry.all_verbs() {
            // first registered pack's description wins on collision
            by_verb.entry(v.name).or_insert(v.description);
        }
        let mut entries: Vec<(&&str, &&str)> = by_verb.iter().collect();
        entries.sort_by_key(|(n, _)| **n);
        let mut out = String::new();
        for (name, desc) in entries {
            out.push_str("  ");
            out.push_str(name);
            out.push_str(" — ");
            out.push_str(desc);
            out.push('\n');
        }
        out
    }

    /// Run a parsed batch in parallel, gathering per-op results in input order.
    async fn run_parsed(&self, ops: Vec<ParsedOp>) -> Value {
        let futures = ops.into_iter().map(|op| {
            let registry = self.registry.clone();
            async move {
                let ParsedOp { tool, args } = op;
                let args_value = Value::Object(args);
                match registry.dispatch(&tool, args_value).await {
                    Ok(result) => json!({ "ok": true, "tool": tool, "result": result }),
                    Err(e) => json!({ "ok": false, "tool": tool, "error": e.to_string() }),
                }
            }
        });
        let results: Vec<Value> = futures::future::join_all(futures).await;
        let total = results.len();
        let succeeded = results
            .iter()
            .filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true))
            .count();
        let failed = total - succeeded;
        json!({
            "results": results,
            "summary": { "total": total, "succeeded": succeeded, "failed": failed },
        })
    }
}

// ── single MCP tool ─────────────────────────────────────────────────────────

#[tool_router]
impl KhiveMcpServer {
    #[tool(description = r#"Run one or more khive verbs in a single MCP call.

ops syntax (ADR-020):

  Single op   : verb(name=value, name=value)
  Batch       : [verb(...), verb(...)]                 — parallel, max 100
  JSON form   : [{"tool":"verb","args":{...}}, ...]    — equivalent

Argument values are JSON literals: strings (double-quoted), numbers, booleans,
null, arrays, objects. Strings may contain commas / parens; escape with \".

Response shape:

  {
    "results": [ {"ok": true, "tool": "verb", "result": {...}}, ... ],
    "summary": { "total": N, "succeeded": N, "failed": N }
  }

A failed op does NOT abort the batch. Each entry has its own ok / error.

Verb discovery: install the `kg` / `gtd` plugins for usage skills. The verbs
currently registered on this server (pack-derived) are listed below. Argument
schemas live in each pack's docs and SKILL.md files.

Tip: for one-shot calls, the single-op form is the densest. Use batch when
several independent ops can run together (e.g. bulk create + link)."#)]
    async fn request(&self, Parameters(p): Parameters<RequestParams>) -> Result<String, McpError> {
        let parsed = parse_request(&p.ops).map_err(dsl_err_to_mcp)?;
        let result = self.run_parsed(parsed.ops).await;
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))
    }
}

fn dsl_err_to_mcp(e: DslError) -> McpError {
    McpError::invalid_params(e.to_string(), None)
}

#[tool_handler]
impl ServerHandler for KhiveMcpServer {
    fn get_info(&self) -> ServerInfo {
        let catalog = self.verb_catalog();
        let instructions = format!(
            "khive — request-only MCP surface (ADR-020 + ADR-025). One tool, `request`, \
             dispatches verbs through the loaded pack registry. Configure packs via \
             KHIVE_PACKS or --pack (built-ins: kg, gtd). Verbs registered on this \
             server:\n{catalog}\nFor detailed usage of each verb, see the corresponding \
             plugin's SKILL.md files."
        );
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(instructions)
    }
}
