//! A loaded WASM module: a bounded pool of instances, each called on the caller's thread.

use crate::host::{Breach, HostData, LogGate, linker};
use crate::load::{self, LoadError};
use hsearch_module_abi::{
    ABI_VERSION, CallError, Descriptor, ENGINE_API_VERSION, FailureKind, LogSink, MAX_BUFFER_BYTES,
    MAX_CONFIG_BYTES, MAX_DESCRIPTOR_BYTES, MAX_ERROR_DETAIL_BYTES, Request, Version, abi_accepted,
    status,
};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use wasmtime::{Config, Engine, Memory, Module, Store, StoreLimitsBuilder, Trap, TypedFunc};

/// Limits the host sets and the module cannot change. Defaults are **proposed**
/// and measured in the M24.03 evidence.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Largest linear memory, and the largest maximum a module may declare.
    pub max_memory_bytes: u64,
    /// Fuel each call may spend (about one unit per executed instruction).
    pub fuel_per_call: u64,
    /// Calls to `log` and `cancelled` one call may make.
    pub host_calls_per_call: u32,
    /// Largest module file read.
    pub max_wasm_bytes: u64,
    /// Most instances of one module, whatever the descriptor asks for.
    pub max_instances: u32,
    /// Largest I/O region a descriptor may ask for.
    pub max_io_bytes: u32,
    /// Resolution of the wall-clock deadline: how often the epoch advances while calls run.
    pub epoch_tick: Duration,
    /// Trapped or limit-breaching calls within `strike_window` that mark the module failed.
    pub strike_limit: usize,
    /// The strike window.
    pub strike_window: Duration,
    /// Module log lines passed to the sink per second.
    pub log_per_second: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 64 * 1024 * 1024,
            fuel_per_call: 50_000_000,
            host_calls_per_call: 256,
            max_wasm_bytes: 16 * 1024 * 1024,
            max_instances: 64,
            max_io_bytes: MAX_BUFFER_BYTES,
            epoch_tick: Duration::from_millis(1),
            strike_limit: 3,
            strike_window: Duration::from_secs(60),
            log_per_second: 50,
        }
    }
}

/// Everything the node's configuration says about one WASM module.
pub struct WasmConfig {
    /// The pinned SHA-256 of the module file.
    pub sha256: [u8; 32],
    /// The opaque module section handed to `hs_module_init`, at most 64 KiB.
    pub module_config: Vec<u8>,
    /// When set, the descriptor's name must equal it.
    pub expected_name: Option<String>,
    /// The ABI version this host implements.
    pub host_abi: u32,
    /// The engine API version this host implements.
    pub host_engine_api: Version,
    /// Host-side limits.
    pub limits: Limits,
    /// Where module log lines go; `None` drops them.
    pub log: Option<LogSink>,
}

impl WasmConfig {
    /// A configuration for a module pinned to `sha256`, with every default.
    #[must_use]
    pub fn new(sha256: [u8; 32]) -> Self {
        Self {
            sha256,
            module_config: Vec::new(),
            expected_name: None,
            host_abi: ABI_VERSION,
            host_engine_api: ENGINE_API_VERSION,
            limits: Limits::default(),
            log: None,
        }
    }
}

/// A module's state as capabilities report it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModuleState {
    /// Accepting calls.
    Ready,
    /// Marked failed by repeated traps or limit breaches; no new calls until restart.
    Failed,
}

struct Inst {
    store: Store<HostData>,
    instance: wasmtime::Instance,
    memory: Memory,
    call: TypedFunc<(i32, i32, i32, i32), i32>,
    out_len: TypedFunc<(), i32>,
    in_ptr: usize,
    out_ptr: usize,
}

struct Pool {
    idle: Vec<Inst>,
    live: usize,
    waiters: usize,
}

/// Wakes a thread that advances the engine's epoch while any call is running, and
/// parks it otherwise, so an idle module costs no wake-ups.
struct Ticker {
    running: Mutex<usize>,
    wake: Condvar,
    stop: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Inner {
    desc: Descriptor,
    engine: Engine,
    module: Module,
    linker: wasmtime::Linker<HostData>,
    gate: Arc<LogGate>,
    limits: Limits,
    pool: Mutex<Pool>,
    returned: Condvar,
    max_instances: usize,
    failed: AtomicBool,
    strikes: Mutex<VecDeque<Instant>>,
    ticker: Arc<Ticker>,
}

/// A loaded WASM module.
pub struct WasmModule {
    inner: Arc<Inner>,
}

impl fmt::Debug for WasmModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmModule")
            .field("name", &self.inner.desc.name)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl Drop for WasmModule {
    fn drop(&mut self) {
        self.inner.ticker.stop.store(true, Ordering::Release);
        let _guard = lock(&self.inner.ticker.running);
        self.inner.ticker.wake.notify_all();
    }
}

fn engine(limits: &Limits) -> Result<Engine, LoadError> {
    let mut config = Config::new();
    config
        .consume_fuel(true)
        .epoch_interruption(true)
        .wasm_multi_memory(false)
        .wasm_simd(false)
        .wasm_relaxed_simd(false);
    config.wasm_memory64(false);
    config.max_wasm_stack(512 * 1024);
    let _ = limits;
    Engine::new(&config).map_err(|_| LoadError::Invalid("the engine could not be configured"))
}

fn read_exact(inst: &mut Inst, ptr: usize, len: usize) -> Option<Vec<u8>> {
    let end = ptr.checked_add(len)?;
    inst.memory
        .data(&inst.store)
        .get(ptr..end)
        .map(<[u8]>::to_vec)
}

impl WasmModule {
    /// Loads a module from a file: hash, compile, check imports and exports, then
    /// version, describe and init on the first instance.
    ///
    /// # Errors
    /// [`LoadError`]; nothing of the module is registered.
    pub fn load(path: &Path, config: WasmConfig) -> Result<Self, LoadError> {
        let io = |_| LoadError::Io("cannot read the module file");
        let meta = std::fs::metadata(path).map_err(io)?;
        if !meta.is_file() {
            return Err(LoadError::Io("the module path is not a regular file"));
        }
        if meta.len() > config.limits.max_wasm_bytes {
            return Err(LoadError::TooLarge);
        }
        let bytes = std::fs::read(path).map_err(io)?;
        Self::load_bytes(&bytes, config)
    }

    /// Loads a module from bytes already in memory, with the same checks.
    ///
    /// # Errors
    /// [`LoadError`]; nothing of the module is registered.
    pub fn load_bytes(bytes: &[u8], config: WasmConfig) -> Result<Self, LoadError> {
        if bytes.len() as u64 > config.limits.max_wasm_bytes {
            return Err(LoadError::TooLarge);
        }
        if config.module_config.len() > MAX_CONFIG_BYTES as usize {
            return Err(LoadError::ConfigTooLarge);
        }
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        if digest != config.sha256 {
            return Err(LoadError::HashMismatch);
        }
        let engine = engine(&config.limits)?;
        let module = Module::new(&engine, bytes)
            .map_err(|_| LoadError::Invalid("not a valid core WebAssembly module"))?;
        load::check(&module, config.limits.max_memory_bytes)?;
        let linker = linker(&engine)
            .map_err(|_| LoadError::Invalid("the host interface could not be linked"))?;
        let gate = Arc::new(LogGate::new(
            config.log.clone(),
            config.limits.log_per_second,
        ));
        let ticker = Arc::new(Ticker {
            running: Mutex::new(0),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let placeholder = Descriptor {
            name: String::new(),
            version: String::new(),
            abi: Version { major: 0, minor: 0 },
            engine_api: Version { major: 0, minor: 0 },
            max_concurrent_calls: 1,
            io_bytes: 1,
            extension_points: Vec::new(),
        };
        let mut inner = Inner {
            desc: placeholder,
            engine,
            module,
            linker,
            gate,
            limits: config.limits.clone(),
            pool: Mutex::new(Pool {
                idle: Vec::new(),
                live: 0,
                waiters: 0,
            }),
            returned: Condvar::new(),
            max_instances: 1,
            failed: AtomicBool::new(false),
            strikes: Mutex::new(VecDeque::new()),
            ticker,
        };
        let mut first = inner.instantiate().map_err(|_| {
            LoadError::Trapped("the module could not be instantiated within its limits")
        })?;
        let desc = inner.bring_up(&mut first, &config)?;
        inner.gate.set_name(&desc.name);
        inner.max_instances = desc
            .max_concurrent_calls
            .min(config.limits.max_instances)
            .max(1) as usize;
        inner.desc = desc;
        {
            let mut pool = lock(&inner.pool);
            pool.live = 1;
            pool.idle.push(first);
        }
        let inner = Arc::new(inner);
        spawn_ticker(&inner);
        Ok(Self { inner })
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

    /// Instances currently alive, idle or running.
    #[must_use]
    pub fn live_instances(&self) -> usize {
        lock(&self.inner.pool).live
    }

    /// Runs one call and leaves its result in `out` (cleared first, empty on error).
    ///
    /// # Errors
    /// A typed [`CallError`] naming the module and point. A trap or limit breach
    /// fails this call, discards the instance and counts a strike; the module stays
    /// loaded unless the strike limit is reached.
    pub fn call(&self, request: &Request<'_>, out: &mut Vec<u8>) -> Result<(), CallError> {
        let inner = &*self.inner;
        out.clear();
        let name = inner.desc.name.as_str();
        let err =
            |failure, reason| CallError::new(name, failure, reason).at(request.kind, request.point);
        if inner.failed.load(Ordering::Acquire) {
            return Err(err(FailureKind::Unavailable, "the module is marked failed"));
        }
        if !inner
            .desc
            .extension_points
            .iter()
            .any(|p| p.kind == request.kind && p.id == request.point)
        {
            return Err(err(
                FailureKind::Unavailable,
                "the module does not declare that point",
            ));
        }
        let io = inner.desc.io_bytes as usize;
        if request.point.len() + request.input.len() > io {
            return Err(err(
                FailureKind::Limit,
                "point id and input over the module's I/O region",
            ));
        }
        if request.output_bound == 0
            || request.output_limit < request.output_bound
            || request.output_limit > MAX_BUFFER_BYTES
        {
            return Err(err(FailureKind::Limit, "invalid output bound"));
        }
        let Some(deadline) = Instant::now().checked_add(request.timeout) else {
            return Err(err(FailureKind::Limit, "the timeout is too large"));
        };
        let mut inst = inner.acquire(deadline).map_err(|(f, r)| err(f, r))?;
        let guard = TickGuard::new(&inner.ticker);
        let outcome = inner.run(&mut inst, request, deadline, out);
        drop(guard);
        match outcome {
            Ok(()) => {
                inner.give_back(inst);
                Ok(())
            }
            Err(Fault::Keep(e)) => {
                inner.give_back(inst);
                out.clear();
                Err(e.at(request.kind, request.point))
            }
            Err(Fault::Discard(e)) => {
                inner.discard();
                out.clear();
                inner.strike();
                Err(e.at(request.kind, request.point))
            }
        }
    }
}

/// How a call ended badly: the instance is fine (a status the module chose) or must go.
enum Fault {
    Keep(CallError),
    Discard(CallError),
}

struct TickGuard<'a>(&'a Ticker);

impl<'a> TickGuard<'a> {
    fn new(ticker: &'a Ticker) -> Self {
        *lock(&ticker.running) += 1;
        ticker.wake.notify_all();
        Self(ticker)
    }
}

impl Drop for TickGuard<'_> {
    fn drop(&mut self) {
        *lock(&self.0.running) -= 1;
    }
}

fn spawn_ticker(inner: &Arc<Inner>) {
    let (ticker, engine, tick) = (
        Arc::clone(&inner.ticker),
        inner.engine.clone(),
        inner.limits.epoch_tick,
    );
    let spawned = std::thread::Builder::new()
        .name(format!("hsearch-wasm-epoch-{}", inner.desc.name))
        .spawn(move || {
            loop {
                {
                    let mut running = lock(&ticker.running);
                    while *running == 0 && !ticker.stop.load(Ordering::Acquire) {
                        running = ticker
                            .wake
                            .wait(running)
                            .unwrap_or_else(PoisonError::into_inner);
                    }
                }
                if ticker.stop.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(tick);
                engine.increment_epoch();
            }
        });
    if spawned.is_err() {
        // Without the ticker the wall-clock deadline cannot fire; fuel still bounds CPU, but fail closed.
        inner.failed.store(true, Ordering::Release);
    }
}

impl Inner {
    fn instantiate(&self) -> Result<Inst, ()> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.max_memory_bytes as usize)
            .instances(1)
            .tables(4)
            .table_elements(100_000)
            .memories(1)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(
            &self.engine,
            HostData::new(
                limits,
                self.limits.host_calls_per_call,
                Arc::clone(&self.gate),
            ),
        );
        store.limiter(|d| &mut d.limits);
        store.set_fuel(self.limits.fuel_per_call).map_err(|_| ())?;
        store.set_epoch_deadline(u64::MAX / 4);
        let instance = self
            .linker
            .instantiate(&mut store, &self.module)
            .map_err(|_| ())?;
        let memory = instance.get_memory(&mut store, "memory").ok_or(())?;
        let call = instance
            .get_typed_func(&mut store, "hs_module_call")
            .map_err(|_| ())?;
        let out_len = instance
            .get_typed_func(&mut store, "hs_output_len")
            .map_err(|_| ())?;
        let in_ptr = instance
            .get_typed_func::<(), i32>(&mut store, "hs_input_region")
            .map_err(|_| ())?;
        let out_ptr = instance
            .get_typed_func::<(), i32>(&mut store, "hs_output_region")
            .map_err(|_| ())?;
        let (in_ptr, out_ptr) = (
            in_ptr.call(&mut store, ()).map_err(|_| ())?,
            out_ptr.call(&mut store, ()).map_err(|_| ())?,
        );
        let (in_ptr, out_ptr) = (
            usize::try_from(in_ptr).map_err(|_| ())?,
            usize::try_from(out_ptr).map_err(|_| ())?,
        );
        Ok(Inst {
            store,
            instance,
            memory,
            call,
            out_len,
            in_ptr,
            out_ptr,
        })
    }

    /// Version, describe, validate, init: the load-time conversation with a fresh instance.
    fn bring_up(&self, inst: &mut Inst, config: &WasmConfig) -> Result<Descriptor, LoadError> {
        let trap = |_| LoadError::Trapped("a load-time call trapped or ran out of fuel");
        let abi = {
            let version = instance_func::<(), i32>(inst, "hs_abi_version")?;
            inst.store
                .set_fuel(self.limits.fuel_per_call)
                .map_err(trap)?;
            version.call(&mut inst.store, ()).map_err(trap)?
        };
        let reported = u32::from_ne_bytes(abi.to_ne_bytes());
        if !abi_accepted(reported, config.host_abi) {
            return Err(LoadError::AbiRefused {
                module: reported,
                host: config.host_abi,
            });
        }
        // The descriptor is pure and its size bounds the regions, so it is read first.
        let describe = instance_func::<(), i32>(inst, "hs_module_describe")?;
        inst.store
            .set_fuel(self.limits.fuel_per_call)
            .map_err(trap)?;
        let code = describe.call(&mut inst.store, ()).map_err(trap)?;
        if code != status::OK {
            return Err(LoadError::BadDescriptor("hs_module_describe failed"));
        }
        inst.store.set_fuel(1_000).map_err(trap)?;
        let len = inst.out_len.call(&mut inst.store, ()).map_err(trap)?;
        let len = usize::try_from(len)
            .ok()
            .filter(|l| *l <= MAX_DESCRIPTOR_BYTES as usize)
            .ok_or(LoadError::BadDescriptor(
                "descriptor length is out of range",
            ))?;
        let bytes = read_exact(inst, inst.out_ptr, len).ok_or(LoadError::BadDescriptor(
            "descriptor lies outside the memory",
        ))?;
        let desc = Descriptor::parse(&bytes).map_err(|e| LoadError::BadDescriptor(e.0))?;
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
        if desc.io_bytes > self.limits.max_io_bytes {
            return Err(LoadError::DescriptorMismatch(
                "io_bytes is over the host limit",
            ));
        }
        let io = desc.io_bytes as usize;
        let memory = inst.memory.data_size(&inst.store);
        let (a, b) = (inst.in_ptr, inst.out_ptr);
        let fits = |p: usize| p.checked_add(io).is_some_and(|end| end <= memory);
        if !fits(a) || !fits(b) {
            return Err(LoadError::DescriptorMismatch(
                "an I/O region lies outside the memory",
            ));
        }
        if a < b + io && b < a + io {
            return Err(LoadError::DescriptorMismatch("the I/O regions overlap"));
        }
        if config.module_config.len() > io {
            return Err(LoadError::ConfigTooLarge);
        }
        inst.memory
            .write(&mut inst.store, a, &config.module_config)
            .map_err(|_| LoadError::ConfigTooLarge)?;
        let init = instance_func::<i32, i32>(inst, "hs_module_init")?;
        inst.store
            .set_fuel(self.limits.fuel_per_call)
            .map_err(trap)?;
        let code = init
            .call(
                &mut inst.store,
                i32::try_from(config.module_config.len()).unwrap_or(0),
            )
            .map_err(trap)?;
        if code != status::OK {
            return Err(LoadError::InitFailed(code));
        }
        Ok(desc)
    }

    /// Takes an idle instance, makes one if there is room, or waits for one until `deadline`.
    fn acquire(&self, deadline: Instant) -> Result<Inst, (FailureKind, &'static str)> {
        let mut pool = lock(&self.pool);
        loop {
            if let Some(inst) = pool.idle.pop() {
                return Ok(inst);
            }
            if pool.live < self.max_instances {
                pool.live += 1;
                drop(pool);
                return self.instantiate().map_err(|()| {
                    lock(&self.pool).live -= 1;
                    self.returned.notify_one();
                    (
                        FailureKind::Unavailable,
                        "an instance could not be created within the module's limits",
                    )
                });
            }
            if pool.waiters >= 2 * self.max_instances {
                return Err((FailureKind::Busy, "the module's queue is full"));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err((FailureKind::Busy, "no instance free before the deadline"));
            }
            pool.waiters += 1;
            pool = self
                .returned
                .wait_timeout(pool, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            pool.waiters -= 1;
        }
    }

    fn give_back(&self, inst: Inst) {
        lock(&self.pool).idle.push(inst);
        self.returned.notify_one();
    }

    fn discard(&self) {
        lock(&self.pool).live -= 1;
        self.returned.notify_one();
    }

    fn strike(&self) {
        let now = Instant::now();
        let mut strikes = lock(&self.strikes);
        strikes.push_back(now);
        while strikes
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.limits.strike_window)
        {
            strikes.pop_front();
        }
        if strikes.len() >= self.limits.strike_limit {
            self.failed.store(true, Ordering::Release);
        }
    }

    fn run(
        &self,
        inst: &mut Inst,
        request: &Request<'_>,
        deadline: Instant,
        out: &mut Vec<u8>,
    ) -> Result<(), Fault> {
        let name = self.desc.name.as_str();
        let fail = |failure, reason| Fault::Discard(CallError::new(name, failure, reason));
        let (plen, ilen) = (request.point.len(), request.input.len());
        // Point id then input, at the input region; lengths were checked against io_bytes.
        let (start, end) = (inst.in_ptr, inst.in_ptr + plen + ilen);
        let data = inst.memory.data_mut(&mut inst.store);
        let Some(region) = data.get_mut(start..end) else {
            return Err(fail(
                FailureKind::Unavailable,
                "the input region lies outside the memory",
            ));
        };
        region[..plen].copy_from_slice(request.point.as_bytes());
        region[plen..].copy_from_slice(request.input);
        let remaining = deadline.saturating_duration_since(Instant::now());
        let tick = self.limits.epoch_tick.as_nanos().max(1);
        let ticks = u64::try_from(remaining.as_nanos() / tick)
            .unwrap_or(u64::MAX / 4)
            .saturating_add(1);
        inst.store.data_mut().begin(deadline);
        let timeout_ms = i32::try_from(request.timeout.as_millis()).unwrap_or(i32::MAX);
        if inst.store.set_fuel(self.limits.fuel_per_call).is_err() {
            return Err(fail(FailureKind::Unavailable, "fuel could not be set"));
        }
        inst.store.set_epoch_deadline(ticks);
        let args = (
            i32::try_from(request.kind.code()).unwrap_or(0),
            plen as i32,
            ilen as i32,
            timeout_ms,
        );
        let code = match inst.call.call(&mut inst.store, args) {
            Ok(code) => code,
            Err(e) => return Err(Fault::Discard(self.classify(inst, &e))),
        };
        // Reading the length is a guest call too: fresh fuel, the same deadline.
        let _ = inst.store.set_fuel(10_000);
        let len = match inst.out_len.call(&mut inst.store, ()) {
            Ok(len) => len,
            Err(e) => return Err(Fault::Discard(self.classify(inst, &e))),
        };
        let io = self.desc.io_bytes as usize;
        let Some(len) = usize::try_from(len).ok().filter(|l| *l <= io) else {
            return Err(fail(
                FailureKind::OutputMalformed,
                "the module reported more output than its region holds",
            ));
        };
        let (o, memory) = (inst.out_ptr, inst.memory.data_size(&inst.store));
        let Some(bytes) = o
            .checked_add(len)
            .filter(|end| *end <= memory)
            .map(|end| &inst.memory.data(&inst.store)[o..end])
        else {
            return Err(fail(
                FailureKind::OutputMalformed,
                "the output region lies outside the memory",
            ));
        };
        match code {
            status::OK => {
                if len > request.output_limit as usize {
                    return Err(Fault::Keep(CallError::new(
                        name,
                        FailureKind::Limit,
                        "the result exceeds the point's output bound",
                    )));
                }
                out.extend_from_slice(bytes);
                Ok(())
            }
            status::BUFFER_TOO_SMALL => Err(Fault::Keep(CallError::new(
                name,
                FailureKind::Limit,
                "the result does not fit the module's output region",
            ))),
            c if status::is_defined(c) => {
                let detail = if c == status::MODULE {
                    &bytes[..bytes.len().min(MAX_ERROR_DETAIL_BYTES)]
                } else {
                    &[][..]
                };
                Err(Fault::Keep(
                    CallError::from_status(name, c).with_detail(detail),
                ))
            }
            _ => Err(fail(
                FailureKind::OutputMalformed,
                "the module returned an undefined status",
            )),
        }
    }

    /// Maps a trap, fuel or deadline stop, or a host-enforced breach, to a typed error.
    fn classify(&self, inst: &Inst, error: &wasmtime::Error) -> CallError {
        let name = self.desc.name.as_str();
        match inst.store.data().breach {
            Some(Breach::HostCalls) => {
                return CallError::new(
                    name,
                    FailureKind::Limit,
                    "the call exceeded its host-call budget",
                );
            }
            None => {}
        }
        match error.downcast_ref::<Trap>() {
            Some(Trap::OutOfFuel) => {
                CallError::new(name, FailureKind::Limit, "the call exhausted its fuel")
            }
            Some(Trap::Interrupt) => {
                CallError::new(name, FailureKind::Limit, "the call exceeded its deadline")
            }
            Some(Trap::MemoryOutOfBounds) => CallError::new(
                name,
                FailureKind::Trapped,
                "the module accessed memory out of bounds",
            ),
            Some(Trap::UnreachableCodeReached) => CallError::new(
                name,
                FailureKind::Trapped,
                "the module reached an unreachable instruction",
            ),
            Some(Trap::StackOverflow) => CallError::new(
                name,
                FailureKind::Trapped,
                "the module overflowed its stack",
            ),
            Some(_) => CallError::new(name, FailureKind::Trapped, "the module trapped"),
            None => CallError::new(
                name,
                FailureKind::Limit,
                "the call exceeded its memory limit",
            ),
        }
    }
}

fn instance_func<P, R>(inst: &mut Inst, name: &'static str) -> Result<TypedFunc<P, R>, LoadError>
where
    P: wasmtime::WasmParams,
    R: wasmtime::WasmResults,
{
    inst.instance
        .get_typed_func::<P, R>(&mut inst.store, name)
        .map_err(|_| LoadError::BadExport(name))
}
