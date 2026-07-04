# Demos

Two runnable transcripts. Every command below was executed against a real `kkernel` 0.3.0
binary on a scratch database — copy-paste the commands to reproduce them, or read the captured
output directly.

| Demo                                       | What it shows                                                                                                           |
| ------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------- |
| [`research-ingest.md`](research-ingest.md) | Create typed entities, link them, search, and traverse the graph — the "structure vs. similarity" story from the README |
| [`gtd-memory.md`](gtd-memory.md)           | Task lifecycle (GTD pack) and salience-weighted memory recall                                                           |

## Running these yourself

Never point a demo at your production database. Use a throwaway path:

```bash
export KHIVE_DB=/tmp/khive-demo.db
kkernel exec 'stats()'          # confirms a fresh, empty database
```

`kkernel exec '<ops>'` runs the same DSL the MCP `request` tool accepts — everything in these
transcripts also works verbatim inside `request(ops="...")` from an MCP client.
