//! Native commit identity and bounded in-process checkpoint lineage.
use std::sync::{Arc, Mutex, atomic::AtomicUsize};

use lucene_index::segment_infos::{self, SegmentInfos};

use crate::{Directory, Error, Result};

/// Existing native commit header identity, not a hsearch persisted format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeCommit {
    id: [u8; 16],
    generation: i64,
    version: i64,
}
impl NativeCommit {
    pub(crate) fn from_infos(infos: &SegmentInfos) -> Self {
        Self {
            id: infos.id,
            generation: infos.generation,
            version: infos.version,
        }
    }
    pub(crate) fn read(directory: &dyn Directory) -> Result<Self> {
        segment_infos::read_latest(directory)
            .map(|infos| Self::from_infos(&infos))
            .map_err(|_| Error::Native)
    }
}

#[derive(Default)]
pub(crate) struct Head {
    pub(crate) epoch: u64,
    pub(crate) native: Option<NativeCommit>,
    pub(crate) writer_active: bool,
    pub(crate) uncertain: bool,
}

/// Constant-size shared head. Checkpoint clones cannot restore an older epoch.
#[derive(Default)]
pub(crate) struct Lineage {
    pub(crate) head: Mutex<Head>,
    pub(crate) readers: Arc<AtomicUsize>,
}

/// The owning Engine declares its native writer before this lease, so an
/// implicit Engine drop releases the native writer before releasing admission.
pub(crate) struct WriterLease {
    pub(crate) lineage: Arc<Lineage>,
    finished: bool,
}
impl WriterLease {
    pub(crate) fn new(lineage: Arc<Lineage>) -> Self {
        Self {
            lineage,
            finished: false,
        }
    }
    pub(crate) fn invalidate(&self) {
        let mut head = self
            .lineage
            .head
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        head.uncertain = true;
    }
    pub(crate) fn finish(&mut self, native_close_succeeded: bool) -> Result<()> {
        let mut head = self.lineage.head.lock().map_err(|_| Error::Poisoned)?;
        head.uncertain |= !native_close_succeeded;
        head.writer_active = false;
        self.finished = true;
        Ok(())
    }
}
impl Drop for WriterLease {
    fn drop(&mut self) {
        if !self.finished {
            let mut head = self
                .lineage
                .head
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            // Unobserved rollback or close is not proof of a reusable lineage.
            head.uncertain = true;
            head.writer_active = false;
        }
    }
}
