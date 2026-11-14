//! Native C-ABI extension module loader (M24.02, PR-23, decision D18).
//!
//! The one crate in the workspace where `unsafe` is allowed, and inside it only
//! `mod sys`, whose header carries the written safety proof. Everything else
//! here is safe code: staging and hashing the file ([`image`]), reading its
//! dependencies ([`elf`]), version and descriptor checks (from
//! `hsearch-module-abi`), and the bounded call pool ([`NativeModule`]).
//!
//! A native module is **trusted**: it shares the node's address space and can
//! take it down. The loader bounds what it can bound (the file is hashed and
//! loaded from a sealed copy, its dependencies are limited, its output is
//! validated, a hung call costs a pool slot and quarantines its buffers) and
//! never claims more. See `docs/plan/extension-modules.md` sections 1 and 3.

mod elf;
mod host;
mod image;
mod module;
#[allow(unsafe_code)]
mod sys;

pub use host::{LogRecord, LogSink};
pub use hsearch_module_abi::Request;
pub use module::{LoadConfig, LoadError, ModuleState, NativeModule, Policy};
