# Extension modules: ABI, host interface and extension points

Status: **specification, M24.01** (PR-23, decision D18, conflict 11). No loader, runtime or engine wiring exists yet: those are M24.02, M24.03 and M24.04. Everything here is a contract, not a measurement; where a number is a starting default it says **proposed**. The golden fixtures are under `fixtures/extensions/abi-v1/` and are checked by `tools/tests/test_extension_abi.py`.

This page fixes ABI version 1.0 and engine API version 1.0. A module is **native** (a shared library, in-process, trusted) or **WASM** (a core WebAssembly module, in-process, sandboxed). A third model, a separate native process over RPC, is reserved at the end and not built by M24.

## 1. What each model can and cannot isolate

"Cannot" means the host has no mechanism to prevent it, not that the module is expected to do it. This table is the statement M24.01's definition of done asks for.

| Concern | Native in-process (C ABI) | WASM in-process | Separate process (reserved) |
| --- | --- | --- | --- |
| Reading or writing host memory | **Not isolated.** Same address space; the module can read or corrupt anything, including other tenants' cached data | **Isolated.** One linear memory per instance, bounds-checked by the runtime; the host reads and writes it only through the two I/O regions | Isolated by the OS |
| Files, sockets, environment, clock | **Not isolated.** Ordinary libc is available. The host grants none and the header declares none, but cannot stop a call | **Isolated.** The only imports are `log` and `cancelled`; any other import refuses the module at link time. No WASI | Isolated only if the OS profile (seccomp, namespaces) is applied |
| Memory use | Not bounded | Bounded: the linear memory has a declared maximum, and the instance is refused over it | cgroup limit |
| CPU time | Not bounded. `timeout_ms` is advisory and `cancelled()` is cooperative; a spinning module holds its thread | Bounded by fuel (instruction budget) and a wall-clock deadline; the instance is stopped | Killable |
| Crash, abort, UB | **Takes the node down** (D18) | A trap stops that instance only | Takes the module process down only |
| Panic | Must not unwind across the C boundary (undefined behaviour). A Rust module must catch it and return `HS_E_MODULE`, or build with `panic = "abort"` and accept that it aborts the node | A Rust panic is a trap | Same as WASM |
| Wrong output | The host bounds-checks and validates every output byte before use; a malformed result is refused | Same | Same |
| Supply chain | The node pins the artifact's SHA-256 in configuration and verifies it before loading. No signature scheme in v1 | Same | Same |
| Determinism | Required of the module (section 3); not enforceable, only testable | Enforceable for free: no clock, randomness or I/O import exists | Required, not enforceable |

What the host enforces **for every model**, because it is on the host side of the boundary: version refusal, declared extension points only, output buffer size, output validation, per-call input size, concurrency per module, and per-role enablement. What an extension point lets a module reach is limited by construction: a module receives bytes and returns bytes. It is never handed an index, a segment, a blob handle, a socket or a pointer into the engine, so the second definition-of-done item ("no extension point lets a module read files, sockets or memory outside what the host grants") holds for every point by interface, and **for WASM also by enforcement**. For a native module it holds by interface only; a hostile native module can bypass the interface, which is why native modules are trusted and enabled only by explicit configuration.

## 2. Descriptor

`hs_module_describe` writes a UTF-8 JSON object of at most `HS_MAX_DESCRIPTOR_BYTES` (16 KiB). It is pure and identical on every call. Unknown keys are refused. Example: `fixtures/extensions/abi-v1/descriptor.example.json`.

| Key | Rule |
| --- | --- |
| `name` | `^[a-z][a-z0-9_-]{0,63}$`, unique among loaded modules |
| `version` | `major.minor.patch`, the module's own |
| `abi` | `major.minor` of this ABI the module was built against |
| `engine_api` | `major.minor` of the engine API the module requires (point semantics and wire encodings of section 6) |
| `max_concurrent_calls` | integer at least 1; the host never exceeds it for this module |
| `io_bytes` | size of each WASM I/O region; must be at least the largest input or output the module's points use. Ignored for native |
| `extension_points` | non-empty array of `{kind, id, version, deterministic}`; `kind` is `analyzer`, `query`, `ingest` or `scoring`; `id` matches `^[a-z][a-z0-9_]{0,63}$` and is unique per kind; `version` is `major.minor.patch`; `deterministic` must be `true` |

`deterministic: true` is a declaration that a call's output depends only on its point id and input bytes and the module's configuration. It is what lets recovery, copy and CCR reproduce terms and plans ([modules.md](modules.md): "schema binds analyzer version/data hashes"). Mapping binding is in section 7.

## 3. Native C ABI

The header is `fixtures/extensions/abi-v1/hsearch_module.h`; `fixtures/extensions/abi-v1/layout.json` is its golden layout (every struct has no implicit padding; LP64 targets only: x86_64 and aarch64 on Linux and macOS). A module exports exactly five symbols: `hs_abi_version`, `hs_module_init`, `hs_module_describe`, `hs_module_call`, `hs_module_shutdown`.

**Call sequence.** The host opens the library, resolves `hs_abi_version` **alone**, and calls it before resolving anything else. A refused module is closed without any other symbol having been resolved or called. Then `hs_module_init(host, config)`, `hs_module_describe`, and only after the descriptor is accepted any `hs_module_call`. `hs_module_shutdown` runs once after the last call has returned.

**Version rule (`abi_version` is `(major << 16) | minor`).** Accept only if the module's major equals the host's and the module's minor is at most the host's. A newer minor than the host is refused; an older one is accepted. Minors may only append fields to a struct that carries `struct_size`, or add constants. A major is a different ABI and is refused. The descriptor's `abi` must equal what `hs_abi_version` returned, and its `engine_api` is checked by the same rule against the host's engine API version; a mismatch in either is `ModuleVersionRefused` and nothing of the module is registered.

**Ownership.** The host owns every buffer and lends it for exactly one call. `hs_slice` is read-only input. `hs_out` is a host buffer the module fills: it writes at most `cap` bytes and sets `len`; if the result would not fit it returns `HS_E_BUFFER_TOO_SMALL` with `len` set to the size needed, and the host retries once if that size is within its limit for the point, otherwise refuses with a limit error. A module never returns memory for the host to free, never frees host memory, and never retains a pointer past the call. `hs_host_v1.log` copies the message before it returns.

**Status codes a module returns** (`int32_t`; zero succeeds, every failure is negative; identical in the WASM interface):

| Code | Value | Meaning |
| --- | --- | --- |
| `HS_OK` | 0 | Success |
| `HS_E_BUFFER_TOO_SMALL` | -1 | `out->len` holds the size required |
| `HS_E_INVALID_ARGUMENT` | -2 | Malformed input for the point |
| `HS_E_UNSUPPORTED` | -3 | Unknown kind, point or version |
| `HS_E_LIMIT` | -4 | The module refused an over-limit input |
| `HS_E_CANCELLED` | -5 | `cancelled()` returned nonzero and the module stopped |
| `HS_E_MODULE` | -6 | Module failure; optional UTF-8 detail written to `out` |
| `HS_E_NOT_INITIALIZED` | -7 | A call arrived before a successful `hs_module_init` |

**Concurrency.** `hs_module_call` may run on many threads at once, up to `max_concurrent_calls`; the module must be thread-safe. The host runs native calls on a bounded dedicated pool so a module that hangs costs pool slots, not the node's request threads. It cannot cancel such a call.

**Loading is the only place `unsafe` exists** (D18). The loader, M24.02, must: open the file once, hash that open descriptor and compare it with the pinned SHA-256, and load that same descriptor rather than re-resolving the path (no check-then-use gap); use immediate binding and local symbol scope; and keep all library open, symbol lookup and call code inside the one loader crate with its written safety proof.

## 4. WASM host interface

A module is a core WebAssembly (wasm32) module. There is no component model and no WASI, so the interface needs no particular runtime; the choice of runtime is M24.03. The golden is `fixtures/extensions/abi-v1/wasm-host-interface.json`.

- **Imports.** Module `hsearch_host_v1` provides `log(level, ptr, len)` and `cancelled() -> i32`. Nothing else is linked. A module with any other import is refused at link time, which is the whole "forbidden host call" mechanism: there is no such call to make.
- **Exports.** The same five entry points as the C ABI, plus `memory`, `hs_input_region`, `hs_output_region` and `hs_output_len`. The two regions are fixed buffers of `io_bytes` that **the guest owns**; the host copies the input in and the result out. This avoids a guest allocator across the boundary and bounds per-call memory by construction. A call writes the point id bytes then the input bytes at the input region, passes their lengths, and reads `hs_output_len` bytes from the output region. Config for `hs_module_init` goes through the input region the same way.
- **Return values** use the same status codes as the C ABI. Traps, fuel exhaustion, memory growth past the maximum and deadline expiry are host-side outcomes (section 5).
- **Limits** the host sets per module in configuration, never the module: maximum linear memory, fuel per call, wall-clock deadline per call, `io_bytes` ceiling, and the table and instance count. Defaults are **proposed** and measured in M24.03.

## 5. Failure semantics

| Outcome | Native | WASM |
| --- | --- | --- |
| Module returns a negative status | Typed `ModuleError(code)`; the call fails, the module stays loaded | Same |
| Output malformed (bad length, UTF-8, count over bound, out-of-range offsets, non-finite score, IR that fails schema or budget) | `ModuleOutputMalformed`; the call fails; the module stays loaded (a strike, below) | Same |
| Trap, fuel, memory or deadline | Not applicable: nothing stops it, a crash is a node crash | `ModuleTrapped` / `ModuleLimit`; the instance is discarded and the call fails. The node keeps serving |
| Repeated failure | Not applicable | **Proposed:** 3 trapped or limit-breaching calls within 60 s move the module to `failed`, reported in capabilities until restart or explicit re-enable. Confirmed by M24.03 against the measured instantiate cost |

Three rules hold in every row. **A failing module never falls back silently**: a request that needs the module's analyzer, query type, processor or scoring function fails with a typed, retriable-or-not error naming the module, and never runs a different analyzer or plan. **A failure does not change what an existing index reads**: an index whose mapping references an unavailable module refuses to open for the affected operations rather than reading with other terms. **Errors carry the module name, extension point, status code and at most `HS_MAX_ERROR_DETAIL_BYTES` of module-supplied detail**, which is logged but never echoed to a client unredacted.

## 6. Extension points and wire encodings

All structured data crossing the boundary uses one encoding, so a native and a WASM build of the same module produce byte-identical output. Integers are little-endian; `string` is `u32 length` then UTF-8 bytes; arrays are `u32 count` then items. A decoder checks every length against the remaining bytes and against the point's bound **before** allocating, refuses invalid UTF-8, and refuses trailing bytes. The shapes follow what the engine already has: `Token`, `Value`, `Kind` and the query IR in `crates/engine`.

| Kind | Input | Output | Host validation | Roles |
| --- | --- | --- | --- | --- |
| `analyzer` | the raw UTF-8 text, at most the document byte bound | `u32 count`, then per token `string term`, `i32 position`, `i32 start_offset`, `i32 end_offset` | count at most the term bound; terms non-empty and within the term bound; positions non-decreasing; offsets within the input's UTF-16 length with start at most end | `index`, `search`, `coordinator` where the mapping is read or a query is analyzed |
| `query` | `string body` (the module's clause as JSON, bounded), then `u32 count` of `{string field, u8 kind}` for the schema, kinds `1 keyword, 2 text, 3 integer, 4 boolean, 5 date` | a core query IR tree, preorder, tags `1 All, 2 Id, 3 Term, 4 Terms, 5 Range, 6 Match, 7 Bool`, values tagged `1 string, 2 integer, 3 boolean, 4 date` | field exists and has the right kind; the **same node, byte, depth and term budget as a built-in query**; a module cannot emit an operator the core does not advertise. The host lowers before fan-out, so search nodes never need the module | `coordinator` |
| `ingest` | `u32 n` documents; each is `u32 fields`, then `{string name, u8 kind, value}` | per document `u8 status` (0 keep, 1 drop, 2 reject), then the document for keep or a `string reason` for reject | the result is validated against the mapping exactly like an unprocessed document, including the document byte bound; a processor sees one batch, never another request's documents | `index`, or `coordinator` where bulk bodies are parsed |
| `scoring` | `u32 docs`, `u32 features`, then `docs * features` `f32` | `docs` `f32` | docs at most a **proposed** 1,024 and features at most a **proposed** 16 per call; every output finite | `search` |

One call handles a block or batch, never one value per call, so the boundary is crossed once per block as [modules.md](modules.md) requires. The point id and its `version` come from the mapping reference (section 7), never from the request.

## 7. Enablement, roles and mapping binding

- **Enablement is explicit node configuration, per module and per role.** Each entry names `name`, `kind` (`native` or `wasm`), `path`, `sha256`, `roles` (a subset of `index`, `search`, `coordinator`, and only roles the module's points may run in per the table above), `enabled`, the WASM limits, and an opaque `config` object handed to `hs_module_init`. The node's configuration format is M24.04's; the fields above are the contract. A module not listed, or `enabled: false`, **is not loaded and does not exist**: it is absent from `/v1/capabilities`, its points answer as unknown, and its file is never opened.
- **Native modules need a second, deliberate switch.** A native entry is refused unless the node is started with native modules allowed, so a configuration copied from a sandbox-only deployment cannot load one by accident.
- **Capabilities** list each loaded module's name, version, kind, ABI, engine API, roles, points and state (`ready`, `failed`). They list nothing the module was not accepted for.
- **A mapping binds a module point as `{module, kind, id, version, sha256}`.** The extension point is chosen by mapping, not by node state: enabling or upgrading a module never changes how an existing index reads. A node whose loaded module does not match the pinned name, point version and hash fails closed for that index (`ModuleUnavailable`) instead of reading with different terms. Replicas, recovery, copy and CCR followers apply the same rule, so a follower without the module refuses to apply analyzer-dependent operations rather than diverge. How the binding is stored and migrated is M24.04.
- **Disabling or removing a module that a live mapping references is refused** until the index is migrated, the same rule [modules.md](modules.md) states for format owners.

## 8. Cost, memory and the performance standard

This task adds no code on a hot path and **measured nothing**; the numbers that matter are named as validation for the tasks that create the code (M24.02 native call cost, M24.03 WASM call and instantiate cost and fuel overhead, M24.05 end to end against a built-in analyzer). The design choices made so they can be measured and bounded:

- **No object-store traffic.** Modules are node-local files read once at startup and hashed. A module has no blob, network or file handle, so it adds no PUT, GET, LIST or inter-AZ bytes. Distributing module files to nodes is outside the engine (PR-19, no control plane).
- **Bounded memory.** Every buffer has a ceiling set by the host before allocation: per-call input and output, descriptor, config, WASM linear memory and regions. The host's output buffers are per-worker and reused; nothing is sized by what a module reports.
- **Copies and allocations.** Native: the input is a borrowed slice, the output is written directly into the host's buffer, zero copies and zero allocation per call on the host side, and one retry only when the result outgrew the buffer. WASM: one copy in and one copy out per call, and no guest allocator on the call path. A call carries a block, so the per-call fixed cost (a thread hop for native, a fuel-metered entry for WASM) is amortized over the block.
- **Search visibility.** An ingest processor or analyzer is on the write path and adds its call time to bulk latency before refresh; a query or scoring module adds to query latency. The module's call time is reported per point so it can be held against the visibility targets; no budget is claimed here.
- **Downsides, stated.** A native module costs the node its fault isolation. A WASM module costs a copy in and out, fuel accounting, and an instantiate cost on restart after a trap. Batching raises the latency of the first item in a block. A determinism rule that forbids clocks and randomness excludes some analyzers (for example one that fetches a dictionary), which must ship its data in the module. Pinning hashes in mappings makes module upgrades an explicit migration instead of a hot swap, deliberately.

## 9. Review against the example module's needs

The M24.05 example serves an analyzer and a query type, as native and as WASM, from public contracts only. What it needs and where the spec supplies it: a unique name, version and points (descriptor); an analyzer producing engine tokens with offsets (section 6 analyzer, mirroring `Token`); a query type that becomes a plan the engine can run without engine internals (section 6 query, lowering to the core IR under the built-in budget); identical bytes from both forms (one wire encoding, deterministic, shared status codes); a version-mismatch refusal (section 3); a disabled-module test (section 7: absent, file never opened); WASM trap, memory, fuel and forbidden-import tests (sections 4 and 5); a native panic documented as taking the node down (section 1). One gap found and recorded: the wire encodings have no golden vectors yet. They arrive with the example, M24.05, whose reference outputs become the cross-form test; M24.01's golden is the header, layout and host interface.

## 10. Reserved: a separate-process model

An optional third model runs a native module in its own process and speaks the same five entry points and wire encodings as length-prefixed frames over a Unix socket pair, inheriting its isolation from the OS. It is **not specified further and no M24 task builds it**; it is kept out of the way so the descriptor, point ids and wire encodings need no change if it is added. It would need its own version rule, restart policy and a sandbox profile.

## 11. Not done by this task, and handed on

| Item | Owner |
| --- | --- |
| Loader crate, its safety proof, the lint exception and the AGENTS.md `unsafe` rule amendment | M24.02 |
| WASM runtime choice, exact pin, licence inventory, limit defaults, trap and quarantine measurements | M24.03 |
| Registration through `hsearch-engine`, node configuration format, capability reporting, mapping binding storage | M24.04 |
| Wire golden vectors, the external example, identical-results proof, documentation reuse | M24.05, M16.06 |
| Feature list supplied to scoring modules (which per-clause scores and doc values) | M24.04 |
