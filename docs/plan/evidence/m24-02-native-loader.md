# M24.02 evidence: native module loader crate

Code and measurements, on Linux x86_64 only. Numbers were taken on a shared 4-vCPU cloud VM and are **provisional**: relative, not absolute. Nothing was run on aarch64 or macOS.

## Delivered

- `crates/module-abi` (safe): ABI and engine API versions and the version rule, status codes, hard ceilings, the strict descriptor parser, and the typed failures of spec section 5 (`ModuleBusy`, `ModuleError`, `ModuleOutputMalformed`, `ModuleLimit`, `ModuleTrapped`, `ModuleVersionRefused`, `ModuleUnavailable`, module text kept out of `Display`). Shared with the WASM host in M24.03.
- `crates/module-native`: the loader. `unsafe` lives in `mod sys` only, whose header is the written safety proof. Staging reads the file once through a 64 KiB buffer into a sealed `memfd` while hashing, compares the pin, scans `DT_NEEDED` with a bounded safe ELF reader, loads from the sealed copy with `RTLD_NOW | RTLD_LOCAL`, resolves and calls `hs_abi_version` alone, then binds, initialises, describes and validates. A bounded pool (`min(max_concurrent_calls, policy)` workers, three call slots per worker) runs calls with a per-call deadline, cooperative cancel through `cancelled`, a grace period, quarantine of lent buffers, output validation with guard bytes, a retry-once buffer protocol, a strike rule, and a rate-limited, 1 KiB-bounded log sink. See spec section 12 for each decision.
- Workspace exception: `[workspace.metadata.unsafe] crates = ["hsearch-module-native"]` in `Cargo.toml`; the loader crate restates the workspace lints with `unsafe_code = "deny"`. `tools/workspace.py check` refuses any other crate that does not inherit `[workspace.lints]` and any change to that list; `selftest` gained six negative controls for it. AGENTS.md rule amended. No new gate. `git diff Cargo.toml` shows the member entries and the exception list, and nothing else.
- Both crates are in `scripts/coverage.sh`.

## Validation (the task's list)

`crates/module-native/tests/load.rs` compiles `tests/c/hs_test_module.c` with `cc` into real shared libraries, one compile-time variant per broken rule (26 tests):

- valid module loads, echoes, reports kind, point id and a nonzero call id; undeclared point, wrong kind, over-limit input and invalid bounds are refused;
- wrong version: major 2, major 0 and a newer minor are refused **by version, with only `hs_abi_version` exported**, which proves no other symbol was resolved first; an older minor is accepted; a newer engine API is refused (`ModuleVersionRefused`) and the module is shut down;
- missing symbol (`hs_module_call`) refused; hash mismatch, text file, empty file, absent path, directory, oversize file, oversize config and a library needing `libstdc++` refused;
- bad descriptors (not JSON, unknown key, length lie, describe failure, overrun) refused, with `hs_module_shutdown` run after a successful init and not after a failed one;
- malformed buffers: length larger than the buffer, "too small" with a need that already fits, an undefined status, a module that needs a bigger buffer twice, and an overrun past the output buffer (guard bytes), each refused with `ModuleOutputMalformed`; three strikes in the window fail the module, strikes age out, an overrun fails it at once;
- module-returned statuses become typed errors and the module's text is in `detail` only, never in `Display`;
- cooperative cancel at the deadline stops one call and not its neighbour; a hung call returns the caller at the deadline plus grace, marks the module failed, counts the lent bytes as quarantined until the call returns, and refuses later calls; a full queue is `ModuleBusy` and retriable; a queued call that cannot start in time is abandoned and its slot recycled;
- 8 threads x 200 calls get their own bytes back and unique call ids; log lines are cut to 1 KiB and rate limited; `close` runs `shutdown` once after the last call and a hung call holds it back; the pool is capped by policy.
- `sys.rs` unit tests hold every `#[repr(C)]` struct to `fixtures/extensions/abi-v1/layout.json` (all 21 entries), limits, kinds and status codes to the same golden, and drive the `extern "C"` callbacks through the very pointers a module receives (null message, 3,000-byte message, null context, a panicking host).

### Sanitizers

- **Miri** (`cargo +nightly miri test -p hsearch-module-abi -p hsearch-module-native --lib`): the callback and layout tests pass. Miri found a real Stacked Borrows problem while the loader was being written: the host table was a boxed value, and moving the box invalidated the pointer the module keeps. Both host allocations are now raw pointers freed in `Drop` (see the `Host` comment). Miri cannot `dlopen`, so it covers the safe parts and the callbacks only.
- **AddressSanitizer and ThreadSanitizer** (`RUSTFLAGS=-Zsanitizer=address|thread`, nightly, TSAN with `-Zbuild-std`): all 26 loader tests pass with zero reports from either. The C test module is not itself instrumented.
- **valgrind memcheck** over a run of 500 and of 1,500 calls through a real module: 0 errors, 0 bytes definitely lost.

### Tests were seen to fail

Six deliberate mutations, each reverted, each caught: hash comparison skipped (`refuses_a_file_that_is_not_the_pinned_one`), version rule reversed to accept a newer minor (two abi tests), guard-byte check removed (`an_overrun_fails_the_module_at_once`), cancellation flag made a no-op (`cancellation_follows_the_owning_call_id`), quarantine accounting removed (`a_hung_call_fails_the_module_and_quarantines...`), `shutdown` skipped after a refused descriptor (two tests).

### Coverage

`cargo llvm-cov` (line floor 90): `hsearch-module-abi` 98.09% lines, `hsearch-module-native` 96.11% lines (module.rs 92.94%, the lowest file).

## Call cost (`cargo run --release -p hsearch-module-native --example bench`)

Echo point, one caller, 200,000 calls each, ns per call. "Direct" is the same C function called straight from one C thread with no hop and no copies (`examples/direct.c`).

| payload | pooled p50 | pooled p99 | direct p50 |
| --- | --- | --- | --- |
| 64 B | 28,580 | 79,705 | 27 |
| 4 KiB | 29,954 | 92,430 | 70 |
| 64 KiB | 45,771 | 123,072 | 1,305 |

**The cost is the thread hop, and the hop is the platform's.** A bare `std::sync::mpsc` ping-pong between two threads on the same VM measures 26,228 ns p50 and 60,242 ns p99, so the pool's own machinery (mutexes, the slot, one copy in and one out) adds about 2 to 4 µs over the floor for small payloads. A wake-up on this virtualised host is slow; bare-metal figures were not measured. Removing a second wake-up (the worker no longer notifies the caller when a call merely starts) was tried and moved nothing measurable, because the floor is the two remaining wake-ups.

Throughput, 4 KiB echo, 8 workers: 1 caller 31,676 calls/s; 2 callers 88,994; 4 callers 154,535; 8 callers 233,594; 16 callers 281,718 (the VM has 4 vCPUs).

**Allocations:** valgrind counts 225 allocations in total for 500 calls and 225 for 1,500: a steady-state call allocates nothing. **Memory:** the pool holds three slots per worker, each with an input buffer (at most the policy's 1 MiB input bound) and an output buffer (the point's bound plus 16 guard bytes), allocated on first use and reused; a buffer grown by the retry path is released after the call. Whole-process RSS after the benchmark was 5.1 MB with 8 workers. The pool adds 8 threads per loaded module at the descriptor's concurrency; the policy caps workers at 1,024.

## Trade-offs, stated

- A pooled call costs one scheduler round trip (about 26 µs here, platform-dependent). That is acceptable per block (an analyzer or ingest batch, a scoring block) and wrong per value, which is why the contract is block-shaped. A caller that wants no hop would run the module on its own thread, and a hung module would then hold that thread: the spec chose the pool, this task built it, and an inline mode is not provided.
- One extra copy of the input and one of the output, so that a call that outlives its deadline keeps owning what it was lent. This replaces the "zero copies" sentence of spec section 8, which is amended.
- The library and its sealed descriptor are never released: loading is configuration at start-up, and unloading cannot be proven safe against threads a module started.
- Linux only. The guard bytes detect an overrun after the fact; they are not isolation. A panic or crash inside a native module takes the node down (D18). Load time was 0.56 to 0.60 ms for a 16 KB test module (hash, sealed copy, `dlopen`, init, describe, 8 threads); hash time scales with file size and was not measured on a large module.

## Not run, and why

- aarch64 and macOS: no hardware here; the macOS copy-and-hash path is not built.
- Large-module hash and load time: no large module exists until M24.05.
- The loader is not yet reachable from a node: wiring is M24.04.

## Review

Independent review is recorded on the pull request.
