//! The only `unsafe` in the workspace.
//!
//! # Safety proof
//!
//! Everything unsafe is here and falls into four kinds. Each states what the
//! safe wrapper guarantees and what it needs from the module, which is trusted
//! by configuration (D18): a module that breaks its half of the contract can
//! corrupt the node, and nothing in this file can prevent that.
//!
//! 1. **Opening** (`Library::open`). `dlopen` runs the library's static
//!    constructors. That cannot be made safe; the protection is that only a
//!    file whose SHA-256 equals the pin in node configuration reaches here
//!    (`image::stage`). Flags are `RTLD_NOW | RTLD_LOCAL`: every symbol is
//!    bound at load, so no lazy resolution can run module code later, and the
//!    module's symbols do not enter the global scope or interpose on the node's.
//! 2. **Resolving** (`Library::symbol`). A symbol is looked up by its fixed name
//!    and used as the function pointer type the golden header declares. The
//!    `layout` test holds every `#[repr(C)]` struct below to `layout.json`, and
//!    the five signatures are those of `hsearch_module.h`. A module exporting
//!    the name with a different signature is a contract violation of a trusted
//!    module. `hs_abi_version` is resolved and called alone first; a refused
//!    module has had no other symbol resolved or called.
//! 3. **Calling.** Every pointer passed is derived from a live Rust reference
//!    or slice for the whole call, with `len` and `cap` equal to the real
//!    lengths, and no pointer is retained after the call returns: the callee is
//!    told the buffers are lent for one call. A call is made at most
//!    `max_concurrent_calls` times at once because the pool has that many
//!    workers. `hs_module_shutdown` is called by `Bound::shutdown`, which takes
//!    `self` by value after every worker has exited.
//! 4. **Callbacks into Rust** (`log`, `cancelled`). They are `extern "C"` and
//!    never unwind (the body is wrapped in `catch_unwind`; a panic returns the
//!    safe default). The `ctx` pointer is the address of a `Host` that outlives
//!    every call because `Bound` keeps it in an `Arc` it never drops while
//!    code is loaded, and the library is never unloaded (below). The message
//!    pointer is read only for `min(len, 1 KiB)` bytes and only when non-null;
//!    a module that passes a short allocation with a long `len` has broken the
//!    contract it was given.
//!
//! **The library is never closed.** `dlclose` is not safe to promise against
//! threads a module started, thread-local destructors or `atexit` handlers it
//! registered, any of which would run unmapped code. Loaded code stays mapped
//! for the life of the process, so no function pointer here can dangle; the
//! price is address space for each library loaded, and a restart is the only
//! unload. Loading is by node configuration at start-up, so this is bounded by
//! configuration.
//!
//! **What is not covered**: a module that writes outside its buffers, calls
//! back with a forged pointer, never returns, or aborts. The host bounds the
//! consequences it can (guard bytes after each output buffer, a call deadline,
//! quarantine of lent buffers, a strike rule) and says so in the
//! specification's isolation table.

use libloading::os::unix::{Library as RawLibrary, RTLD_LOCAL, RTLD_NOW};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// `hs_slice`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HsSlice {
    pub ptr: *const u8,
    pub len: u64,
}

/// `hs_out`.
#[repr(C)]
pub struct HsOut {
    pub ptr: *mut u8,
    pub cap: u64,
    pub len: u64,
}

type LogFn = unsafe extern "C" fn(*mut c_void, u64, u32, *const u8, u64);
type CancelledFn = unsafe extern "C" fn(*mut c_void, u64) -> i32;

/// `hs_host_v1`.
#[repr(C)]
pub struct HsHostV1 {
    pub struct_size: u32,
    pub reserved0: u32,
    pub ctx: *mut c_void,
    pub log: Option<LogFn>,
    pub cancelled: Option<CancelledFn>,
}

/// `hs_call`.
#[repr(C)]
pub struct HsCall {
    pub struct_size: u32,
    pub kind: u32,
    pub point: HsSlice,
    pub input: HsSlice,
    pub timeout_ms: u32,
    pub reserved0: u32,
    pub call_id: u64,
}

/// What the host gives a module's callbacks: per-call cancellation and a sink.
pub trait Callbacks: Sync {
    /// True when the call named by `call_id` was abandoned by its caller.
    fn cancelled(&self, call_id: u64) -> bool;
    /// A module log line, already bounded to 1 KiB.
    fn log(&self, call_id: u64, level: u32, message: &[u8]);
}

/// Most log bytes read from a module per message.
const MAX_LOG_BYTES: u64 = 1024;

/// The host struct a module keeps for the life of the module.
///
/// Both allocations are held as raw pointers, not boxes: a module (C code) keeps
/// the address handed to `hs_module_init`, and moving a `Box` asserts unique
/// ownership of what it points at, which would invalidate a pointer derived
/// earlier. A raw pointer can move freely; `Drop` frees both exactly once.
pub struct Host {
    raw: *mut HsHostV1,
    holder: *mut Box<dyn Callbacks + Send>,
}

// SAFETY: `raw` and `holder` are owned by this struct, created in `new`, freed
// only in `drop`, and never aliased mutably after construction: `raw` is only
// read and `holder` is only used through `&dyn Callbacks`, which is `Sync`.
unsafe impl Send for Host {}
// SAFETY: as above; shared access only reads.
unsafe impl Sync for Host {}

impl Drop for Host {
    fn drop(&mut self) {
        // SAFETY: both pointers came from `Box::into_raw` in `new` and are freed here only.
        unsafe {
            drop(Box::from_raw(self.raw));
            drop(Box::from_raw(self.holder));
        }
    }
}

unsafe extern "C" fn log_trampoline(
    ctx: *mut c_void,
    call_id: u64,
    level: u32,
    msg: *const u8,
    len: u64,
) {
    if ctx.is_null() || msg.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `ctx` is the address of the `Box<dyn Callbacks + Send>` the
        // `Host` owns (proof, point 4). `msg` is non-null and the module vouches
        // for `len` bytes, of which at most `MAX_LOG_BYTES` are read.
        let (callbacks, bytes) = unsafe {
            (
                &**ctx.cast::<Box<dyn Callbacks + Send>>(),
                std::slice::from_raw_parts(msg, len.min(MAX_LOG_BYTES) as usize),
            )
        };
        callbacks.log(call_id, level, bytes);
    }));
}

unsafe extern "C" fn cancelled_trampoline(ctx: *mut c_void, call_id: u64) -> i32 {
    if ctx.is_null() {
        return 1;
    }
    // A panic means the host could not answer; stopping is the safe reply.
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: as in `log_trampoline`.
        let callbacks = unsafe { &**ctx.cast::<Box<dyn Callbacks + Send>>() };
        i32::from(callbacks.cancelled(call_id))
    }))
    .unwrap_or(1)
}

impl Host {
    /// Builds the host struct handed to `hs_module_init`.
    pub fn new(callbacks: Box<dyn Callbacks + Send>) -> Self {
        let holder = Box::into_raw(Box::new(callbacks));
        let raw = Box::into_raw(Box::new(HsHostV1 {
            struct_size: std::mem::size_of::<HsHostV1>() as u32,
            reserved0: 0,
            ctx: holder.cast::<c_void>(),
            log: Some(log_trampoline),
            cancelled: Some(cancelled_trampoline),
        }));
        Self { raw, holder }
    }

    /// The pointer the module keeps; valid until this `Host` is dropped.
    pub(crate) const fn as_ptr(&self) -> *const HsHostV1 {
        self.raw
    }
}

/// An opened library that has not been bound to its five entry points.
pub struct Library {
    // Never dropped, so never `dlclose`d, on any path including a refusal.
    raw: std::mem::ManuallyDrop<RawLibrary>,
}

/// Why a symbol or the library could not be used.
pub enum OpenError {
    /// `dlopen` failed; the text is the loader's.
    Open(String),
    /// A required export is absent.
    Missing(&'static str),
}

type AbiVersionFn = unsafe extern "C" fn() -> u32;
type InitFn = unsafe extern "C" fn(*const HsHostV1, HsSlice) -> i32;
type DescribeFn = unsafe extern "C" fn(*mut HsOut) -> i32;
type CallFn = unsafe extern "C" fn(*const HsCall, *mut HsOut) -> i32;
type ShutdownFn = unsafe extern "C" fn();

impl Library {
    /// Opens the library at `path` with immediate binding and local scope.
    pub fn open(path: &str) -> Result<Self, OpenError> {
        // SAFETY: proof, point 1. The caller has verified the SHA-256 of the
        // bytes behind `path`; constructors run here and are trusted by pin.
        unsafe { RawLibrary::open(Some(path), RTLD_NOW | RTLD_LOCAL) }
            .map(|raw| Self {
                raw: std::mem::ManuallyDrop::new(raw),
            })
            .map_err(|e| OpenError::Open(e.to_string()))
    }

    fn symbol<T: Copy>(&self, name: &'static str) -> Result<T, OpenError> {
        let mut nul = Vec::with_capacity(name.len() + 1);
        nul.extend_from_slice(name.as_bytes());
        nul.push(0);
        // SAFETY: proof, point 2. `T` is one of the five fn pointer types above,
        // named by the caller from the header's fixed symbol list.
        let symbol =
            unsafe { self.raw.get::<T>(&nul[..]) }.map_err(|_| OpenError::Missing(name))?;
        Ok(*symbol)
    }

    /// Resolves and calls `hs_abi_version`, and nothing else.
    pub fn abi_version(&self) -> Result<u32, OpenError> {
        let f: AbiVersionFn = self.symbol("hs_abi_version")?;
        // SAFETY: proof, point 3. No arguments; returns a plain integer.
        Ok(unsafe { f() })
    }

    /// Resolves the other four symbols. Call only after the version was accepted.
    pub fn bind(self) -> Result<Bound, OpenError> {
        let bound = Bound {
            init: self.symbol("hs_module_init")?,
            describe: self.symbol("hs_module_describe")?,
            call: self.symbol("hs_module_call")?,
            shutdown: self.symbol("hs_module_shutdown")?,
        };
        // `self.raw` is never dropped (proof, "The library is never closed"): the
        // code the four pointers name stays mapped, so they cannot dangle.
        Ok(bound)
    }
}

/// The four entry points after the version was accepted. Plain function
/// pointers into code that stays mapped, so `Bound` is `Copy` and `Sync`.
#[derive(Clone, Copy)]
pub struct Bound {
    init: InitFn,
    describe: DescribeFn,
    call: CallFn,
    shutdown: ShutdownFn,
}

impl Bound {
    /// `hs_module_init`.
    pub fn init(&self, host: &Host, config: &[u8]) -> i32 {
        let slice = HsSlice {
            ptr: config.as_ptr(),
            len: config.len() as u64,
        };
        // SAFETY: proof, point 3. `host.raw` is valid and pinned for the life of
        // the module; `config` outlives the call.
        unsafe { (self.init)(host.as_ptr(), slice) }
    }

    /// `hs_module_describe` into `buf`; returns the status and the length written.
    pub fn describe(&self, buf: &mut [u8]) -> (i32, u64) {
        let mut out = HsOut {
            ptr: buf.as_mut_ptr(),
            cap: buf.len() as u64,
            len: 0,
        };
        // SAFETY: proof, point 3. `out` describes exactly `buf`.
        let code = unsafe { (self.describe)(&raw mut out) };
        (code, out.len)
    }

    /// `hs_module_call`: `out` is the whole writable region the module may use.
    pub fn call(
        &self,
        kind: u32,
        point: &[u8],
        input: &[u8],
        timeout_ms: u32,
        call_id: u64,
        out: &mut [u8],
    ) -> (i32, u64) {
        let call = HsCall {
            struct_size: std::mem::size_of::<HsCall>() as u32,
            kind,
            point: HsSlice {
                ptr: point.as_ptr(),
                len: point.len() as u64,
            },
            input: HsSlice {
                ptr: input.as_ptr(),
                len: input.len() as u64,
            },
            timeout_ms,
            reserved0: 0,
            call_id,
        };
        let mut hs_out = HsOut {
            ptr: out.as_mut_ptr(),
            cap: out.len() as u64,
            len: 0,
        };
        // SAFETY: proof, point 3. Both structs and the slices they point at live
        // until this returns; `hs_out` describes exactly `out`.
        let code = unsafe { (self.call)(&raw const call, &raw mut hs_out) };
        (code, hs_out.len)
    }

    /// `hs_module_shutdown`. Takes `self` so it can run once; the caller
    /// guarantees no call is outstanding.
    pub fn shutdown(self) {
        // SAFETY: proof, point 3. Called once, after every worker has exited.
        unsafe { (self.shutdown)() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// The structs are held to `fixtures/extensions/abi-v1/layout.json`, the same
    /// golden the C header's layout is checked against.
    #[test]
    fn layout_matches_the_golden_header_layout() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/extensions/abi-v1/layout.json"
        );
        let golden: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("layout.json")).expect("json");
        let want = |key: &str| {
            golden["layout"][key]
                .as_u64()
                .unwrap_or_else(|| panic!("{key} missing")) as usize
        };
        let ours = [
            ("hs_slice.size", size_of::<HsSlice>()),
            ("hs_slice.ptr", offset_of!(HsSlice, ptr)),
            ("hs_slice.len", offset_of!(HsSlice, len)),
            ("hs_out.size", size_of::<HsOut>()),
            ("hs_out.ptr", offset_of!(HsOut, ptr)),
            ("hs_out.cap", offset_of!(HsOut, cap)),
            ("hs_out.len", offset_of!(HsOut, len)),
            ("hs_host_v1.size", size_of::<HsHostV1>()),
            ("hs_host_v1.struct_size", offset_of!(HsHostV1, struct_size)),
            ("hs_host_v1.reserved0", offset_of!(HsHostV1, reserved0)),
            ("hs_host_v1.ctx", offset_of!(HsHostV1, ctx)),
            ("hs_host_v1.log", offset_of!(HsHostV1, log)),
            ("hs_host_v1.cancelled", offset_of!(HsHostV1, cancelled)),
            ("hs_call.size", size_of::<HsCall>()),
            ("hs_call.struct_size", offset_of!(HsCall, struct_size)),
            ("hs_call.kind", offset_of!(HsCall, kind)),
            ("hs_call.point", offset_of!(HsCall, point)),
            ("hs_call.input", offset_of!(HsCall, input)),
            ("hs_call.timeout_ms", offset_of!(HsCall, timeout_ms)),
            ("hs_call.reserved0", offset_of!(HsCall, reserved0)),
            ("hs_call.call_id", offset_of!(HsCall, call_id)),
        ];
        assert_eq!(
            golden["layout"].as_object().map(serde_json::Map::len),
            Some(ours.len()),
            "every golden field is checked"
        );
        for (key, got) in ours {
            assert_eq!(got, want(key), "{key}");
        }
    }

    type Seen = std::sync::Arc<std::sync::Mutex<Vec<(u64, u32, Vec<u8>)>>>;

    struct Probe {
        seen: Seen,
        panic: bool,
    }

    impl Callbacks for Probe {
        fn cancelled(&self, call_id: u64) -> bool {
            assert!(!self.panic, "host bug");
            call_id == 7
        }
        fn log(&self, call_id: u64, level: u32, message: &[u8]) {
            assert!(!self.panic, "host bug");
            self.seen
                .lock()
                .expect("seen")
                .push((call_id, level, message.to_vec()));
        }
    }

    /// The `extern "C"` callbacks are exercised through the very pointers a module
    /// is handed, including their guards: a null message, a message over the 1 KiB
    /// bound, and a panic in the host, which must not unwind into C.
    #[test]
    fn host_callbacks_are_bounded_and_never_unwind() {
        let seen = Seen::default();
        let host = Host::new(Box::new(Probe {
            seen: Seen::clone(&seen),
            panic: false,
        }));
        // SAFETY: `as_ptr` is valid while `host` lives.
        let table = unsafe { &*host.as_ptr() };
        let (ctx, log, cancelled) = (
            table.ctx,
            table.log.expect("log"),
            table.cancelled.expect("cancelled"),
        );
        assert_eq!(table.struct_size as usize, size_of::<HsHostV1>());
        let long = vec![b'a'; 3000];
        // SAFETY: the pointers are the ones `Host::new` built; `msg` and `len` describe live buffers.
        unsafe {
            log(ctx, 5, 3, b"hi".as_ptr(), 2);
            log(ctx, 5, 2, long.as_ptr(), long.len() as u64);
            log(ctx, 5, 1, std::ptr::null(), 10);
            log(std::ptr::null_mut(), 5, 1, b"x".as_ptr(), 1);
            assert_eq!(cancelled(ctx, 7), 1);
            assert_eq!(cancelled(ctx, 8), 0);
            assert_eq!(cancelled(std::ptr::null_mut(), 8), 1, "no host means stop");
        }
        let recorded = seen.lock().expect("seen").clone();
        assert_eq!(
            recorded.len(),
            2,
            "a null message or null context logs nothing"
        );
        assert_eq!(
            (recorded[0].0, recorded[0].1, recorded[0].2.as_slice()),
            (5, 3, &b"hi"[..])
        );
        assert_eq!(
            recorded[1].2.len(),
            1024,
            "a 3000 byte message is read to the 1 KiB bound only"
        );

        let panicking = Host::new(Box::new(Probe {
            seen: Seen::default(),
            panic: true,
        }));
        // SAFETY: as above.
        let table = unsafe { &*panicking.as_ptr() };
        let (ctx, log, cancelled) = (
            table.ctx,
            table.log.expect("log"),
            table.cancelled.expect("cancelled"),
        );
        // SAFETY: as above. The panics are caught inside the trampolines.
        unsafe {
            log(ctx, 1, 1, b"x".as_ptr(), 1);
            assert_eq!(cancelled(ctx, 1), 1, "a host panic answers 'stop'");
        }
    }

    #[test]
    fn limits_and_codes_match_the_golden() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/extensions/abi-v1/layout.json"
        );
        let golden: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("layout.json")).expect("json");
        assert_eq!(golden["abi_version"], hsearch_module_abi::ABI_VERSION);
        assert_eq!(
            golden["limits"]["buffer"],
            hsearch_module_abi::MAX_BUFFER_BYTES
        );
        assert_eq!(
            golden["limits"]["descriptor"],
            hsearch_module_abi::MAX_DESCRIPTOR_BYTES
        );
        assert_eq!(
            golden["limits"]["config"],
            hsearch_module_abi::MAX_CONFIG_BYTES
        );
        assert_eq!(
            golden["limits"]["error_detail"],
            hsearch_module_abi::MAX_ERROR_DETAIL_BYTES
        );
        let codes = &golden["status"];
        assert_eq!(codes["HS_OK"], hsearch_module_abi::status::OK);
        assert_eq!(
            codes["HS_E_BUFFER_TOO_SMALL"],
            hsearch_module_abi::status::BUFFER_TOO_SMALL
        );
        assert_eq!(
            codes["HS_E_INVALID_ARGUMENT"],
            hsearch_module_abi::status::INVALID_ARGUMENT
        );
        assert_eq!(
            codes["HS_E_UNSUPPORTED"],
            hsearch_module_abi::status::UNSUPPORTED
        );
        assert_eq!(codes["HS_E_LIMIT"], hsearch_module_abi::status::LIMIT);
        assert_eq!(
            codes["HS_E_CANCELLED"],
            hsearch_module_abi::status::CANCELLED
        );
        assert_eq!(codes["HS_E_MODULE"], hsearch_module_abi::status::MODULE);
        assert_eq!(
            codes["HS_E_NOT_INITIALIZED"],
            hsearch_module_abi::status::NOT_INITIALIZED
        );
        assert_eq!(golden["kinds"]["analyzer"], 1);
        assert_eq!(golden["kinds"]["scoring"], 4);
    }
}
