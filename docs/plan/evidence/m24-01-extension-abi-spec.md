# M24.01 evidence: extension module ABI, host interface and extension points

Documentation and fixtures only. No Rust code, no loader, no WASM runtime, and **no performance number was measured**: nothing here sits on a hot path yet. The call-cost, memory and instantiate-cost measurements are named in the spec (section 8) as validation for M24.02, M24.03 and M24.05.

## Delivered

- [extension-modules.md](../extension-modules.md): the contract for ABI 1.0 and engine API 1.0. Section 1 is the per-model isolation table; sections 2 to 7 are the descriptor, native ABI, WASM host interface, failure semantics, extension points with wire encodings, and enablement and mapping binding; section 8 is cost, memory and downsides; section 9 is the review against the example module's needs; section 10 reserves the out-of-process model.
- Golden fixtures, `fixtures/extensions/abi-v1/`: `hsearch_module.h` (the C ABI), `layout.json` (struct layout, limits, kinds, status codes, symbols and function signatures), `wasm-host-interface.json`, `descriptor.example.json`, `wire-vectors.json` (golden bytes for every extension point kind, produced by the reference encoder `tools/tests/extension_wire.py`; the analyzer vector is also checked against hand-computed bytes).
- `tools/tests/test_extension_abi.py` with `tools/tests/fixtures/extension_abi_probe.c`: compiles the header with `cc -std=c11 -Wall -Wextra -Werror -pedantic`, compares the layout it prints with `layout.json`, and holds the WASM interface, the example descriptor and the spec to the same version, status codes and symbols.
- `modules.md` no longer says native dynamic loading is not promised (D18 superseded that); `product-coverage.md` PR-23 links the spec.

## Observed

- The test file was run before the spec existed and failed on the missing spec file (the other nine tests passed because the header and fixtures already existed). With the spec in place all ten pass.
- Two deliberate header mutations each made `test_layout_matches_golden` fail, then were reverted: `timeout_ms` widened from `uint32_t` to `uint64_t`, and `HS_E_LIMIT` changed from -4 to -9.
- The header also compiles as C++17 (`c++ -fsyntax-only -std=c++17 -Wall -Werror`).
- Layout golden generated on x86_64 Linux. aarch64 is LP64 and every field is explicitly sized with no implicit padding, but the layout was **not run on aarch64**.

## Definition of done

- States what each model can and cannot isolate: spec section 1.
- No extension point lets a module read files, sockets or memory outside what the host grants: holds by interface for every point (a module receives and returns bytes only) and by enforcement for WASM (two imports, link-time refusal of anything else). **For native modules it does not hold by enforcement and the spec says so**; that is D18's trust model, not a gap in the spec.

## Not run, and why

- `Review the spec against the example module's needs`: done as a written walk-through (spec section 9), against the M24.05 task text; the example module does not exist yet. It first missed that the draft example query (`prefix_any`) cannot be expressed in the core IR; the independent review found it and the example was changed (see Review).
- The archived multi-role review packet and audited-seed machinery was not used (the harness is frozen); the independent review for this PR is a separate reviewer session reading the diff, recorded on the PR.
- No sanitizer or Miri run: there is no `unsafe` code in this task.

## Review

One independent reviewer session read the diff and returned 3 P1, 5 P2 and 4 P3 findings, fixed in a second commit: the `cancelled` and `log` callbacks now take a per-call `call_id` (field added to `hs_call`) with the host struct's lifetime stated; the example query was changed to one the core IR can express, and analysis of emitted Match/Term nodes is stated to be the field's bound analyzer; section 6 now gives exact widths, tags, the Bool child layout, kind and value numbering, bounds taken from the engine, and golden vectors; WASM retry, native hang bounds (queue, wait deadline, quarantine), the scoring floating-point caveat, loader limits (constructors run before the version check; dependencies not covered by the hash), ingest-runs-once, log cap, retriability and the WASM instance-pool memory cost are stated. The test now checks function signatures, replaces two vacuous tests with compile-time padding assertions in the C probe, and checks the vectors; a changed `cancelled` return type was observed to fail it. Not changed: the header does not carry per-platform calling-convention guarantees beyond the C ABI of the target; by-value `hs_slice` in `hs_module_init` is kept (16 bytes, one call at start-up) and noted for non-C bindings in M24.02.
