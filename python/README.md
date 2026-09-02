# khive-py

Python client for the khive knowledge-graph database.

khive here is a database, used the way you use Postgres: the daemon
(`kkernel mcp --daemon`) is the database process and single writer; this
package is the client library. It speaks the daemon's native Unix-socket
wire (length-prefixed JSON frames, version-checked) and never opens the
database file itself.

```python
from khive import Khive

db = Khive()  # connects to ~/.khive/khived.sock

a = db.entities.create(kind="concept", name="LoRA")
b = db.entities.create(kind="concept", name="PEFT")
db.graph.link(a.id, b.id, "extends", weight=0.9)

db.graph.neighbors(a.id)
db.query("MATCH (c:concept)-[:extends]->(t) RETURN c, t")
db.search("low rank adaptation")
db.diagnostics()          # writer/WAL/checkpoint counters
```

- Interface types are pydantic models (`Entity`, `Note`, `Edge`, `Page`).
  Every record shares one core — `id · kind · namespace · properties ·
  metadata · tags · created_at/updated_at/deleted_at` — and `kind` is the
  universal discriminator: an edge's kind IS its relation. This is the
  target contract; where the daemon still speaks older field names
  (`relation`, top-level `weight`) the client translates at the wire
  boundary until the server follows.
- The transport is swappable (`Transport` ABC): a Unix socket to a local
  daemon, or `HttpTransport` to a khive-cloud deployment (below). Client
  code does not change.
- A `batch()` is one request, not a transaction: ops succeed or fail
  individually and the per-op results say which.

## khive-cloud

The same `Khive` facade works against a remote khive-cloud deployment over
`HttpTransport` — install the `cloud` extra first (`pip install
'khive-py[cloud]'`, or `uv pip install -e '.[cloud]'` from a checkout):

```python
import khive

url = "https://khive-cloud.example"
db = khive.cloud(url, api_key)
# equivalent: khive.Khive(transport=khive.HttpTransport(url, api_key))

db.stats()
db.search("low rank adaptation", kind="entity")
```

`HttpTransport` speaks `POST /v1/request` (the same op-dispatch envelope the
socket daemon returns) and `GET /health`; a non-2xx response raises
`AuthError` (401/403), `RateLimited` (429), `BadRequest` (4xx), or
`ServerError` (5xx) — all subclasses of `HttpError`. An `AsyncHttpTransport`
twin is available for asyncio callers (used directly; `Session` itself stays
sync).

MCP access — the same deployment's `request` tool over streamable HTTP — is
in `khive.mcp`:

```python
from khive.mcp import mcp_list_tools, mcp_request, mcp_session

mcp_list_tools(url, api_key)  # -> ["request"]
mcp_request(url, api_key, "stats()")  # sync convenience, asyncio.run under the hood

async with mcp_session(url, api_key) as session:
    await session.call_tool("request", {"ops": "stats()"})
```

### `khive-cloud` CLI

The `cloud` extra also installs a `khive-cloud` script:

```bash
export KHIVE_CLOUD_URL=https://khive-cloud.example
export KHIVE_CLOUD_API_KEY=sk_...

khive-cloud whoami
khive-cloud exec 'stats()'
khive-cloud tools
khive-cloud health   # no auth required
```

Exit codes: `0` ok, `1` a server or op error (including `exec` against ops
whose envelope reports `summary.failed`/`aborted` > 0), `2` usage (missing
`--url`/`--api-key` or their env vars).

## Scratch database

Experiments should run against a scratch daemon, not a production store:

```bash
KHIVE_SOCKET=/tmp/khive-scratch.sock KHIVE_PID=/tmp/khive-scratch.pid \
  kkernel mcp --daemon --db /tmp/khive-scratch.db
```

```python
db = Khive(socket_path="/tmp/khive-scratch.sock")
```

## Tests

`pytest` boots its own scratch daemon (needs `kkernel` on PATH or
`KKERNEL=/path/to/kkernel`):

```bash
uv pip install -e '.[dev]'
pytest tests/
```

The cloud/MCP/CLI tests run against offline fake servers and need the
`cloud` extra (`uv pip install -e '.[dev,cloud]'`); without it they skip
cleanly rather than failing. `tests/test_cloud_live.py` is the exception —
it talks to a real khive-cloud deployment and is skipped unless
`KHIVE_CLOUD_URL` and `KHIVE_CLOUD_API_KEY` are both set.
