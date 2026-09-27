//! Bounded JSON-RPC line reader in front of rmcp's unbounded `read_until`.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncBufRead, AsyncRead, BufReader, ReadBuf};

/// Allows a maximally JSON-escaped 1 MiB `ops` string plus the MCP envelope.
const DEFAULT_MAX_LINE_BYTES: usize = khive_request::MAX_OPS_INPUT_LEN * 8 + 64 * 1024;
const MAX_CONFIGURED_LINE_BYTES: usize = 64 * 1024 * 1024;
const ENV_KEY: &str = "KHIVE_MCP_STDIO_MAX_LINE_BYTES";

pub(crate) fn max_line_bytes_from_env() -> anyhow::Result<usize> {
    match std::env::var(ENV_KEY) {
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_MAX_LINE_BYTES),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("{ENV_KEY} must be a UTF-8 integer from 1 to {MAX_CONFIGURED_LINE_BYTES}")
        }
        Ok(raw) => parse_max_line_bytes(&raw),
    }
}

fn parse_max_line_bytes(raw: &str) -> anyhow::Result<usize> {
    let value = raw.trim().parse::<usize>().ok();
    match value {
        Some(value @ 1..=MAX_CONFIGURED_LINE_BYTES) => Ok(value),
        _ => anyhow::bail!(
            "{ENV_KEY}={raw:?} is invalid: expected an integer from 1 to {MAX_CONFIGURED_LINE_BYTES} bytes"
        ),
    }
}

/// An overlong line is replaced with a short, invalid JSON line. rmcp emits
/// its ordinary JSON-RPC parse error (without an id) for that sentinel. The reader
/// then drains the rest of the offending line before forwarding the next one.
/// This keeps both our staging buffer and rmcp's line buffer bounded, even if
/// the peer sends no newline after crossing the limit.
pub(crate) struct BoundedLineReader<R> {
    inner: BufReader<R>,
    max_line_bytes: usize,
    line: Vec<u8>,
    ready: Vec<u8>,
    ready_offset: usize,
    discarding: bool,
    eof: bool,
}

impl<R: AsyncRead> BoundedLineReader<R> {
    pub(crate) fn new(inner: R, max_line_bytes: usize) -> Self {
        assert!(max_line_bytes > 0, "stdio line limit must be positive");
        Self {
            inner: BufReader::new(inner),
            max_line_bytes,
            line: Vec::new(),
            ready: Vec::new(),
            ready_offset: 0,
            discarding: false,
            eof: false,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedLineReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        dst: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if dst.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if this.ready_offset < this.ready.len() {
                let remaining = &this.ready[this.ready_offset..];
                let n = remaining.len().min(dst.remaining());
                dst.put_slice(&remaining[..n]);
                this.ready_offset += n;
                return Poll::Ready(Ok(()));
            }
            this.ready.clear();
            this.ready_offset = 0;
            if this.eof {
                return Poll::Ready(Ok(()));
            }

            let input = match Pin::new(&mut this.inner).poll_fill_buf(cx) {
                Poll::Ready(Ok(input)) => input,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            };
            if input.is_empty() {
                this.eof = true;
                if !this.discarding && !this.line.is_empty() {
                    std::mem::swap(&mut this.line, &mut this.ready);
                    continue;
                }
                return Poll::Ready(Ok(()));
            }

            let newline = input.iter().position(|&byte| byte == b'\n');
            let consumed = newline.map_or(input.len(), |position| position + 1);
            if this.discarding {
                Pin::new(&mut this.inner).consume(consumed);
                if newline.is_some() {
                    this.discarding = false;
                }
                continue;
            }
            if consumed > this.max_line_bytes.saturating_sub(this.line.len()) {
                this.line.clear();
                this.discarding = newline.is_none();
                Pin::new(&mut this.inner).consume(consumed);
                this.ready.extend_from_slice(b"!\n");
                continue;
            }
            this.line.extend_from_slice(&input[..consumed]);
            Pin::new(&mut this.inner).consume(consumed);
            if newline.is_some() {
                std::mem::swap(&mut this.line, &mut this.ready);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::transport::{async_rw::AsyncRwTransport, Transport as _};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    #[test]
    fn configured_limit_remains_finite() {
        assert_eq!(parse_max_line_bytes("96").unwrap(), 96);
        assert!(parse_max_line_bytes("0").is_err());
        assert!(parse_max_line_bytes("67108865").is_err());
        assert!(parse_max_line_bytes("not-a-number").is_err());
    }

    #[tokio::test]
    async fn oversized_unterminated_line_gets_error_then_next_request_arrives() {
        let (server_io, client_io) = tokio::io::duplex(256);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (client_read, mut client_write) = tokio::io::split(client_io);
        let mut transport =
            AsyncRwTransport::new_server(BoundedLineReader::new(server_read, 96), server_write);
        let receive = tokio::spawn(async move { transport.receive().await });

        // No newline: the cap itself must trigger rmcp's parse error instead
        // of waiting for an unbounded `read_until` to finish the line.
        client_write.write_all(&[b'x'; 97]).await.unwrap();
        let mut client_read = BufReader::new(client_read);
        let mut reply = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_read.read_line(&mut reply),
        )
        .await
        .expect("oversized unterminated line must get a prompt error")
        .unwrap();
        let response: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(response["error"]["code"].as_i64(), Some(-32700));
        assert!(response.get("id").is_none());

        client_write
            .write_all(b"ignored remainder\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        let message = tokio::time::timeout(std::time::Duration::from_secs(2), receive)
            .await
            .expect("next line must be received")
            .unwrap()
            .expect("valid request must remain in the session");
        assert!(matches!(
            message,
            rmcp::model::JsonRpcMessage::Request(request)
                if request.id == rmcp::model::RequestId::Number(1)
        ));
    }
}
