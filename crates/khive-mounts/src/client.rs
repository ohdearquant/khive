use std::{collections::BTreeSet, process::Stdio, sync::Arc, time::Duration};

use khive_runtime::{mount_config::MountConfig, mounted_verb::MountedVerb};
use khive_storage::SqlAccess;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};

use crate::{catalog, error::Failure, store};

const MAX_FRAME: usize = 4 * 1024 * 1024;
const MAX_TOOLS: usize = 1024;

pub(crate) struct Client {
    sender: mpsc::Sender<Job>,
    task: JoinHandle<()>,
    timeout_ms: u64,
}
impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum Operation {
    Catalog,
    Invoke {
        definition: MountedVerb,
        arguments: Value,
        sql: Arc<dyn SqlAccess>,
    },
}
struct Job {
    operation: Operation,
    reply: oneshot::Sender<Result<Value, Failure>>,
}

impl Client {
    pub async fn start(config: MountConfig) -> Result<Self, Failure> {
        let connection = Connection::start(&config).await?;
        let (sender, receiver) = mpsc::channel(32);
        let timeout_ms = config.timeout_ms;
        let task = tokio::spawn(supervise(config, connection, receiver));
        Ok(Self {
            sender,
            task,
            timeout_ms,
        })
    }
    async fn request(&self, operation: Operation) -> Result<Value, Failure> {
        let (reply, receiver) = oneshot::channel();
        self.sender
            .try_send(Job { operation, reply })
            .map_err(|_| Failure::error("mount_busy"))?;
        timeout(Duration::from_millis(self.timeout_ms), receiver)
            .await
            .map_err(|_| Failure::timeout())?
            .map_err(|_| Failure::error("mount_down"))?
    }
    pub async fn catalog(&self) -> Result<Vec<Value>, Failure> {
        let value = self.request(Operation::Catalog).await?;
        value.as_array().cloned().ok_or_else(Failure::malformed)
    }
    pub async fn invoke(
        &self,
        definition: MountedVerb,
        arguments: Value,
        sql: Arc<dyn SqlAccess>,
    ) -> Result<Value, Failure> {
        self.request(Operation::Invoke {
            definition,
            arguments,
            sql,
        })
        .await
    }
}

async fn supervise(config: MountConfig, connection: Connection, mut receiver: mpsc::Receiver<Job>) {
    let mut connection = Some(connection);
    let mut restarted = false;
    loop {
        let Some(current) = connection.as_mut() else {
            while let Some(job) = receiver.recv().await {
                let _ = job.reply.send(Err(Failure::error("mount_down")));
            }
            return;
        };
        let restart = tokio::select! {
            _ = current.child.wait() => true,
            job = receiver.recv() => {
                let Some(job) = job else { return };
                if job.reply.is_closed() { continue; }
                let result = timeout(Duration::from_millis(config.timeout_ms), current.perform(&config, job.operation)).await.unwrap_or_else(|_| Err(Failure::timeout()));
                let fatal = result.as_ref().err().is_some_and(|error| error.fatal);
                let _ = job.reply.send(result);
                fatal
            }
        };
        if restart {
            if let Some(mut old) = connection.take() {
                let _ = old.child.kill().await;
            }
            if !restarted {
                restarted = true;
                tracing::warn!(mount = %config.name, restart = 1, "mounted process restarting");
                connection = Connection::start(&config).await.ok();
            } else {
                tracing::warn!(mount = %config.name, reason = "mount_down", "mounted process exhausted its restart");
            }
        }
    }
}

struct Connection {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    sequence: u64,
}
impl Connection {
    async fn start(config: &MountConfig) -> Result<Self, Failure> {
        timeout(Duration::from_millis(config.timeout_ms), async {
            tracing::info!(mount = %config.name, credential = config.credential.as_deref(), "starting mounted source");
            let mut command = Command::new(&config.command);
            command.args(&config.args).env_clear().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
            for name in config.env.iter().chain(config.credential.iter()) {
                if let Some(value) = std::env::var_os(name) { command.env(name, value); }
                else if config.credential.as_ref() == Some(name) { return Err(Failure::error("credential_unavailable")); }
            }
            let mut child = command.spawn().map_err(|_| Failure::error("mount_down"))?;
            let stdin = child.stdin.take().ok_or_else(Failure::io)?;
            let stdout = BufReader::new(child.stdout.take().ok_or_else(Failure::io)?);
            let mut connection = Self { child, stdin, stdout, sequence: 0 };
            let init = connection.rpc("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "khive-mounts", "version": env!("CARGO_PKG_VERSION")}})).await?;
            if !matches!(init.get("protocolVersion").and_then(Value::as_str), Some("2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25")) || !init.get("capabilities").is_some_and(Value::is_object) || !init.get("serverInfo").is_some_and(Value::is_object) {
                return Err(Failure::malformed());
            }
            connection.write(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await?;
            Ok(connection)
        }).await.unwrap_or_else(|_| Err(Failure::timeout()))
    }
    async fn write(&mut self, value: &Value) -> Result<(), Failure> {
        let mut bytes = serde_json::to_vec(value).map_err(|_| Failure::malformed())?;
        if bytes.len() > MAX_FRAME {
            return Err(Failure::error("request_too_large"));
        }
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .map_err(|_| Failure::io())?;
        self.stdin.flush().await.map_err(|_| Failure::io())
    }
    async fn read(&mut self) -> Result<Value, Failure> {
        let mut bytes = Vec::new();
        loop {
            let buffer = self.stdout.fill_buf().await.map_err(|_| Failure::io())?;
            if buffer.is_empty() {
                return Err(Failure::io());
            }
            let take = buffer
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(buffer.len(), |pos| pos + 1);
            if bytes.len() + take > MAX_FRAME {
                return Err(Failure::malformed());
            }
            bytes.extend_from_slice(&buffer[..take]);
            self.stdout.consume(take);
            if bytes.last() == Some(&b'\n') {
                break;
            }
        }
        serde_json::from_slice(&bytes).map_err(|_| Failure::malformed())
    }
    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value, Failure> {
        self.sequence += 1;
        let id = self.sequence;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        for _ in 0..128 {
            let reply = self.read().await?;
            if reply.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                return Err(Failure::malformed());
            }
            if reply.get("id").is_none() && reply.get("method").and_then(Value::as_str).is_some() {
                continue;
            }
            if reply.get("id").and_then(Value::as_u64) != Some(id)
                || reply.get("method").is_some()
                || reply.get("result").is_some() == reply.get("error").is_some()
            {
                return Err(Failure::malformed());
            }
            if reply.get("error").is_some() {
                return Err(Failure::error("foreign_error"));
            }
            return reply.get("result").cloned().ok_or_else(Failure::malformed);
        }
        Err(Failure::malformed())
    }
    async fn catalog(&mut self) -> Result<Vec<Value>, Failure> {
        let mut tools = Vec::new();
        let mut cursor = None;
        let mut cursors = BTreeSet::new();
        for _ in 0..32 {
            let params = cursor
                .as_ref()
                .map_or(json!({}), |cursor| json!({"cursor": cursor}));
            let result = self.rpc("tools/list", params).await?;
            let page = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(Failure::malformed)?;
            if tools.len() + page.len() > MAX_TOOLS {
                return Err(Failure::malformed());
            }
            tools.extend(page.iter().cloned());
            match result.get("nextCursor") {
                None => return Ok(tools),
                Some(Value::String(value))
                    if !value.is_empty() && cursors.insert(value.clone()) =>
                {
                    cursor = Some(value.clone())
                }
                _ => return Err(Failure::malformed()),
            }
        }
        Err(Failure::malformed())
    }
    async fn perform(
        &mut self,
        config: &MountConfig,
        operation: Operation,
    ) -> Result<Value, Failure> {
        let Operation::Invoke {
            definition,
            arguments,
            sql,
        } = operation
        else {
            return self.catalog().await.map(Value::Array);
        };
        if !catalog::validates(&definition.input_schema, &arguments) {
            return Err(Failure::error("invalid_arguments"));
        }
        let published = self.catalog().await.map_err(|mut error| {
            error.reason = "catalog_drift";
            error
        })?;
        let mut selected = config.clone();
        selected.tools.retain(|tool| tool.name == definition.name);
        // A newly pinned identifier may be added by the operator without restarting this client.
        if selected.tools.is_empty() {
            selected
                .tools
                .push(khive_runtime::mount_config::MountToolConfig {
                    name: definition.name.clone(),
                    effect: definition.effect,
                });
        }
        let live = catalog::pin(&selected, &published, definition.generation)
            .map_err(|_| Failure::error("catalog_drift"))?;
        if live
            .first()
            .is_none_or(|tool| tool.digest != definition.digest)
        {
            return Err(Failure::error("catalog_drift"));
        }
        let current = store::load(&sql, &config.name)
            .await
            .map_err(|_| Failure::error("catalog_drift"))?;
        if current.is_none_or(|(generation, _)| generation != definition.generation) {
            return Err(Failure::error("catalog_drift"));
        }
        let result = self
            .rpc(
                "tools/call",
                json!({"name": definition.name, "arguments": arguments}),
            )
            .await?;
        let parsed: rmcp::model::CallToolResult =
            serde_json::from_value(result.clone()).map_err(|_| Failure::malformed())?;
        if parsed.is_error == Some(true) {
            return Err(Failure::error("foreign_error"));
        }
        if let Some(schema) = &definition.output_schema {
            if !result
                .get("structuredContent")
                .is_some_and(|value| catalog::validates(schema, value))
            {
                return Err(Failure::malformed());
            }
        }
        Ok(result)
    }
}
