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
- The transport is swappable (`Transport` ABC); Unix socket today, HTTP
  planned for remote use. Client code does not change.
- A `batch()` is one request, not a transaction: ops succeed or fail
  individually and the per-op results say which.

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
