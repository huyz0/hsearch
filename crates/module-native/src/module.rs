//! A loaded native module and its bounded call pool.

use crate::host::{HostState, LogSink, SLOT_BITS, SharedCallbacks};
use crate::image;
use crate::sys::{Bound, Host, Library, OpenError};
use hsearch_module_abi::{
    ABI_VERSION, CallError, Descriptor, ENGINE_API_VERSION, FailureKind, Kind, MAX_BUFFER_BYTES,
    MAX_CONFIG_BYTES, MAX_DESCRIPTOR_BYTES, Version, abi_accepted, status,
};
use std::collections::VecDeque;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Bytes after every output buffer the module is not told about. A module that
/// writes into them has overrun its buffer, which is detected and fails it.
const GUARD: usize = 16;
const GUARD_BYTE: u8 = 0xA5;
/// Most worker threads for one module; keeps call ids' slot field in 16 bits.
const MAX_WORKERS: u32 = 1024;

/// Host-side bounds that are policy rather than contract.
#[derive(Clone, Debug)]
pub struct Policy {
    /// Largest call input accepted, at most `HS_MAX_BUFFER_BYTES`.
    pub max_input_bytes: u32,
    /// How long after its deadline a running call may take to stop before the
    /// module is marked failed (**proposed** 1 s).
    pub grace: Duration,
    /// Malformed outputs within `strike_window` that mark the module failed.
    pub strike_limit: usize,
    /// The strike window (**proposed** 60 s).
    pub strike_window: Duration,
    /// Most worker threads, whatever the descriptor asks for.
    pub max_workers: u32,
    /// Module log lines passed to the sink per second; the rest are counted.
    pub log_per_second: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_input_bytes: 1024 * 1024,
            grace: Duration::from_secs(1),
            strike_limit: 3,
            strike_window: Duration::from_secs(60),
            max_workers: 256,
            log_per_second: 50,
        }
    }
}

/// Everything the node's configuration says about one native module.
pub struct LoadConfig {
    /// The pinned SHA-256 of the module file; the loader refuses any other bytes.
    pub sha256: [u8; 32],
    /// The opaque module section handed to `hs_module_init`, at most 64 KiB.
    pub module_config: Vec<u8>,
    /// When set, the descriptor's name must equal it.
    pub expected_name: Option<String>,
    /// The ABI version this host implements; a module's must be accepted by it.
    pub host_abi: u32,
    /// The engine API version this host implements.
    pub host_engine_api: Version,
    /// Largest module file read.
    pub max_library_bytes: u64,
    /// Host-side bounds.
    pub policy: Policy,
    /// Where module log lines go; `None` drops them.
    pub log: Option<LogSink>,
}

impl LoadConfig {
    /// A configuration for a module pinned to `sha256`, with every default.
    #[must_use]
    pub fn new(sha256: [u8; 32]) -> Self {
        Self {
            sha256,
            module_config: Vec::new(),
            expected_name: None,
            host_abi: ABI_VERSION,
            host_engine_api: ENGINE_API_VERSION,
            max_library_bytes: 256 * 1024 * 1024,
            policy: Policy::default(),
            log: None,
        }
    }
}

/// Why a module was not loaded. Nothing of it is registered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadError {
    /// This platform cannot load native modules yet.
    Unsupported(&'static str),
    /// The file could not be read or staged.
    Io(&'static str),
    /// The file is over the configured size.
    TooLarge,
    /// The file's SHA-256 is not the pinned one.
    HashMismatch,
    /// The file is not a loadable 64-bit little-endian shared object.
    BadImage(&'static str),
    /// The library needs something beyond libc, libm and the compiler runtime.
    DisallowedDependency(String),
    /// The dynamic loader refused the library.
    Open(String),
    /// `hs_abi_version` returned a version this host does not accept.
    AbiRefused {
        /// What the module reported.
        module: u32,
        /// What the host implements.
        host: u32,
    },
    /// A required export is absent.
    MissingSymbol(&'static str),
    /// The module configuration is over 64 KiB.
    ConfigTooLarge,
    /// `hs_module_init` returned this status.
    InitFailed(i32),
    /// `hs_module_describe` failed or wrote an invalid descriptor.
    BadDescriptor(&'static str),
    /// The descriptor disagrees with the module or the configuration.
    DescriptorMismatch(&'static str),
    /// The descriptor requires an engine API this host does not implement.
    EngineApiRefused(Version),
}

impl LoadError {
    /// The class an operator sees: version refusals are their own, the rest are
    /// "unavailable".
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
            Self::Unsupported(why)
            | Self::Io(why)
            | Self::BadImage(why)
            | Self::BadDescriptor(why)
            | Self::DescriptorMismatch(why) => {
                write!(f, "native module refused: {why}")
            }
            Self::TooLarge => write!(f, "native module refused: file over the configured size"),
            Self::HashMismatch => write!(
                f,
                "native module refused: SHA-256 differs from the pinned value"
            ),
            Self::DisallowedDependency(name) => write!(
                f,
                "native module refused: needs {name}, beyond libc, libm and the compiler runtime"
            ),
            Self::Open(why) => write!(f, "native module refused: the dynamic loader said: {why}"),
            Self::AbiRefused { module, host } => write!(
                f,
                "native module refused: ABI {}.{} is not accepted by host {}.{}",
                module >> 16,
                module & 0xFFFF,
                host >> 16,
                host & 0xFFFF
            ),
            Self::MissingSymbol(name) => write!(f, "native module refused: missing export {name}"),
            Self::ConfigTooLarge => write!(f, "native module refused: configuration over 64 KiB"),
            Self::InitFailed(code) => {
                write!(f, "native module refused: hs_module_init returned {code}")
            }
            Self::EngineApiRefused(v) => write!(f, "native module refused: needs engine API {v}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// A module's state as capabilities report it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModuleState {
    /// Accepting calls.
    Ready,
    /// Marked failed: a hang, an overrun or too many strikes. No new calls
    /// until the node restarts.
    Failed,
}

/// One call into an extension point.
pub struct Request<'a> {
    /// The point's kind.
    pub kind: Kind,
    /// The point's id, as declared.
    pub point: &'a str,
    /// The point's input encoding, borrowed for the call.
    pub input: &'a [u8],
    /// The whole call's deadline, from submission: queue wait, run time and the
    /// cooperative cancel. Also the `timeout_ms` the module is told.
    pub timeout: Duration,
    /// The first output buffer: the largest result the point's bounds allow.
    pub output_bound: u32,
    /// The most the host will retry a too-small buffer up to; at least `output_bound`.
    pub output_limit: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Idle,
    Queued,
    Running,
    Done,
    Abandoned,
}

enum Fail {
    Status(i32, Vec<u8>),
    Malformed(&'static str),
    Limit(&'static str),
    Overrun,
}

struct SlotState {
    phase: Phase,
    kind: u32,
    point: usize,
    timeout_ms: u32,
    bound: u32,
    limit: u32,
    input: Vec<u8>,
    out: Vec<u8>,
    lent: u64,
    result: Option<Result<(usize, bool), Fail>>,
}

struct CallSlot {
    state: Mutex<SlotState>,
    changed: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Inner {
    desc: Descriptor,
    bound: Bound,
    // Kept alive: the module holds a pointer to it until shutdown returns.
    _host: Host,
    host_state: Arc<HostState>,
    slots: Box<[CallSlot]>,
    free: Mutex<Vec<usize>>,
    queue: Mutex<VecDeque<usize>>,
    work: Condvar,
    shutdown: AtomicBool,
    failed: AtomicBool,
    strikes: Mutex<VecDeque<Instant>>,
    quarantined: AtomicU64,
    seq: AtomicU64,
    live: Mutex<usize>,
    exited: Condvar,
    policy: Policy,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The last reference is gone: every worker has exited, so no call is
        // outstanding and the contract's "after the last call has returned" holds.
        self.bound.shutdown();
    }
}

/// A loaded, initialised native module with its bounded pool.
pub struct NativeModule {
    inner: Arc<Inner>,
    workers: Vec<JoinHandle<()>>,
}

impl fmt::Debug for NativeModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeModule")
            .field("name", &self.inner.desc.name)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

fn map_open(e: OpenError) -> LoadError {
    match e {
        OpenError::Open(why) => LoadError::Open(why),
        OpenError::Missing(name) => LoadError::MissingSymbol(name),
    }
}

impl NativeModule {
    /// Loads a module: stage and hash, open, check the ABI version alone, bind,
    /// initialise, describe and validate, then start the pool.
    ///
    /// # Errors
    /// [`LoadError`]; on any error nothing of the module is registered and, if
    /// `hs_module_init` had succeeded, `hs_module_shutdown` has been called.
    pub fn load(path: &Path, config: LoadConfig) -> Result<Self, LoadError> {
        if config.module_config.len() > MAX_CONFIG_BYTES as usize {
            return Err(LoadError::ConfigTooLarge);
        }
        let staged = image::stage(path, &config.sha256, config.max_library_bytes)?;
        let library = open_staged(&staged)?;
        staged.keep_open();
        // The version is read and judged before any other symbol is resolved.
        let reported = library.abi_version().map_err(map_open)?;
        if !abi_accepted(reported, config.host_abi) {
            return Err(LoadError::AbiRefused {
                module: reported,
                host: config.host_abi,
            });
        }
        let bound = library.bind().map_err(map_open)?;
        // The module keeps this host struct from init on, and the descriptor that
        // sizes the pool is not known until after init, so the cancellation table
        // is sized for the most workers policy allows.
        let slots = config.policy.max_workers.clamp(1, MAX_WORKERS) as usize * 3;
        let host_state = Arc::new(HostState::new(
            slots,
            config.log.clone(),
            config.policy.log_per_second,
        ));
        let host = Host::new(Box::new(SharedCallbacks(Arc::clone(&host_state))));
        let code = bound.init(&host, &config.module_config);
        if code != status::OK {
            return Err(LoadError::InitFailed(code));
        }
        let desc = match describe(&bound, &config, reported) {
            Ok(d) => d,
            Err(e) => {
                bound.shutdown();
                return Err(e);
            }
        };
        host_state.set_name(&desc.name);
        Ok(Self::start(bound, host, host_state, desc, config))
    }

    fn start(
        bound: Bound,
        host: Host,
        host_state: Arc<HostState>,
        desc: Descriptor,
        config: LoadConfig,
    ) -> Self {
        let workers = desc
            .max_concurrent_calls
            .min(config.policy.max_workers.clamp(1, MAX_WORKERS)) as usize;
        let total = workers * 3;
        let inner = Arc::new(Inner {
            desc,
            bound,
            _host: host,
            host_state,
            slots: (0..total)
                .map(|_| CallSlot {
                    state: Mutex::new(SlotState {
                        phase: Phase::Idle,
                        kind: 0,
                        point: 0,
                        timeout_ms: 0,
                        bound: 0,
                        limit: 0,
                        input: Vec::new(),
                        out: Vec::new(),
                        lent: 0,
                        result: None,
                    }),
                    changed: Condvar::new(),
                })
                .collect(),
            free: Mutex::new((0..total).rev().collect()),
            queue: Mutex::new(VecDeque::with_capacity(total)),
            work: Condvar::new(),
            shutdown: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            strikes: Mutex::new(VecDeque::new()),
            quarantined: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            live: Mutex::new(workers),
            exited: Condvar::new(),
            policy: config.policy,
        });
        let handles = (0..workers)
            .map(|i| {
                let inner = Arc::clone(&inner);
                std::thread::Builder::new()
                    .name(format!("hsearch-ext-{}-{i}", inner.desc.name))
                    .spawn(move || worker(&inner))
            })
            .collect::<Result<Vec<_>, _>>();
        // A thread that cannot start leaves the pool smaller than `live`; fail
        // the module rather than run with a count that lies.
        match handles {
            Ok(workers) => Self { inner, workers },
            Err(_) => {
                inner.failed.store(true, Ordering::Release);
                Self {
                    inner,
                    workers: Vec::new(),
                }
            }
        }
    }

    /// The validated descriptor.
    #[must_use]
    pub fn descriptor(&self) -> &Descriptor {
        &self.inner.desc
    }

    /// Ready or failed, as capabilities report it.
    #[must_use]
    pub fn state(&self) -> ModuleState {
        if self.inner.failed.load(Ordering::Acquire) {
            ModuleState::Failed
        } else {
            ModuleState::Ready
        }
    }

    /// Bytes lent to calls that outlived their deadline and have not returned.
    /// They are never freed or reused until the call returns.
    #[must_use]
    pub fn quarantined_bytes(&self) -> u64 {
        self.inner.quarantined.load(Ordering::Acquire)
    }

    /// Module log lines dropped by the rate limit.
    #[must_use]
    pub fn dropped_logs(&self) -> u64 {
        self.inner.host_state.dropped_logs()
    }

    /// Runs one call and appends its result to `out` (cleared first).
    ///
    /// # Errors
    /// A typed [`CallError`] naming the module and point; the module stays
    /// loaded unless the error marked it failed (see [`ModuleState`]).
    pub fn call(&self, request: &Request<'_>, out: &mut Vec<u8>) -> Result<(), CallError> {
        let inner = &*self.inner;
        let name = inner.desc.name.as_str();
        let err =
            |failure, reason| CallError::new(name, failure, reason).at(request.kind, request.point);
        if inner.failed.load(Ordering::Acquire) {
            return Err(err(FailureKind::Unavailable, "the module is marked failed"));
        }
        let Some(point) = inner
            .desc
            .extension_points
            .iter()
            .position(|p| p.kind == request.kind && p.id == request.point)
        else {
            return Err(err(
                FailureKind::Unavailable,
                "the module does not declare that point",
            ));
        };
        if request.input.len() > inner.policy.max_input_bytes.min(MAX_BUFFER_BYTES) as usize {
            return Err(err(
                FailureKind::Limit,
                "input over the host's per-call bound",
            ));
        }
        if request.output_bound == 0
            || request.output_limit < request.output_bound
            || request.output_limit > MAX_BUFFER_BYTES
        {
            return Err(err(FailureKind::Limit, "invalid output bound"));
        }
        let timeout_ms = u32::try_from(request.timeout.as_millis()).unwrap_or(u32::MAX);
        let deadline = Instant::now() + request.timeout;
        let Some(idx) = lock(&inner.free).pop() else {
            return Err(err(FailureKind::Busy, "the module's queue is full"));
        };
        let slot = &inner.slots[idx];
        {
            let mut s = lock(&slot.state);
            s.input.clear();
            s.input.extend_from_slice(request.input);
            // Sized here, on the caller's side, so the bytes lent to the worker are
            // known exactly if the call later has to be quarantined.
            let need = request.output_bound as usize + GUARD;
            if s.out.len() < need {
                s.out.resize(need, GUARD_BYTE);
            }
            (s.kind, s.point, s.timeout_ms, s.bound, s.limit) = (
                request.kind.code(),
                point,
                timeout_ms,
                request.output_bound,
                request.output_limit,
            );
            s.result = None;
            s.phase = Phase::Queued;
        }
        lock(&inner.queue).push_back(idx);
        inner.work.notify_one();
        // One wait, one wake: the worker notifies only when the call is done, so the
        // caller is not woken a second time when the call merely starts. What the
        // call was doing is read at the deadline instead.
        let mut s = lock(&slot.state);
        let mut cancelled = false;
        let mut cutoff = deadline;
        while s.phase != Phase::Done {
            let now = Instant::now();
            if now < cutoff {
                s = slot
                    .changed
                    .wait_timeout(s, cutoff - now)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
                continue;
            }
            if s.phase == Phase::Queued {
                // The worker that pops the stale entry recycles the slot.
                s.phase = Phase::Abandoned;
                return Err(err(FailureKind::Busy, "no worker free before the deadline"));
            }
            if cancelled {
                // Past the grace period: the call is still inside the module. Its
                // buffers belong to the worker until it returns.
                inner.failed.store(true, Ordering::Release);
                inner.quarantined.fetch_add(s.lent, Ordering::AcqRel);
                s.phase = Phase::Abandoned;
                return Err(err(
                    FailureKind::Limit,
                    "the call outlived its deadline and grace; the module is marked failed",
                ));
            }
            inner.host_state.slots[idx].cancel();
            cancelled = true;
            cutoff = now + inner.policy.grace;
        }
        let outcome = s.result.take();
        let result = match outcome {
            Some(Ok((len, retried))) => {
                out.clear();
                out.extend_from_slice(&s.out[..len]);
                if retried {
                    // A grown buffer is not kept: the retry path is rare and the
                    // retained size is bounded by the point's bound instead.
                    s.out = Vec::new();
                }
                Ok(())
            }
            Some(Err(fail)) => Err(inner
                .convert(fail, cancelled)
                .at(request.kind, request.point)),
            None => Err(err(FailureKind::Unavailable, "the call produced no result")),
        };
        s.phase = Phase::Idle;
        drop(s);
        lock(&inner.free).push(idx);
        result
    }

    /// Stops the pool and waits up to `wait` for every worker to exit. Returns
    /// whether all did; a hung call keeps its worker (and the module's code and
    /// shutdown) alive until it returns, which is the contract's "a node with a
    /// hung call exits without it".
    #[must_use]
    pub fn close(mut self, wait: Duration) -> bool {
        self.inner.shutdown.store(true, Ordering::Release);
        self.inner.work.notify_all();
        let deadline = Instant::now() + wait;
        let mut live = lock(&self.inner.live);
        while *live > 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            live = self
                .inner
                .exited
                .wait_timeout(live, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        drop(live);
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
        true
    }
}

impl Drop for NativeModule {
    fn drop(&mut self) {
        // Idle workers exit on their own; a hung one holds the module open.
        self.inner.shutdown.store(true, Ordering::Release);
        self.inner.work.notify_all();
    }
}

impl Inner {
    fn convert(&self, fail: Fail, cancelled: bool) -> CallError {
        let name = self.desc.name.as_str();
        match fail {
            Fail::Status(status::CANCELLED, _) if cancelled => CallError::new(
                name,
                FailureKind::Limit,
                "the call was cancelled at its deadline",
            ),
            Fail::Status(code, detail) => CallError::from_status(name, code).with_detail(&detail),
            Fail::Limit(why) => CallError::new(name, FailureKind::Limit, why),
            Fail::Malformed(why) => {
                self.strike();
                CallError::new(name, FailureKind::OutputMalformed, why)
            }
            Fail::Overrun => {
                self.failed.store(true, Ordering::Release);
                CallError::new(
                    name,
                    FailureKind::OutputMalformed,
                    "the module wrote past its output buffer; marked failed",
                )
            }
        }
    }

    fn strike(&self) {
        let now = Instant::now();
        let mut strikes = lock(&self.strikes);
        strikes.push_back(now);
        while strikes
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.policy.strike_window)
        {
            strikes.pop_front();
        }
        if strikes.len() >= self.policy.strike_limit {
            self.failed.store(true, Ordering::Release);
        }
    }

    fn release(&self, idx: usize) {
        lock(&self.free).push(idx);
    }

    fn run(&self, idx: usize) {
        let slot = &self.slots[idx];
        let (kind, point, timeout_ms, bound, limit, mut input, mut out) = {
            let mut s = lock(&slot.state);
            if s.phase == Phase::Abandoned {
                s.phase = Phase::Idle;
                drop(s);
                self.release(idx);
                return;
            }
            s.phase = Phase::Running;
            let (input, out) = (std::mem::take(&mut s.input), std::mem::take(&mut s.out));
            s.lent = (input.capacity() + out.capacity()) as u64;
            (
                s.kind,
                s.point,
                s.timeout_ms,
                s.bound as usize,
                s.limit as usize,
                input,
                out,
            )
        };
        let id = ((self.seq.fetch_add(1, Ordering::Relaxed) + 1) << SLOT_BITS) | idx as u64;
        self.host_state.slots[idx].begin(id);
        let result = self.execute(kind, point, &input, timeout_ms, id, bound, limit, &mut out);
        self.host_state.slots[idx].end();
        input.clear();
        let mut s = lock(&slot.state);
        (s.input, s.out) = (input, out);
        if s.phase == Phase::Abandoned {
            self.quarantined.fetch_sub(s.lent, Ordering::AcqRel);
            s.phase = Phase::Idle;
            drop(s);
            self.release(idx);
        } else {
            s.result = Some(result);
            s.phase = Phase::Done;
            slot.changed.notify_all();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn execute(
        &self,
        kind: u32,
        point: usize,
        input: &[u8],
        timeout_ms: u32,
        id: u64,
        bound: usize,
        limit: usize,
        out: &mut Vec<u8>,
    ) -> Result<(usize, bool), Fail> {
        let point = self.desc.extension_points[point].id.as_bytes();
        let mut cap = bound;
        for attempt in 0..2 {
            if out.len() < cap + GUARD {
                out.resize(cap + GUARD, GUARD_BYTE);
            }
            out[cap..cap + GUARD].fill(GUARD_BYTE);
            let (code, len) = self
                .bound
                .call(kind, point, input, timeout_ms, id, &mut out[..cap]);
            if out[cap..cap + GUARD].iter().any(|b| *b != GUARD_BYTE) {
                return Err(Fail::Overrun);
            }
            match code {
                status::OK if len as usize <= cap => return Ok((len as usize, attempt == 1)),
                status::OK => {
                    return Err(Fail::Malformed(
                        "the module reported more bytes than its buffer holds",
                    ));
                }
                status::BUFFER_TOO_SMALL => {
                    let need = usize::try_from(len).unwrap_or(usize::MAX);
                    if need <= cap {
                        return Err(Fail::Malformed(
                            "the module asked for a larger buffer than the one it had",
                        ));
                    }
                    if need > limit {
                        return Err(Fail::Limit(
                            "the result would exceed the point's output bound",
                        ));
                    }
                    if attempt == 1 {
                        return Err(Fail::Malformed("the module needed a larger buffer twice"));
                    }
                    cap = need;
                }
                c if status::is_defined(c) => {
                    let keep = (len as usize)
                        .min(cap)
                        .min(hsearch_module_abi::MAX_ERROR_DETAIL_BYTES);
                    let detail = if c == status::MODULE {
                        out[..keep].to_vec()
                    } else {
                        Vec::new()
                    };
                    return Err(Fail::Status(c, detail));
                }
                _ => return Err(Fail::Malformed("the module returned an undefined status")),
            }
        }
        Err(Fail::Malformed("the module needed a larger buffer twice"))
    }
}

fn worker(inner: &Arc<Inner>) {
    loop {
        let next = {
            let mut queue = lock(&inner.queue);
            loop {
                if let Some(idx) = queue.pop_front() {
                    break Some(idx);
                }
                if inner.shutdown.load(Ordering::Acquire) {
                    break None;
                }
                queue = inner
                    .work
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        match next {
            Some(idx) => inner.run(idx),
            None => break,
        }
    }
    *lock(&inner.live) -= 1;
    inner.exited.notify_all();
}

fn open_staged(staged: &image::Staged) -> Result<Library, LoadError> {
    Library::open(&staged.load_path()).map_err(map_open)
}

fn describe(bound: &Bound, config: &LoadConfig, reported: u32) -> Result<Descriptor, LoadError> {
    let cap = MAX_DESCRIPTOR_BYTES as usize;
    let mut buf = vec![GUARD_BYTE; cap + GUARD];
    let (code, len) = bound.describe(&mut buf[..cap]);
    if buf[cap..].iter().any(|b| *b != GUARD_BYTE) {
        return Err(LoadError::BadDescriptor(
            "the module wrote past the descriptor buffer",
        ));
    }
    if code != status::OK {
        return Err(LoadError::BadDescriptor("hs_module_describe failed"));
    }
    let len = usize::try_from(len)
        .ok()
        .filter(|l| *l <= cap)
        .ok_or(LoadError::BadDescriptor(
            "descriptor length exceeds its buffer",
        ))?;
    let desc = Descriptor::parse(&buf[..len]).map_err(|e| LoadError::BadDescriptor(e.0))?;
    if let Some(expected) = &config.expected_name
        && *expected != desc.name
    {
        return Err(LoadError::DescriptorMismatch(
            "name differs from the configured name",
        ));
    }
    if !desc.engine_api.accepted_by(config.host_engine_api) {
        return Err(LoadError::EngineApiRefused(desc.engine_api));
    }
    if desc.abi.packed() != reported {
        return Err(LoadError::DescriptorMismatch(
            "descriptor abi differs from hs_abi_version",
        ));
    }
    Ok(desc)
}
