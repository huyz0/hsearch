//! Observe the actual acquired native lock without changing its backend.
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use lucene_store::{Directory, FsIndexOutput, Input, Lock};

use crate::{Error, ReleaseStatus, Result};

/// Caller-owned directory whose lifetime includes every borrowed engine.
/// Wrap an existing backend, reference, or Arc; its files and lock factory are
/// preserved. Only the engine may mutate the backend while it is open.
pub struct EngineDirectory<D: Directory> {
    pub(crate) observed: ObservedDirectory<D>,
}

impl<D: Directory> EngineDirectory<D> {
    /// Retain the exact existing backend and a single bounded writer slot.
    pub fn new(inner: D) -> Self {
        Self {
            observed: ObservedDirectory {
                inner,
                slot: Mutex::new(None),
            },
        }
    }

    /// Access the original backend. Mutating it while an engine is open violates
    /// the adapter's exclusive mutation contract, even if a backend allows it.
    pub fn inner(&self) -> &D {
        &self.observed.inner
    }

    pub(crate) fn begin(&self) -> Result<DirectoryAttempt<'_>> {
        let mut slot = self.observed.slot.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => Error::Busy,
            std::sync::TryLockError::Poisoned(_) => Error::Poisoned,
        })?;
        if slot.is_some() {
            return Err(Error::Busy);
        }
        let observation = Arc::new(Observation::new());
        *slot = Some(Arc::clone(&observation));
        Ok(DirectoryAttempt {
            slot: &self.observed.slot,
            observation,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Waiting,
    Acquiring,
    Refused,
    Held,
    Releasing,
    Acked,
    Failed,
}

#[derive(Debug)]
struct Observation {
    phase: Mutex<Phase>,
    invalid: AtomicBool,
}
impl Observation {
    fn new() -> Self {
        Self {
            phase: Mutex::new(Phase::Waiting),
            invalid: AtomicBool::new(false),
        }
    }
    fn status(&self) -> ReleaseStatus {
        if self.invalid.load(Ordering::Acquire) {
            return ReleaseStatus::Unobserved;
        }
        match self.phase.lock() {
            Ok(phase) => match *phase {
                Phase::Acked => ReleaseStatus::Acknowledged,
                Phase::Failed => ReleaseStatus::Failed,
                _ => ReleaseStatus::Unobserved,
            },
            Err(_) => ReleaseStatus::Unobserved,
        }
    }
    fn may_have_acquired(&self) -> bool {
        self.invalid.load(Ordering::Acquire)
            || self.phase.lock().map_or(true, |phase| {
                !matches!(*phase, Phase::Waiting | Phase::Refused)
            })
    }
}

/// Owns only the admission/observation slot, never the native lock. Engine and
/// Closing declare it after the lineage lease, which follows native teardown.
pub(crate) struct DirectoryAttempt<'d> {
    slot: &'d Mutex<Option<Arc<Observation>>>,
    observation: Arc<Observation>,
}
impl DirectoryAttempt<'_> {
    fn bound(&self) -> bool {
        self.slot.lock().is_ok_and(|slot| {
            slot.as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.observation))
        })
    }
    pub(crate) fn acquired(&self) -> bool {
        self.bound()
            && !self.observation.invalid.load(Ordering::Acquire)
            && self
                .observation
                .phase
                .lock()
                .is_ok_and(|phase| *phase == Phase::Held)
    }
    pub(crate) fn may_have_acquired(&self) -> bool {
        !self.bound() || self.observation.may_have_acquired()
    }
    pub(crate) fn status(&self) -> ReleaseStatus {
        if self.bound() {
            self.observation.status()
        } else {
            ReleaseStatus::Unobserved
        }
    }
}
impl Drop for DirectoryAttempt<'_> {
    fn drop(&mut self) {
        match self.slot.lock() {
            Ok(mut slot)
                if slot
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &self.observation)) =>
            {
                *slot = None;
            }
            _ => {
                self.observation.invalid.store(true, Ordering::Release);
            }
        }
    }
}

fn lock_error() -> lucene_store::Error {
    std::io::Error::other("engine lock acknowledgement unavailable").into()
}

struct ObservedLock {
    inner: Box<dyn Lock>,
    observation: Arc<Observation>,
    started: AtomicBool,
}
impl std::fmt::Debug for ObservedLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ObservedLock")
    }
}
impl ObservedLock {
    fn release(&self) -> lucene_store::Result<()> {
        if self.started.swap(true, Ordering::AcqRel) {
            return if self.observation.status() == ReleaseStatus::Acknowledged {
                Ok(())
            } else {
                Err(lock_error())
            };
        }
        let recording = match self.observation.phase.lock() {
            Ok(mut phase) if *phase == Phase::Held => {
                *phase = Phase::Releasing;
                true
            }
            _ => {
                self.observation.invalid.store(true, Ordering::Release);
                false
            }
        };
        // No observation or directory mutex is held across backend code. Even
        // poisoned state must not prevent the one explicit release attempt.
        let result = self.inner.close();
        match self.observation.phase.lock() {
            Ok(mut phase) if recording && *phase == Phase::Releasing => {
                *phase = if result.is_ok() {
                    Phase::Acked
                } else {
                    Phase::Failed
                };
            }
            _ => {
                self.observation.invalid.store(true, Ordering::Release);
            }
        }
        result
    }
}
impl Lock for ObservedLock {
    fn close(&self) -> lucene_store::Result<()> {
        self.release()
    }
    fn ensure_valid(&self) -> lucene_store::Result<()> {
        if self.observation.invalid.load(Ordering::Acquire)
            || !self
                .observation
                .phase
                .lock()
                .is_ok_and(|phase| *phase == Phase::Held)
        {
            return Err(lock_error());
        }
        self.inner.ensure_valid()
    }
}
impl Drop for ObservedLock {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

// Private so callers cannot acquire a foreign native writer through an armed
// observer. The public wrapper exposes the original backend explicitly.
pub(crate) struct ObservedDirectory<D: Directory> {
    inner: D,
    slot: Mutex<Option<Arc<Observation>>>,
}

impl<D: Directory> Directory for ObservedDirectory<D> {
    fn list_all(&self) -> lucene_store::Result<Vec<String>> {
        self.inner.list_all()
    }
    fn open(&self, name: &str) -> lucene_store::Result<Input> {
        self.inner.open(name)
    }
    fn file_length(&self, name: &str) -> lucene_store::Result<u64> {
        self.inner.file_length(name)
    }
    fn create_output(&self, name: &str) -> lucene_store::Result<FsIndexOutput> {
        self.inner.create_output(name)
    }
    fn create_output_with_estimate(
        &self,
        name: &str,
        estimated_bytes: Option<u64>,
    ) -> lucene_store::Result<FsIndexOutput> {
        self.inner
            .create_output_with_estimate(name, estimated_bytes)
    }
    fn create_temp_output(
        &self,
        prefix: &str,
        suffix: &str,
    ) -> lucene_store::Result<FsIndexOutput> {
        self.inner.create_temp_output(prefix, suffix)
    }
    fn create_temp_output_with_estimate(
        &self,
        prefix: &str,
        suffix: &str,
        estimated_bytes: Option<u64>,
    ) -> lucene_store::Result<FsIndexOutput> {
        self.inner
            .create_temp_output_with_estimate(prefix, suffix, estimated_bytes)
    }
    fn sync(&self, names: &[String]) -> lucene_store::Result<()> {
        self.inner.sync(names)
    }
    fn rename(&self, source: &str, dest: &str) -> lucene_store::Result<()> {
        self.inner.rename(source, dest)
    }
    fn delete_file(&self, name: &str) -> lucene_store::Result<()> {
        self.inner.delete_file(name)
    }
    fn sync_meta_data(&self) -> lucene_store::Result<()> {
        self.inner.sync_meta_data()
    }
    fn pending_deletions(&self) -> lucene_store::Result<BTreeSet<String>> {
        self.inner.pending_deletions()
    }
    fn fs_directory_path(&self) -> Option<&Path> {
        self.inner.fs_directory_path()
    }
    fn copy_from(&self, from: &dyn Directory, src: &str, dest: &str) -> lucene_store::Result<()> {
        self.inner.copy_from(from, src, dest)
    }
    fn obtain_lock(&self, name: &str) -> lucene_store::Result<Box<dyn Lock>> {
        if name != "write.lock" {
            return self.inner.obtain_lock(name);
        }
        let observation = self
            .slot
            .lock()
            .map_err(|_| lock_error())?
            .as_ref()
            .cloned()
            .ok_or_else(lock_error)?;
        {
            let mut phase = observation.phase.lock().map_err(|_| lock_error())?;
            if *phase != Phase::Waiting {
                observation.invalid.store(true, Ordering::Release);
                return Err(lock_error());
            }
            *phase = Phase::Acquiring;
        }
        match self.inner.obtain_lock(name) {
            Ok(inner) => {
                let lock = ObservedLock {
                    inner,
                    observation,
                    started: AtomicBool::new(false),
                };
                {
                    let mut phase = lock.observation.phase.lock().map_err(|_| lock_error())?;
                    if *phase != Phase::Acquiring
                        || lock.observation.invalid.load(Ordering::Acquire)
                    {
                        lock.observation.invalid.store(true, Ordering::Release);
                        return Err(lock_error());
                    }
                    *phase = Phase::Held;
                }
                Ok(Box::new(lock))
            }
            Err(error) => {
                if let Ok(mut phase) = observation.phase.lock() {
                    *phase = Phase::Refused;
                }
                Err(error)
            }
        }
    }
}

#[cfg(test)]
#[path = "directory_tests.rs"]
mod tests;
