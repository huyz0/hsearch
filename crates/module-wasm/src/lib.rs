//! WASM extension module host (M24.03, PR-23, decision D18).
//!
//! A module is a core WebAssembly (wasm32) module run in-process by `wasmtime`
//! (Cranelift only), under limits the host sets and the module cannot change:
//!
//! - **Memory:** the module must declare a maximum no larger than
//!   [`Limits::max_memory_bytes`], and the store's resource limiter refuses growth
//!   past it; growth that fails traps the call.
//! - **CPU:** every call gets [`Limits::fuel_per_call`] units of fuel and a
//!   wall-clock deadline enforced by epoch interruption; either stops the call.
//! - **Host calls:** the only imports are `hsearch_host_v1.log` and
//!   `hsearch_host_v1.cancelled`. A module with any other import is refused at
//!   load. There is no WASI, so no file, socket, clock, randomness or environment.
//!   Calls to the two imports are counted against [`Limits::host_calls_per_call`].
//! - **Isolation of failure:** a trap, fuel, memory or deadline breach discards
//!   that instance and fails that call; the node keeps serving, other instances
//!   are untouched, and repeated breaches mark the module `failed`.
//!
//! The host reads and writes guest memory only inside the two I/O regions, with
//! every offset and length checked against the memory size. The crate contains no
//! `unsafe`. See `docs/plan/extension-modules.md` sections 4, 5 and 13.

mod host;
mod load;
mod module;

pub use hsearch_module_abi::Request;
pub use load::LoadError;
pub use module::{Limits, ModuleState, WasmConfig, WasmModule};
