//! The extension module contract that does not depend on how a module is loaded.
//!
//! This is the safe half of M24 (PR-23, D18): ABI and engine API versions and the
//! rule that accepts them, status codes, hard ceilings, the strict descriptor
//! parser and the typed failures of `docs/plan/extension-modules.md` section 5.
//! The native loader (`hsearch-module-native`) and the WASM host both build on
//! it, so a module declared once is judged the same way in either form. Nothing
//! here opens a library, runs a module or allocates by what a module reports.

mod descriptor;
mod error;
mod log;
mod request;
mod version;

pub use descriptor::{Descriptor, DescriptorError, ExtensionPoint, Kind};
pub use error::{CallError, FailureKind, MAX_ERROR_DETAIL_BYTES};
pub use log::{LogRecord, LogSink};
pub use request::Request;
pub use version::{ENGINE_API_VERSION, Version, abi_accepted, pack_abi};

/// ABI major in this header's `HS_ABI_VERSION_MAJOR`.
pub const ABI_MAJOR: u32 = 1;
/// ABI minor in this header's `HS_ABI_VERSION_MINOR`.
pub const ABI_MINOR: u32 = 0;
/// Packed `(major << 16) | minor` value `hs_abi_version` must return.
pub const ABI_VERSION: u32 = (ABI_MAJOR << 16) | ABI_MINOR;

/// Hard ceiling on any buffer crossing the boundary (`HS_MAX_BUFFER_BYTES`).
pub const MAX_BUFFER_BYTES: u32 = 16 * 1024 * 1024;
/// Hard ceiling on a descriptor (`HS_MAX_DESCRIPTOR_BYTES`).
pub const MAX_DESCRIPTOR_BYTES: u32 = 16 * 1024;
/// Hard ceiling on the opaque module configuration (`HS_MAX_CONFIG_BYTES`).
pub const MAX_CONFIG_BYTES: u32 = 64 * 1024;

/// Status codes a module returns. Zero succeeds and every failure is negative.
pub mod status {
    /// Success.
    pub const OK: i32 = 0;
    /// `out.len` holds the size required.
    pub const BUFFER_TOO_SMALL: i32 = -1;
    /// Malformed input for the point.
    pub const INVALID_ARGUMENT: i32 = -2;
    /// Unknown kind, point or version.
    pub const UNSUPPORTED: i32 = -3;
    /// The module refused an over-limit input.
    pub const LIMIT: i32 = -4;
    /// `cancelled()` returned nonzero and the module stopped.
    pub const CANCELLED: i32 = -5;
    /// Module failure; optional UTF-8 detail written to the output buffer.
    pub const MODULE: i32 = -6;
    /// A call arrived before a successful init.
    pub const NOT_INITIALIZED: i32 = -7;

    /// True for a code the contract defines; anything else is malformed.
    #[must_use]
    pub const fn is_defined(code: i32) -> bool {
        code <= OK && code >= NOT_INITIALIZED
    }
}
