# M07.02 evidence: bounded authenticated peer RPC

`provisional`: loopback on a shared 4-CPU cloud VM, release build, server and clients in one process, so RSS figures include both ends. Numbers are relative. No object store is involved, so no MinIO, S3 or RustFS. Reproduce:

```
cargo test -p hsearch-peer
cargo run --release -p hsearch-peer --example peer_profile
```

## What is built (`crates/peer`, new crate `hsearch-peer`, depends on `hsearch-security` only)

- **Wire** (`wire.rs`): a 20-byte header (magic, version, kind, RPC, status, stream, deadline, length) validated field by field before any body is read. A request names its RPC and the length cap is that RPC's own (a heartbeat 256 B, a membership exchange 64 KiB, a shard search 1 MiB, a shard write 16 MiB), so a small RPC cannot claim a large body. A request must carry a deadline, at most 60 s. A frame that fails validation ends the connection.
- **Budget** (`budget.rs`): process-wide caps on connections, queued frames and queued body bytes, taken all-or-nothing and given back when a permit drops (a connection that dies mid-frame cannot leak any), with high-water marks and refusal counters for the report.
- **Receive state machine** (`inbound.rs`, no I/O): a body is admitted against the budget from its header alone; its buffer then grows only as bytes arrive (doubling, never past the declared length), so a peer that declares and does not send holds a permit, not memory. A body that does not fit is skipped without being stored and answered `Overloaded`; the connection survives. Time rules: header within `header_ms` of its first byte, body within a base plus the time a minimum byte rate would take, idle only when nothing is pending, all checked on a tick whether or not bytes arrive.
- **Transport** (`transport.rs`): blocking TCP + rustls through `PeerTls` (TLS 1.3, mandatory client certificates, no resumption, no plaintext or insecure mode exists to fall back to). One 256 KiB-stack thread per connection, capped by the budget; over the cap the socket is dropped before any TLS work. The server reads the caller's role from its certificate and checks it against `PeerRpc::allowed_callers` on every request; the client checks the server's role against the roles it expects before sending a byte. A connection is re-verified against the trust in force at every request and ends when its certificate stopped being trusted or it passed its maximum age. The frame's deadline reaches the handler as `Cx::is_done`.

**Design call: not HTTP/2.** The task said "typed H2/mTLS". This is a small framed protocol with streams, a mandatory deadline and cancel, over the same mTLS. HTTP/2's HPACK tables, CONTINUATION frames, priority trees, flow-control windows and rapid-reset are each an unbounded-memory or amplification surface that needs its own mitigation, and `h2` brings an async runtime for six message shapes. If a gateway ever needs HTTP/2 it sits in front of this, not inside it. Cost: no off-the-shelf tooling (curl, grpc clients, proxies) can speak it.

## Tests (28 in `crates/peer/tests`, run three times, stable)

`frames.rs` (12, no socket): every RPC and kind round-trips; every header field's refusal named (magic, version, kind, RPC, status, reserved, stream 0 and even, deadline missing and past the cap, wrong direction, cancel with body or deadline, response with deadline); length caps per RPC and for responses; the budget is all-or-nothing, gives back on drop and records peaks; a frame split at every byte decodes the same as whole; a declared 16 MiB body holds a permit and allocates nothing, and 1 KiB received means at most 2 KiB held; **an oversized prefix ends the connection with nothing ever reserved**; a body that does not fit is skipped and the next request is served; a full queue sheds; header trickle, stalled header and slow body are each too slow, and a skipped body is held to the same clock; idle only when nothing is pending; a client decodes responses and refuses requests.

`mtls.rs` (16, real sockets and real mutual TLS 1.3): the happy path over 20 calls, handler errors, local refusal of an over-cap body; **wrong role per RPC** (a search node may heartbeat and fetch segments, not write, search or drain; the whole `PeerRpc::ALL` table walked for all three roles against the policy, the channel surviving each refusal); a client refusing a server in an unexpected role; an oversized prefix closing the connection with `peak_bytes` 0; a header dribbled a byte at a time cut off; a declared 8 MiB body holding a permit and releasing it when the peer vanishes; **a stalled TLS handshake** cut off and its connection slot returned; plaintext frames and a TLS client with no certificate never getting an application answer; a certificate from another CA unable to connect; **certificate rotation** keeping an open channel and serving a new one, on both ends; a channel past its maximum age ending at the next request; the deadline reaching the handler and the caller giving up on time; a queued request cancelled before it ran; a body over the shared budget shed while the next request is served; the connection cap refusing and freeing slots; six concurrent callers inside the budget.

## Measurements (`examples/peer_profile.rs`)

**Round trips** (one channel, sequential, mutual TLS 1.3, loopback): heartbeat p50 221 us and p99 300 us; 4 KiB p50 213 us and p99 326 us; 1 MiB p50 2.3 ms and p99 3.3 ms (862 MiB/s in both directions together). A mutual TLS handshake p50 is 2.2 ms.

**Memory per connection** (both ends in one process, so an upper bound for a server): 100 connections 57 KiB each, 500 connections 47 KiB, 1,000 connections 36 KiB, one 256 KiB-stack thread each (1,002 threads at 1,000).

**Declared but unsent bodies:** 200 connections each declaring a 1 MiB write body and sending 1 KiB held 200 MiB of budget and 200 frames while the process grew 2.4 MiB (12 KiB a connection). The budget, not memory, is what a declared body costs.

**Budget** (what the caps held in the tests above): peak connections 2 under a cap of 2 with the third refused; peak bytes 0 for the oversized prefix; peak bytes within 64 KiB for a 1 MiB body shed under a 64 KiB cap; six concurrent callers peaked within 6 connections and 24 KB.

## Limits and downsides

- **A running handler is not interrupted by a `Cancel`.** The blocking connection thread reads only between requests, so a cancel is honoured for a request still queued on the connection, and a running request is stopped by its deadline (`Cx::is_done`, cooperative: a handler that does not check it runs on and its late answer is replaced by `DeadlineExceeded`). Interrupting a running handler needs a transport that reads while it works.
- **Requests on a connection are served in order, one at a time.** The queue (default 16) is bounded and sheds `Overloaded`; there is no per-connection parallelism. A caller wanting concurrency opens channels, each costing about 40 to 60 KiB and a thread.
- **One thread per connection.** Fine at the hundreds to low thousands of peers a node has (1,000 measured); not a design for tens of thousands.
- **A timed-out call closes the channel**, because its late answer would arrive in the middle of the next call. The caller reconnects (2.2 ms).
- **Not measured:** RSS and latency on a real network, ARM, TLS session cost under CPU contention, and the node's own listener, which does not use this yet (M07.03 and later wire it). The channel pool and reconnection policy belong to the callers.
- **Deadline units are the caller's relative milliseconds**, so clock skew between nodes does not matter, and network delay is not subtracted from the server's allowance (the server counts from when the header completed).
