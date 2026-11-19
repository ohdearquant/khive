mod fixture;
mod process;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{ensure, Context};
use clap::Parser;
use khive_mcp::args::Args;
use khive_mcp::serve::{build_server, serve_server};
use khive_mcp::transport::TransportRegistry;
use serde_json::Value;

pub const HELPER_MARKER: &str = "KHIVE_MCP_BENCH_STARTUP_HELPER";
const FIXTURE_ROOT: &str = "KHIVE_MCP_BENCH_FIXTURE_ROOT";
pub(super) const PROTOCOL_DEADLINE: Duration = Duration::from_secs(30);

pub fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .expect("startup benchmark executor")
}

pub fn helper() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::var_os(FIXTURE_ROOT).context("fixture root")?);
    let db = root.join("store/kg.db");
    let config = root.join("config.toml");
    // Explicit parsed arguments exercise the production bootstrap, including DB
    // open and registry construction. No fixture generation happens in this arm.
    let args = Args::try_parse_from([
        std::ffi::OsStr::new("mcp-startup-helper"),
        std::ffi::OsStr::new("--db"),
        db.as_os_str(),
        std::ffi::OsStr::new("--config"),
        config.as_os_str(),
        std::ffi::OsStr::new("--actor"),
        std::ffi::OsStr::new("local"),
        std::ffi::OsStr::new("--pack"),
        std::ffi::OsStr::new("kg"),
        std::ffi::OsStr::new("--no-embed"),
        std::ffi::OsStr::new("--transport"),
        std::ffi::OsStr::new("stdio"),
    ])?;
    executor().block_on(async {
        let (server, schedule_runtime) = build_server(&args).await?;
        ensure!(
            schedule_runtime.is_none(),
            "KG-only startup has no scheduler"
        );
        serve_server(
            server,
            &args,
            &TransportRegistry::with_builtins(),
            None,
            None,
        )
        .await
    })
}

pub async fn sample() -> anyhow::Result<Duration> {
    let fixture = fixture::Fixture::new().await?;
    let (elapsed, response) = process::round(&fixture.root).await?;
    // round has already closed stdin, observed exit0 and reaped the helper.
    // Correctness and fixture cleanup do not enter the measured duration.
    validate_response(&response)?;
    fixture.close()?;
    Ok(elapsed)
}

fn validate_response(response: &Value) -> anyhow::Result<()> {
    ensure!(
        response["jsonrpc"] == "2.0" && response["id"] == 2,
        "call envelope: {response}"
    );
    ensure!(response.get("error").is_none(), "RPC error: {response}");
    let result = &response["result"];
    ensure!(result["isError"] == false, "tool error: {response}");
    let content = result["content"].as_array().context("tool content array")?;
    ensure!(
        content.len() == 1 && content[0]["type"] == "text",
        "tool text: {response}"
    );
    let body: Value =
        serde_json::from_str(content[0]["text"].as_str().context("tool text string")?)?;
    let rows = body["results"].as_array().context("operation results")?;
    ensure!(
        rows.len() == 1 && rows[0]["tool"] == "stats" && rows[0]["ok"] == true,
        "stats receipt: {body}"
    );
    let stats = &rows[0]["result"];
    ensure!(
        stats["entities"] == 100 && stats["edges"] == 0 && stats["notes"] == 0,
        "fixture counts: {stats}"
    );
    ensure!(
        stats["count_scope"]["namespaces"] == "caller_visible"
            && stats["count_scope"]["rows"] == "live_only",
        "count scope: {stats}"
    );
    Ok(())
}
