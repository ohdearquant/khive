# HTTP and MCP clients

Install `khive-py[cloud]` for HTTP and MCP support.

`khive.HttpTransport` is synchronous and implements the `Transport` interface
(`round_trip(frame, timeout)`). Pass it to `khive.Khive`, or use `khive.cloud()`.
It also offers `send_dsl(ops, timeout=...)` for a verbatim request string and
`close()`/a synchronous context manager for cleanup.

`khive.AsyncHttpTransport` exposes async `round_trip` and `aclose`, and an async
context manager. Use it directly from an event loop; the synchronous `Khive`
facade does not drive it.

REST requests use `POST /v1/request`; health checks use `GET /health`. Credentials
travel in `Authorization: ApiKey <key>`, never in the request body.

| HTTP status | REST exception |
| ----------- | -------------- |
| 401 or 403  | `AuthError`    |
| 429         | `RateLimited`  |
| Other 4xx   | `BadRequest`   |
| 5xx         | `ServerError`  |

These exceptions subclass `HttpError`, itself a `TransportError`. A successful
HTTP response may still contain per-operation errors; those remain in the result
envelope. There is no automatic retry.

The `khive.mcp` helpers use the streamable HTTP endpoint `/mcp`:

- `mcp_session`: async context manager yielding an initialized MCP session.
- `alist_tool_names` and `acall_request`: async helpers.
- `mcp_list_tools` and `mcp_request`: synchronous helpers; use the async equivalents
  inside a running event loop.

MCP uses the same authentication header and URL security check. Its helpers
translate authentication failures to `AuthError`; other SDK failures may surface
as `KhiveError`. The REST status table above does not promise identical MCP SDK
error classification.

Plain `http://` is refused unless the host is loopback (`127.0.0.1`, `::1` or
`localhost`). Pass `allow_insecure=True` explicitly to permit another HTTP host.

## Configuration

`khive.cloud(base_url=None, api_key=None, allow_insecure=False, ...)` resolves each
value independently: an explicit argument wins; otherwise it reads
`KHIVE_CLOUD_URL` or `KHIVE_CLOUD_API_KEY`. Missing values raise `ValueError` naming
the argument and environment variable. **There is no default base URL.** Empty
explicit values are configuration errors rather than environment fallbacks.
The transport constructors and MCP helpers require explicit URL and credential
arguments and do not read these environment variables.

```python
import khive

db = khive.cloud()  # requires both environment variables
db.whoami()
db.session.transport.close()
```

## Command line

```sh
khive-cloud whoami
khive-cloud exec 'whoami() | stats()'
khive-cloud --url https://example.invalid whoami
```

The credential comes only from `KHIVE_CLOUD_API_KEY`. `--url` overrides `KHIVE_CLOUD_URL`;
`--allow-insecure` permits non-loopback HTTP. Options may precede or follow the
command. `whoami` sends `whoami()`; `exec` sends its argument unchanged. Both print
the REST result envelope as JSON. Credential strings are redacted from result
values and reflected server text. Dictionary keys are preserved verbatim. If any
field name contains a configured credential, the CLI withholds the entire result
and exits with code 6. Errors print one line to stderr with the exception class
and message.

| Exit code | Meaning                                                       |
| --------- | ------------------------------------------------------------- |
| 0         | Request completed; inspect per-operation outcomes in the JSON |
| 2         | Usage or configuration error                                  |
| 3         | `AuthError`                                                   |
| 4         | `RateLimited`                                                 |
| 5         | Other `HttpError` or `TransportError`                         |
| 6         | Result withheld because a field name contains a credential    |
