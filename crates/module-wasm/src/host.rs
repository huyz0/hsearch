//! What the two imports reach, and the limiter, per instance.

use hsearch_module_abi::{LogRecord, LogSink};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use wasmtime::{Caller, Extern, Linker, StoreLimits};

/// Why a call was stopped by the host rather than by the module.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Breach {
    HostCalls,
}

/// Rate limit for module log lines, shared by every instance of a module.
pub(crate) struct LogGate {
    name: std::sync::OnceLock<String>,
    sink: Option<LogSink>,
    per_second: u32,
    window: Mutex<(Instant, u32)>,
}

impl LogGate {
    pub(crate) fn new(sink: Option<LogSink>, per_second: u32) -> Self {
        Self {
            name: std::sync::OnceLock::new(),
            sink,
            per_second,
            window: Mutex::new((Instant::now(), 0)),
        }
    }

    /// Names the module once its descriptor is known; lines logged during `describe` carry no name.
    pub(crate) fn set_name(&self, name: &str) {
        let _ = self.name.set(name.to_owned());
    }

    fn emit(&self, level: u32, message: &[u8]) {
        let Some(sink) = &self.sink else { return };
        {
            let mut window = self
                .window
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = Instant::now();
            if now.duration_since(window.0) >= Duration::from_secs(1) {
                *window = (now, 0);
            }
            if window.1 >= self.per_second {
                return;
            }
            window.1 += 1;
        }
        // Control characters would forge lines or inject escapes in a line-oriented sink.
        let text: String = String::from_utf8_lossy(message)
            .chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
        let level = level.clamp(1, 4);
        sink(&LogRecord {
            module: self.name.get().map_or("", String::as_str),
            call_id: 0,
            level,
            message: &text,
        });
    }
}

/// Per-store data: the limiter, the call's budget and deadline.
pub(crate) struct HostData {
    pub(crate) limits: StoreLimits,
    pub(crate) breach: Option<Breach>,
    host_calls: u32,
    budget: u32,
    deadline: Instant,
    gate: std::sync::Arc<LogGate>,
}

impl HostData {
    pub(crate) fn new(limits: StoreLimits, budget: u32, gate: std::sync::Arc<LogGate>) -> Self {
        Self {
            limits,
            breach: None,
            host_calls: 0,
            budget,
            deadline: Instant::now(),
            gate,
        }
    }

    /// Starts a call: resets the host-call count and sets the deadline `cancelled` reports.
    pub(crate) fn begin(&mut self, deadline: Instant) {
        self.host_calls = 0;
        self.breach = None;
        self.deadline = deadline;
    }

    fn count(&mut self) -> wasmtime::Result<()> {
        self.host_calls += 1;
        if self.host_calls > self.budget {
            self.breach = Some(Breach::HostCalls);
            return Err(wasmtime::Error::msg("host call budget exhausted"));
        }
        Ok(())
    }
}

/// Most log bytes read from a module per message.
const MAX_LOG_BYTES: usize = 1024;

/// A linker with exactly the two imports of `hsearch_host_v1` and nothing else.
pub(crate) fn linker(engine: &wasmtime::Engine) -> wasmtime::Result<Linker<HostData>> {
    let mut linker = Linker::new(engine);
    linker.func_wrap(
        "hsearch_host_v1",
        "log",
        |mut caller: Caller<'_, HostData>,
         level: i32,
         ptr: i32,
         len: i32|
         -> wasmtime::Result<()> {
            caller.data_mut().count()?;
            let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
                return Ok(());
            };
            // A bad pointer or length is the module's bug; it logs nothing rather than trapping.
            let (Ok(start), Ok(len)) = (usize::try_from(ptr), usize::try_from(len)) else {
                return Ok(());
            };
            let len = len.min(MAX_LOG_BYTES);
            let message = start
                .checked_add(len)
                .and_then(|end| memory.data(&caller).get(start..end))
                .map(<[u8]>::to_vec);
            if let Some(message) = message {
                caller
                    .data()
                    .gate
                    .emit(u32::try_from(level).unwrap_or(0), &message);
            }
            Ok(())
        },
    )?;
    linker.func_wrap(
        "hsearch_host_v1",
        "cancelled",
        |caller: Caller<'_, HostData>| -> wasmtime::Result<i32> {
            Ok(i32::from(Instant::now() >= caller.data().deadline))
        },
    )?;
    Ok(linker)
}
