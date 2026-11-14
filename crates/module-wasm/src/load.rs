//! Load-time refusals: the file, its imports, its exports and its memory.

use std::fmt;
use wasmtime::{ExternType, Module, ValType};

use hsearch_module_abi::{FailureKind, Version};

const PAGE: u64 = 65_536;

/// Why a module was not loaded. Nothing of it is registered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadError {
    /// The file could not be read.
    Io(&'static str),
    /// The file is over the configured size.
    TooLarge,
    /// The file's SHA-256 is not the pinned one.
    HashMismatch,
    /// The bytes are not a valid core WebAssembly module (or use a disabled feature).
    Invalid(&'static str),
    /// The module imports something other than `hsearch_host_v1.log` and `.cancelled`
    /// with their exact types. The text is the import's `module.name`.
    ForbiddenImport(String),
    /// A required export is absent or has the wrong type.
    BadExport(&'static str),
    /// The memory has no declared maximum, a maximum over the limit, or is shared or 64-bit.
    MemoryLimit(&'static str),
    /// `hs_abi_version` returned a version this host does not accept.
    AbiRefused {
        /// What the module reported.
        module: u32,
        /// What the host implements.
        host: u32,
    },
    /// The module configuration does not fit its input region or exceeds 64 KiB.
    ConfigTooLarge,
    /// A call during loading (version, describe or init) trapped or ran out of fuel.
    Trapped(&'static str),
    /// `hs_module_init` returned this status.
    InitFailed(i32),
    /// `hs_module_describe` failed or wrote an invalid descriptor.
    BadDescriptor(&'static str),
    /// The descriptor disagrees with the module or the configuration, or its regions are unusable.
    DescriptorMismatch(&'static str),
    /// The descriptor requires an engine API this host does not implement.
    EngineApiRefused(Version),
}

impl LoadError {
    /// Version refusals are their own class, the rest are "unavailable".
    #[must_use]
    pub const fn failure_kind(&self) -> FailureKind {
        match self {
            Self::AbiRefused { .. } | Self::EngineApiRefused(_) => FailureKind::VersionRefused,
            _ => FailureKind::Unavailable,
        }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(why)
            | Self::Invalid(why)
            | Self::BadExport(why)
            | Self::MemoryLimit(why)
            | Self::Trapped(why)
            | Self::BadDescriptor(why)
            | Self::DescriptorMismatch(why) => {
                write!(f, "wasm module refused: {why}")
            }
            Self::TooLarge => write!(f, "wasm module refused: file over the configured size"),
            Self::HashMismatch => write!(
                f,
                "wasm module refused: SHA-256 differs from the pinned value"
            ),
            Self::ForbiddenImport(name) => {
                write!(f, "wasm module refused: import {name} is not granted")
            }
            Self::AbiRefused { module, host } => write!(
                f,
                "wasm module refused: ABI {}.{} is not accepted by host {}.{}",
                module >> 16,
                module & 0xFFFF,
                host >> 16,
                host & 0xFFFF
            ),
            Self::ConfigTooLarge => write!(
                f,
                "wasm module refused: configuration does not fit the input region"
            ),
            Self::InitFailed(code) => {
                write!(f, "wasm module refused: hs_module_init returned {code}")
            }
            Self::EngineApiRefused(v) => write!(f, "wasm module refused: needs engine API {v}"),
        }
    }
}

impl std::error::Error for LoadError {}

fn sig(ty: &wasmtime::FuncType, params: &[ValType], results: &[ValType]) -> bool {
    let same = |have: Vec<ValType>, want: &[ValType]| {
        have.len() == want.len() && have.iter().zip(want).all(|(a, b)| ValType::eq(a, b))
    };
    same(ty.params().collect(), params) && same(ty.results().collect(), results)
}

/// Checks imports, exports and memory against `wasm-host-interface.json`.
pub(crate) fn check(module: &Module, max_memory_bytes: u64) -> Result<(), LoadError> {
    for import in module.imports() {
        let granted = import.module() == "hsearch_host_v1"
            && match (import.name(), import.ty()) {
                ("log", ExternType::Func(f)) => {
                    sig(&f, &[ValType::I32, ValType::I32, ValType::I32], &[])
                }
                ("cancelled", ExternType::Func(f)) => sig(&f, &[], &[ValType::I32]),
                _ => false,
            };
        if !granted {
            return Err(LoadError::ForbiddenImport(format!(
                "{}.{}",
                import.module(),
                import.name()
            )));
        }
    }
    let exports: Vec<(&str, ExternType)> = module.exports().map(|e| (e.name(), e.ty())).collect();
    let find = |name: &str| {
        exports
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| t.clone())
    };
    let func = |name: &'static str, params: &[ValType], results: &[ValType]| match find(name) {
        Some(ExternType::Func(f)) if sig(&f, params, results) => Ok(()),
        _ => Err(LoadError::BadExport(name)),
    };
    let i = ValType::I32;
    func("hs_abi_version", &[], std::slice::from_ref(&i))?;
    func(
        "hs_module_init",
        std::slice::from_ref(&i),
        std::slice::from_ref(&i),
    )?;
    func("hs_module_describe", &[], std::slice::from_ref(&i))?;
    func(
        "hs_module_call",
        &[i.clone(), i.clone(), i.clone(), i.clone()],
        std::slice::from_ref(&i),
    )?;
    func("hs_module_shutdown", &[], &[])?;
    func("hs_input_region", &[], std::slice::from_ref(&i))?;
    func("hs_output_region", &[], std::slice::from_ref(&i))?;
    func("hs_output_len", &[], &[i])?;
    let Some(ExternType::Memory(memory)) = find("memory") else {
        return Err(LoadError::BadExport("memory"));
    };
    if memory.is_shared() || memory.is_64() {
        return Err(LoadError::MemoryLimit(
            "the memory must be a plain 32-bit memory",
        ));
    }
    match memory.maximum() {
        None => Err(LoadError::MemoryLimit("the memory declares no maximum")),
        Some(max) if max.saturating_mul(PAGE) > max_memory_bytes => Err(LoadError::MemoryLimit(
            "the memory's declared maximum is over the host limit",
        )),
        Some(_) => Ok(()),
    }
}
