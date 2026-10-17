use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};

use super::{FIXTURE_ROOT, HELPER_MARKER, PROTOCOL_DEADLINE};

const EXIT_DEADLINE: Duration = Duration::from_secs(10);
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_FRAMES: usize = 16;

/// Own the OS child until wait/try_wait reaps it. No assertion or `?` after
/// spawn can abandon a live helper: unwind/error also kills and waits.
struct ReapedChild {
    child: Child,
    reaped: bool,
}

impl ReapedChild {
    async fn finish(&mut self) -> anyhow::Result<ExitStatus> {
        let deadline = Instant::now() + EXIT_DEADLINE;
        loop {
            if self.child.try_wait()?.is_some() {
                let status = self.child.wait()?;
                self.reaped = true;
                return Ok(status);
            }
            ensure!(
                Instant::now() < deadline,
                "helper failed to exit after stdin EOF"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

impl Drop for ReapedChild {
    fn drop(&mut self) {
        if !self.reaped {
            // Best effort kill, mandatory wait attempt even if kill loses a race
            // with normal exit. OS-level uninterruptible I/O is not a wall bound.
            let _ = self.child.kill();
            if let Err(error) = self.child.wait() {
                eprintln!("startup helper reap failed: {error}");
            }
        }
    }
}

pub(super) async fn round(root: &Path) -> anyhow::Result<(Duration, Value)> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .env_clear()
        // Keep dynamic loader paths, never ambient credentials or khive config.
        .envs(std::env::vars_os().filter(|(key, _)| {
            matches!(
                key.to_str(),
                Some(
                    "PATH" | "DYLD_FALLBACK_LIBRARY_PATH" | "LD_LIBRARY_PATH" | "LLVM_PROFILE_FILE"
                )
            )
        }))
        .env("KHIVE_TEST_HARNESS", "1")
        .env(HELPER_MARKER, "1")
        .env(FIXTURE_ROOT, root)
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("APPDATA", root.join("config"))
        .env("LOCALAPPDATA", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        .env("TMP", root.join("tmp"))
        .env("TEMP", root.join("tmp"))
        .env("KHIVE_VOLUME_LOCK_DIR", root.join("volume-locks"))
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_EVENTS_SPLIT", "0")
        .current_dir(root.join("work"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // No unconsumed stderr pipe can fill and deadlock the helper.
        .stderr(Stdio::inherit());
    #[cfg(windows)]
    {
        // Windows side-by-side loading needs SystemRoot with a custom env.
        command.env(
            "SystemRoot",
            std::env::var_os("SystemRoot").context("Windows SystemRoot")?,
        );
        if let Some(windir) = std::env::var_os("WINDIR") {
            command.env("WINDIR", windir);
        }
    }
    let started = Instant::now();
    let mut child = ReapedChild {
        child: command.spawn()?,
        reaped: false,
    };
    let response =
        tokio::time::timeout(PROTOCOL_DEADLINE, exchange(&mut child.child, started)).await;
    // exchange owns the pipe handles. Completion/error/timeout drops stdin
    // before this untimed wait, delivering real EOF to serve_server.
    let status = child.finish().await?;
    let (elapsed, response) = response.context("startup protocol deadline")??;
    ensure!(status.success(), "helper exited unsuccessfully: {status}");
    Ok((elapsed, response))
}

async fn exchange(child: &mut Child, started: Instant) -> anyhow::Result<(Duration, Value)> {
    let mut input = ChildStdin::from_std(child.stdin.take().context("helper stdin pipe")?)?;
    let output = ChildStdout::from_std(child.stdout.take().context("helper stdout pipe")?)?;
    let mut output = BufReader::new(output);
    write_frame(
        &mut input,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "khive-startup-benchmark", "version": "1"}
            }
        }),
    )
    .await?;
    let (initialized, _) = read_response(&mut output, 1).await?;
    ensure!(
        initialized.get("error").is_none(),
        "initialize error: {initialized}"
    );
    ensure!(
        initialized["result"]["protocolVersion"] == "2025-11-25",
        "initialize result: {initialized}"
    );
    write_frame(
        &mut input,
        &json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }),
    )
    .await?;
    write_frame(
        &mut input,
        &json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "request", "arguments": {
                "ops": "stats()", "presentation": "verbose", "format": "json"
            }}
        }),
    )
    .await?;
    let (response, arrived) = read_response(&mut output, 2).await?;
    // arrived was captured at the full line boundary, before parsing this final
    // response. Fixture assertions and stdin close / reap are all untimed.
    let elapsed = arrived.duration_since(started);
    drop(input);
    Ok((elapsed, response))
}

async fn write_frame(input: &mut ChildStdin, value: &Value) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    input.write_all(&bytes).await?;
    input.flush().await?;
    Ok(())
}

async fn read_response(
    output: &mut BufReader<ChildStdout>,
    id: u64,
) -> anyhow::Result<(Value, Instant)> {
    for _ in 0..MAX_RESPONSE_FRAMES {
        let mut frame = Vec::new();
        loop {
            let available = output.fill_buf().await?;
            ensure!(
                !available.is_empty(),
                "helper closed stdout before response {id}"
            );
            let newline = available.iter().position(|&byte| byte == b'\n');
            let count = newline.map_or(available.len(), |index| index + 1);
            ensure!(
                frame.len() + count <= MAX_FRAME_BYTES,
                "MCP frame exceeds benchmark cap"
            );
            frame.extend_from_slice(&available[..count]);
            output.consume(count);
            if newline.is_some() {
                break;
            }
        }
        let arrived = Instant::now();
        let value: Value = serde_json::from_slice(&frame)?;
        if value["id"] == id {
            return Ok((value, arrived));
        }
        ensure!(
            value.get("id").is_none() && value["method"].is_string(),
            "unexpected MCP frame: {value}"
        );
    }
    bail!("too many notifications before response {id}")
}
