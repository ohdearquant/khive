# Wire-protocol benchmarks

The codec target measures the public length-prefixed frame protocol with generated
JSON response frames. This crate is the pure ADR-137 frame codec and server-side
handshake state machine: it performs no network, MCP-stdio initialization,
request dispatch, database or model work.

## Codec workloads

```sh
cd crates
cargo bench -p khive-wire-protocol --bench wire_codec -- --quick --noplot
cargo bench -p khive-wire-protocol --bench wire_codec -- --test
cargo bench -p khive-wire-protocol --bench wire_codec -- --list
```

No crate feature is required. Criterion is a native-only development dependency;
the benchmark's WASM entry point is empty.

Each group runs at exactly 64, 4,096 and 1,048,576 serialized JSON payload bytes.
These sizes exclude the four-byte big-endian length prefix: wire lengths are
68, 4,100 and 1,048,580 bytes. The generator derives payload padding from the
actual public encoder's empty-response overhead and fills it with a fixed-seed
ASCII sequence. It verifies the complete wire length, declared prefix and
semantic frame equality. This avoids silently measuring an opaque 1 MiB value
plus an unreported JSON envelope. All sizes fit the default 8 MiB payload cap.

| Scene under `wire_codec/`  | Timed public operation                                | Output witness                                                            |
| -------------------------- | ----------------------------------------------------- | ------------------------------------------------------------------------- |
| `encode/{N}`               | `FrameCodec::encode`                                  | Exact generated wire bytes                                                |
| `decode/{N}`               | `FrameCodec::decode`                                  | Complete semantic frame equality                                          |
| `decode_with_consumed/{N}` | `FrameCodec::decode_with_consumed`                    | Exact first-frame equality and consumed length, untouched following frame |
| `encode_frame/{N}`         | Free `encode_frame`                                   | Exact generated wire bytes                                                |
| `decode_frame/{N}`         | Free `decode_frame` with the configured payload cap   | Complete semantic frame equality                                          |
| `round_trip/{N}`           | `FrameCodec::encode` followed by `FrameCodec::decode` | Both exact wire bytes and complete decoded frame                          |

The consumed-length scene receives a real concatenation: the measured response
followed by a small valid Cancel frame. After timing it verifies the exact
boundary and decodes the remaining bytes as that expected second frame. It never
substitutes the private payload parser for a public framed decode.

Fixtures are created lazily for selected scenes. Generation, pre-encoding and
input checks are outside timing. Every timed operation produces owned output;
validation and output destruction happen after its timer stops. A round trip
retains both encoded bytes and the decoded frame until then. Iterations complete
sequentially; only one selected size's fixture and one output are retained at a
time. Listing scenes creates no fixture.

Each selected scene first reports one actual call, labelled `single_call`, with
payload bytes, wire bytes, nanoseconds per operation and decimal MB/s. The
throughput numerator counts one JSON payload per operation, including round trip;
it does not count encode and decode as two payload transfers. Decimal MB/s is
`payload_bytes / seconds / 1,000,000`. A zero elapsed clock observation reports no
throughput value rather than inventing a duration.

Criterion independently measures the same operation with `Throughput::Bytes`.
Its displayed byte-rate unit may be MiB/s; the fixed JSON field explicitly uses
decimal MB/s. Neither is a transport benchmark. A round-trip estimate is the cost
of encoding and decoding one frame, not either half separately. Per-call `Instant`
overhead is included, especially relevant at 64 bytes. Fixed observations are not
medians or tail percentiles; Criterion estimates come from its own samples.

The target requests ten flat samples, 100 ms warmup and one second of measurement
before command-line overrides. These sampling settings are not a runtime bound.
All current baseline values await qualified-host measurement with exact source
revision, toolchain/profile/features, CPU/OS, command and retained raw output.

## Handshake workloads

```sh
cargo bench -p khive-wire-protocol --bench wire_handshake -- --quick --noplot
```

`wire_handshake/gate_exchange` measures a new connection's current-version
`HandshakeGate::admit(Handshake)` and then `admit(Request)`. The fixture constructs
the request and fresh default gate before timing. The output retains the gate and
both outcomes; after timing it checks the exact accepted version and ACK, complete
gate state, and first-request admission. Each iteration starts with an incomplete
gate. There is no reused completed gate and no request dispatch.

`wire_handshake/encoded_exchange` additionally encodes the client's Handshake,
decodes it at the server, encodes the returned ACK, and decodes that ACK at the
client, then admits the same prebuilt first Request. Both wire buffers and decoded
frames remain live until timing stops and are checked against the exact fixture.
The server-to-client ACK never enters the server's inbound gate. The Request is an
in-memory admission witness, not an encoded request exchange. This is a pure
in-memory tailnet protocol exchange, without sockets, authentication, MCP stdio
initialization or a client state-machine implementation.

Each selected scene reports one `single_call` observation in nanoseconds per
connection, followed by Criterion's independent trend. Fixtures, gate construction,
validation and destruction are outside timing. Each timer includes `Instant`
overhead. Sampling settings and lazy scene selection match the codec target.

## Allocation workloads

```sh
cargo bench -p khive-wire-protocol --bench wire_allocations -- --quick
cargo bench -p khive-wire-protocol --bench wire_allocations -- --list
cargo test -p khive-wire-protocol --test benchmark_allocation_observer -- --test-threads=1
```

`wire_allocations` is a separate instrumented executable, with one fixed allocation
report for each of the 18 codec scenes and two handshake scenes. Every size runs
in default, test and quick modes unless explicitly filtered. It accepts `--bench`,
`--test`, `--quick`, `--noplot`, `--list` and one optional literal substring filter.
Listing or filtering out a scene performs no fixture construction or observer
integrity work for that scene. The two timing executables do not install the
allocation observer.

Each report covers one actual operation. The successful Rust global allocator
request count includes allocation, zeroed allocation and successful reallocation
requests. The peak is live Rust heap above the baseline immediately before that
operation, with outputs retained through observation finish. Setup, validation,
logging and output destruction are outside the window. A round trip retains both
its wire buffer and decoded frame. The gate-only report excludes construction of
the gate and request, just as its timing scene does.

These are Rust global allocator metrics, not serialized bytes, total bytes ever
allocated, allocator-internal temporary copies, C allocations, mmap memory or RSS.
The observer tracks all threads but these protocol operations are synchronous.
Its ordinary integrity test verifies a worker's allocation and growth are observed;
the same integrity check runs once before the first selected allocation scene.
No zero-allocation promise or timing threshold is imposed on protocol operations.

## Baselines and execution

```sh
cd crates
cargo bench -p khive-wire-protocol --benches -- --quick --noplot
```

| Group                                 | Baseline            | Reference producer                                 |
| ------------------------------------- | ------------------- | -------------------------------------------------- |
| `wire_codec` (18 scenes)              | Pending measurement | Qualified-host `gate-channel-code` bench-track run |
| `wire_handshake` (2 scenes)           | Pending measurement | Qualified-host `gate-channel-code` bench-track run |
| `wire_allocations` (20 fixed reports) | Pending measurement | Qualified-host `gate-channel-code` bench-track run |

There are no previous published numbers for this crate. Completion requires two
measured reference runs at the same source commit, retained under
`crates/khive-wire-protocol/perf/`. Record both raw outputs, commit, Rust toolchain,
profile/features, CPU/OS and exact commands. The two reference runs must keep the
allocation and latency outputs distinct. Pending rows are not measured baselines;
the suite is not complete until those reference artifacts exist. Timing values are
advisory comparisons with fixed and rolling references, never fixed-percentage gates.

## Performance guarantees

The issue's `bytes/frame (alloc/op)` wording combines different quantities. The
following table separates exact serialized sizes from observed Rust allocations.
Input MB/KB in the issue means 1,048,576/4,096 JSON payload bytes here, excluding the
four-byte prefix. `ns/frame` denotes one complete round-trip operation where named.

| Operation                                  | Input                                                | Metric                                                                                                                           | Bound                                        |
| ------------------------------------------ | ---------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------- |
| `FrameCodec::encode` + `decode` round-trip | 1,048,576-byte JSON payload                          | ns/frame round trip                                                                                                              | Advisory trend against the recorded baseline |
| `FrameCodec::encode` + `decode` round-trip | 1,048,576-byte JSON payload                          | Decimal payload MB/s, one payload per round trip                                                                                 | Advisory trend against the recorded baseline |
| `FrameCodec::encode` + `decode` round-trip | 64-byte JSON payload, 68 wire bytes                  | Successful Rust allocator requests/op and peak live Rust heap above pre-call baseline bytes                                      | Advisory trend against the recorded baseline |
| `FrameCodec::decode_with_consumed`         | 4,096-byte JSON payload plus following frame         | ns/frame decoded                                                                                                                 | Advisory trend against the recorded baseline |
| `HandshakeGate::admit`                     | One fresh handshake/ACK plus first-request admission | ns/connection; allocation companion reports successful Rust allocator requests/connection and peak live Rust heap above baseline | Advisory trend against the recorded baseline |

The encoded-exchange companion reports serialization plus the gate path separately
from gate-only work. Fixed reports provide single-call observations, not median or
p99 estimates. Payload/ACK/consumed-length checks establish truthful fixtures;
observer integrity is the only added ordinary test. Performance rows remain advisory.
