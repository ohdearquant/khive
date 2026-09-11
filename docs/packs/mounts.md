# Mounted tool sources

Configure stdio MCP servers in the same khive TOML file used by `kkernel mcp` and
`kkernel exec`. Each tool becomes `<mount>.<tool>` on the ordinary request surface.
The mount name accepts lowercase ASCII letters, digits and hyphens. Tool names accept
ASCII letters, digits, underscores and hyphens. Native namespaces cannot be shadowed.

```toml
[[mounts]]
name = "demo"
transport = "stdio"
command = "/absolute/path/to/server"
args = []
env = ["PATH"]
credential = "DEMO_API_KEY"
timeout_ms = 30000
tools = [{ name = "echo", effect = "read" }]
```

`env` and `credential` contain environment-variable names, never values. The child
receives only the named variables. An inline credential such as `sk-...` is rejected.
Child stderr is discarded; foreign error bodies are not included in diagnostics.
HTTP transports are unsupported. Omitting a tool's `effect` declares it `mutating`.

At boot khive initializes each server and discovers its tools. Every configured tool
must be published; a failed mount stays unavailable while other mounts can start.
Published tools absent from configuration are not pinned. Pins are stored in the
`tool_source_mounts` table, with a generation and a BLAKE3 digest for each definition.
A runtime restart preserves existing pins. Update configuration and explicitly re-pin
to replace the permitted set or effect classifications:

```sh
kkernel mount repin demo --config /absolute/path/to/khive.toml
kkernel exec 'demo.echo(message="hello")'
```

Re-pin discovers the selected source only, validates its catalog, then atomically
updates the pin record and appends one audit naming the operator and added, removed,
and changed identifiers. Already running instances read the updated pins on subsequent
calls. A call holding the previous generation is denied before invoking the source.
Changes to command, arguments or environment names require restarting serving runtimes.

Every call refreshes the published catalog and checks its pinned definition and current
generation before forwarding. A changed or missing definition returns `tool_error`
with `reason = "catalog_drift"`. Foreign errors, deadline expiry and malformed results
return `tool_error`, `tool_timeout` and `tool_malformed` respectively, in the normal
KhiveError operation envelope. JSON frames are bounded to 4 MiB; discovery is bounded
to 1,024 tools and 32 pages. Schemas must use local fragment references only.

Each subprocess starts at boot, restarts once after a failure, and is terminated when
its runtime registry is dropped. A second failure returns `tool_error` with
`reason = "mount_down"` until the runtime restarts. Calls are never automatically
retried. Enrollment and audit apply exactly as for native verbs. Pinned effect class
and generation appear under `mounted_tool` on the ordinary audit row.

Select `agent` explicitly alongside the native packs you need, for example
`kkernel mcp --pack kg --pack agent`. It is absent from the default pack set.
`verbs(pack="agent")` lists its five operations. `agent.spawn(provider="x", task="t")`
returns `provider_unavailable` until an adapter exists. `agent.observe`, `suspend`,
`resume` and `kill` operate on stored records; selecting the pack does not start agents.

Planning and MCP initialization read an owned in-memory catalog snapshot without
storage, gate, audit, or process work. Boot initializes the snapshot; every ordinary
catalog read, call, and local re-pin refreshes it. External re-pins become visible to
advisory plans on the next ordinary refresh. Real calls still validate the current
persisted generation immediately before calling the source.

Names containing a hyphen or starting with a digit use canonical JSON requests,
since function-call identifiers use letters, digits and underscores and cannot start
with a digit. For example, a configured `9-demo.9-echo` is called as:

```sh
kkernel exec '[{"tool":"9-demo.9-echo","args":{}}]'
```
