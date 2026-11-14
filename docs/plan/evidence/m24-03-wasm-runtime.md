# M24.03 evidence: WASM runtime and host crate

Code and measurements, on Linux x86_64 only. Numbers were taken on a shared 4-vCPU cloud VM and are **provisional**: relative, not absolute. Nothing was run on aarch64 or macOS.

## Delivered

- `crates/module-wasm` (`hsearch-module-wasm`, safe Rust, no `unsafe`): loads a core WebAssembly module under the host interface of spec section 4 and calls its extension points with enforced limits. It shares `hsearch-module-abi` with the native loader. The `Request`, `LogRecord` and `LogSink` types moved into that crate so both hosts take the same call; `module-native` re-exports them.
- **Runtime: wasmtime 50.0.1**, exact pin, Cranelift only (`default-features = false`, features `cranelift`, `runtime`, `std`): no WASI, no component model, no cache, no async, no threads, no GC. SIMD, relaxed SIMD, multi-memory and memory64 are switched off in the engine config, so a module using them is refused as invalid.
- **No ambient authority.** The linker holds exactly `hsearch_host_v1.log` and `hsearch_host_v1.cancelled`. Any other import (a WASI call, a function of another name or with another type) is refused at load as `ForbiddenImport`, so there is no forbidden call to make. A module cannot reach files, sockets or memory outside its own linear memory.
- **Limits, all host-side:** a required, declared memory maximum no larger than the limit (default 64 MiB; a module with no maximum or a larger one is refused at load, and a `memory.grow` past it traps as a limit), with the virtual reservation per memory set to that limit and a 64 KiB guard (wasmtime's default is 4 GiB plus guard per instance); fuel per call (**proposed** default 50 M); a wall-clock deadline per call, enforced by epoch interruption (a ticker thread, 1 ms resolution, parked while no call runs); host calls per call (default 256, each `log` counts; `cancelled` is a lock-free clock read and is free, since fuel and the deadline already bound a polling loop); a load timeout (default 5 s) over instantiation, the start function, version, describe and init; module file size (16 MiB); instances per module (64); wasm stack 512 KiB; log lines per second (50).
- **Outcomes:** a trap, fuel exhaustion, a memory or host-call breach ends that one call with a typed error (`ModuleTrapped` or `ModuleLimit`), discards that instance, counts a strike, and leaves the node serving: three strikes in 60 s mark the module `failed` (`ModuleUnavailable` afterwards). A deadline expiry also discards the instance but counts no strike, because the caller chose the timeout and must not be able to fail a module for everyone. A status the module returns is a typed error and not a strike. A discarded instance is replaced on demand, and **every** new instance (a replacement or the second to the Nth) runs the same version check, region check and `hs_module_init` with the module config as the first; `hs_module_shutdown` runs once, on an idle instance, when the module is dropped. A call's instance slot is held by a guard, so a panic in a log sink releases it.
- **Instances, not threads.** `max_concurrent_calls` is that many instances, created lazily. A call runs on the caller's thread with no hop; a caller that finds no free instance waits until its deadline (`ModuleBusy`), and a queue of more than twice the instance count is refused at once.
- Workspace: `tools/workspace.py` lists the crate (layer over `hsearch-module-abi`, external `wasmtime` and `sha2`), `docs/plan/dependencies.json` is regenerated, so the licence inventory covers all of wasmtime's transitive crates (it passes), and `scripts/coverage.sh` includes the crate.

## Runtime selection (measured)

The same lowercase analyzer module, compiled from `examples/lower.wat` (about 31 units of fuel per byte), through wasmi 2.0.0 and wasmtime 50.0.1, both with fuel metering on; wasmtime also with epoch interruption, as it is run:

| | wasmi | wasmtime (fuel + epoch) |
| --- | --- | --- |
| compile | 0.25 to 0.34 ms | 3.4 to 5.0 ms |
| instantiate | 5 to 6 µs | 6.4 µs |
| call, 64 B | 884 ns | 133 ns |
| call, 8 KiB | 102 µs | 13.0 µs |
| call, 64 KiB | 812 µs | 104.7 µs |
| ns per byte | about 12.4 | about 1.6 |

wasmtime is about 7 to 8 times faster on the byte loop and meters the same fuel per byte (about 31). wasmi has a far smaller dependency tree and compiles in a tenth of the time. wasmtime was chosen because an analyzer or scoring function runs per value or per block on the query path and the 8x is paid on every call; the cost it carries is the build and the binary (below), which are paid once. A module that is rarely called and trusted for its latency would favour wasmi; that is the revisit condition.

### Cost of the limits themselves (raw wasmtime, same loop)

| call | no metering | fuel | fuel + epoch |
| --- | --- | --- | --- |
| 64 B | 98 ns | 146 ns | 209 ns |
| 8 KiB | 9.4 µs | 12.3 µs | 14.5 µs |
| 64 KiB | 64.2 µs | 91.1 µs | 104.5 µs |

Fuel adds about 30 to 45% and epoch checks about another 15 to 30% on a loop with nothing in it but arithmetic and loads; a module doing real work per byte dilutes that. This is the price of both the CPU bound and the deadline, and neither is optional for an untrusted module.

## Call cost through the real host (`cargo run --release -p hsearch-module-wasm --example bench`)

Echo-style lowercase point, one caller, p50 / p99 in ns:

| payload | p50 | p99 |
| --- | --- | --- |
| 64 B | 555 | 808 |
| 4 KiB | 8,983 | 22,391 |
| 64 KiB | 141,836 | 281,091 |

A 64 B call costs 555 ns against 28,580 ns for the pooled native call (M24.02), because the WASM call has no thread hop. A raw wasmtime call of the same module with fuel and epoch on is 133 ns, so about 420 ns is this host's own work: the pool lock, the copies in and out, fuel and epoch setup and validation of the result. Two copies per call (input into guest memory, output out of it) are intrinsic to the guest-owned-region design of spec section 4 and bounded by `io_bytes`.

**Bounded memory reservation costs speed on memory-heavy loops.** With wasmtime's default reservation (4 GiB plus guard per memory, bounds checked by guard pages) the same runs gave 625 ns, 6.9 µs and 102 µs p50 for 64 B, 4 KiB and 64 KiB: the bounded reservation (explicit bounds checks) is the same at 64 B and about 30 to 40% slower at 4 KiB and 64 KiB. It was chosen because 64 instances of one module would otherwise reserve about 384 GiB of address space, which fails under `ulimit -v`, strict overcommit or `vm.max_map_count` once several modules or tenants load. A node that loads few modules and wants the speed can revisit it.

**Load:** 3.5 ms for the first module (engine, compile, validate, first instance, describe, init), 2.7 ms mean over 20 loads of the same file, each building its own engine. **Memory:** about 42 KiB resident per loaded module with one instance on this test module, which reserves 3 pages of which a few are touched; the bound that matters is the declared maximum times the instance count (64 MiB x 64 by default), counted against the node's extension budget, which M24.04 enforces.

**Build and size:** a clean release build of the crate with its 112 dependency crates takes 214 s on 4 vCPUs (mostly wasmtime and Cranelift). The benchmark binary is 18.3 MB, 13.3 MB stripped. If a node build does not need WASM it can leave the crate out; the crate is not yet a dependency of anything (M24.04 wires it).

## Validation (the task's list)

`crates/module-wasm/tests/load.rs`, 36 tests over hand-written WAT modules that each pick a behaviour from the first input byte:

- **traps:** `unreachable`, out-of-bounds load, divide by zero and unbounded recursion (stack overflow) each end the call as `ModuleTrapped`, and the next call on the same module works;
- **fuel:** a module with an endless loop is stopped by fuel (`ModuleLimit`, "fuel");
- **deadline:** the same loop with effectively unlimited fuel is stopped by the wall-clock deadline within 500 ms of a 60 ms timeout;
- **memory:** a module with no declared maximum, or one over the limit, is refused at load; `memory.grow` within the maximum works and past it is a `ModuleLimit` and a fresh instance serves the next call;
- **forbidden host call:** a module importing a WASI function is refused at load, naming the import; the host-call budget stops a logging loop (`ModuleLimit`);
- **after a trap the node keeps serving:** a discarded instance is replaced and starts from zeroed memory (the instance-reuse test), 8 threads x 200 calls get their own bytes back, three strikes mark the module failed and later calls are `ModuleUnavailable`, strikes age out of the window;
- **load-time refusals:** wrong SHA-256, bad or oversized file, wrong ABI (major 2, newer minor, an older module minor accepted), descriptor disagreements (engine API, abi, regions past the memory, regions overlapping, I/O over the host limit, malformed JSON, describe failure), failed init, config over 64 KiB, missing and mistyped exports, an unexported memory;
- **calls:** a module status becomes a typed error with its text kept out of `Display`; an undefined status and an oversized output length are `ModuleOutputMalformed`; undeclared point, oversized input, invalid bounds and a timeout too large to represent are refused without running; a result over the point's bound is a limit and keeps the instance;
- **every instance is initialised:** a module that answers `NOT_INITIALIZED` until `hs_module_init` ran serves a replacement after a trap and a second concurrent instance; shutdown runs once at drop; a start function that never ends is stopped by the load timeout; five calls with a 5 ms timeout do not fail the module; a panicking log sink does not leak instance slots; `cancelled` is 0 before the deadline and a guest polling it sees 1 after;
- **queueing:** a call that cannot get an instance before its deadline is `ModuleBusy`; a full queue is `ModuleBusy` at once (under 300 ms with a 5 s timeout); log lines are rate limited.

Writing the log test found a real bug: the first instance logged under an empty module name because its rate gate was built before the descriptor was read. The gate is now one object shared by every instance and named once the descriptor is known.

Two independent reviews (general and security, reading the code) found, and this revision fixes: `hs_module_init` ran only on the first instance, so replacements and second instances were uninitialised (high); no wall-clock bound on instantiation, the start function and load-time calls (the ticker did not exist yet); a caller's short timeout counted as a strike and could fail the module for all callers; `hs_module_shutdown` never ran; instance slots leaked on a panic; the default 4 GiB-per-instance memory reservation; control characters in module log lines and error detail, and an unclamped log level. Weak tests were strengthened (`cancelled`, the instance count). Not changed: `call_id` is 0 in WASM log records (the call context is not yet passed to the host import; M24.04), and the deadline remains tick-quantised.

### Tests were seen to fail

Thirteen deliberate mutations, each reverted, each caught: strike limit never reached; per-call fuel unbounded; every import granted; host-call budget not enforced; a trapped instance kept in the pool; the ABI version never refused; the log rate limit removed; the queue bound removed; the declared memory maximum unchecked; later instances not initialised; a caller deadline counted as a strike; instantiation not bounded by the deadline (the test hangs, which is its failure); the slot guard disabled.

### Coverage

`cargo llvm-cov test -p hsearch-module-wasm` (line floor 90): 95.93% lines, 94.26% regions (host.rs 94.79%, load.rs 96.08%, module.rs 96.08%). The uncovered lines are defensive: the ticker thread failing to spawn, an I/O region found outside the memory at call time (it cannot shrink and was checked at load), and fuel that cannot be set.

## Trade-offs, stated

- wasmtime is a large dependency (112 crates, 214 s clean build, 13 MB of binary) bought for an 8x faster guest than the interpreter and for epoch deadlines. Its JIT and runtime contain `unsafe`, in a crate we pin exactly and do not audit; our own crate is safe code. A wasmtime vulnerability is a sandbox escape, so the pin moves with its security advisories.
- Fuel and epoch checks cost 30 to 45% and 15 to 30% on a tight loop; both are required to bound an untrusted module.
- A clean call reuses its instance, so a hostile module can carry data between callers and tenants through its own memory. That is the module's data, trusted like a native module's; isolation between tenants is a matter of which module a tenant may call, not of the instance.
- A grow past the declared maximum traps the instance (a limit and a strike) rather than returning -1 to the guest, because the maximum was declared and checked at load: a module that grows past it is not honest about its memory.
- Instances are discarded after any trap or limit breach, never reused, since linear memory and globals of a trapped instance cannot be trusted. Clean calls reuse the instance and its memory, so a module's state can persist across calls; modules must treat the host as stateless between calls.
- The deadline has 1 ms resolution and is counted in ticks of a sleeping thread, so it can fire up to a tick early or later by any scheduler delay; a runaway is also stopped only at the next loop back-edge or function entry.
- Memory counted per instance is the declared maximum; that bound is held, not the resident size.

## Not run, and why

- aarch64 and macOS: no hardware here. wasmtime supports both; the crate has no OS-specific code.
- Miri, ASAN and TSAN: the crate has no `unsafe`, Miri cannot run Cranelift's JIT, and a sanitizer over wasmtime would test upstream, not this code. The thread-safety of the pool, ticker and gate is covered by the 8-thread and queueing tests and by Rust's type system.
- Not yet reachable from a node: wiring is M24.04. No external example module until M24.05, so no large-module load time.

## Review

Independent review is recorded on the pull request.
