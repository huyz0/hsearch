//! Shared sample/scale accounting; cloning a scope never resets its counters.
use super::common::{Context, Counts, Disk};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(super) enum Charge {
    Network { bytes: usize, dropped: bool },
    ReserveWrite(usize),
    WriteCall,
    WriteBytes(usize),
    SyncCall,
    IoError,
}

struct State {
    counts: Counts,
    artifact_bytes: u64,
    artifact_files: u64,
}

#[derive(Clone)]
pub(super) struct Budget {
    state: Arc<Mutex<State>>,
    started: Instant,
    seconds: u64,
    attempts: u64,
    serialized_bytes: u64,
    parent: Option<Box<Budget>>,
}

impl Budget {
    pub(super) fn sample() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                counts: Counts::default(),
                artifact_bytes: 0,
                artifact_files: 0,
            })),
            started: Instant::now(),
            seconds: 90,
            attempts: 32768,
            serialized_bytes: 64 * 1024 * 1024,
            parent: None,
        }
    }
    pub(super) fn scale(parent: &Self) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                counts: Counts::default(),
                artifact_bytes: 0,
                artifact_files: 0,
            })),
            started: Instant::now(),
            seconds: 15,
            attempts: 4096,
            serialized_bytes: 16 * 1024 * 1024,
            parent: Some(Box::new(parent.clone())),
        }
    }
    pub(super) fn check(&self) -> io::Result<()> {
        if self.started.elapsed() >= Duration::from_secs(self.seconds) {
            return Err(io::Error::other("Aggregate native scope deadline"));
        }
        if let Some(parent) = &self.parent {
            parent.check()?;
        }
        Ok(())
    }
    pub(super) fn charge(&self, charge: Charge) -> io::Result<()> {
        if !matches!(charge, Charge::WriteBytes(_) | Charge::IoError) {
            self.check()?;
        }
        if let Some(parent) = &self.parent {
            parent.charge(charge)?;
        }
        let mut state = self.state.lock().unwrap();
        let counts = &mut state.counts;
        match charge {
            Charge::Network { bytes, dropped } => {
                if counts.attempts >= self.attempts
                    || bytes > 1048576
                    || counts.serialized_bytes + bytes as u64 > self.serialized_bytes
                {
                    return Err(io::Error::other("Aggregate native transport budget"));
                }
                counts.attempts += 1;
                counts.serialized_bytes += bytes as u64;
                counts.dropped += u64::from(dropped);
            }
            Charge::ReserveWrite(bytes) => {
                if counts.write_reserved_bytes + bytes as u64 > 512 * 1024 * 1024
                    || counts.sync_reserved_calls + 2 > 65536
                {
                    return Err(io::Error::other(
                        "Aggregate native write/sync reservation budget",
                    ));
                }
                counts.write_reserved_bytes += bytes as u64;
                counts.sync_reserved_calls += 2;
            }
            Charge::WriteCall => {
                if counts.file_write_calls >= 65536 {
                    return Err(io::Error::other("Aggregate native write-call budget"));
                }
                counts.file_write_calls += 1;
            }
            Charge::WriteBytes(bytes) => {
                counts.file_write_bytes += bytes as u64;
                assert!(counts.file_write_bytes <= counts.write_reserved_bytes);
            }
            Charge::SyncCall => {
                if counts.sync_calls >= counts.sync_reserved_calls {
                    return Err(io::Error::other("Unreserved aggregate native sync"));
                }
                counts.sync_calls += 1;
            }
            Charge::IoError => counts.io_errors += 1,
        }
        Ok(())
    }
    pub(super) fn artifact_bytes(&self, bytes: usize) -> io::Result<()> {
        self.check()?;
        if let Some(parent) = &self.parent {
            parent.artifact_bytes(bytes)?;
        }
        let mut state = self.state.lock().unwrap();
        if state.artifact_bytes + bytes as u64 > 32 * 1024 * 1024 {
            return Err(io::Error::other("Aggregate native artifact byte budget"));
        }
        state.artifact_bytes += bytes as u64;
        Ok(())
    }
    pub(super) fn artifact_file(&self) -> io::Result<()> {
        self.check()?;
        if let Some(parent) = &self.parent {
            parent.artifact_file()?;
        }
        let mut state = self.state.lock().unwrap();
        if state.artifact_files >= 256 {
            return Err(io::Error::other("Aggregate native artifact file budget"));
        }
        state.artifact_files += 1;
        Ok(())
    }
    pub(super) fn counts(&self) -> Counts {
        self.state.lock().unwrap().counts.clone()
    }
    pub(super) fn elapsed_us(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_micros()).unwrap()
    }
}

pub(super) fn controls() {
    let sample = Budget::sample();
    let scale = Budget::scale(&sample);
    let other_group = scale.clone();
    for _ in 0..4096 {
        scale
            .charge(Charge::Network {
                bytes: 1,
                dropped: false,
            })
            .unwrap();
    }
    assert!(
        other_group
            .charge(Charge::Network {
                bytes: 1,
                dropped: false
            })
            .is_err()
    );
    assert_eq!(scale.counts().attempts, 4096);
    assert_eq!(sample.counts().attempts, 4097);
    // A refused child attempt remains conservatively charged to its parent.
    let sample = Budget::sample();
    let threads = (0..4)
        .map(|_| {
            let sample = sample.clone();
            std::thread::spawn(move || {
                sample
                    .charge(Charge::ReserveWrite(128 * 1024 * 1024))
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(sample.counts().write_reserved_bytes, 512 * 1024 * 1024);
    assert!(sample.charge(Charge::ReserveWrite(1)).is_err());
    sample.charge(Charge::WriteCall).unwrap();
    sample.charge(Charge::WriteBytes(1)).unwrap();
    sample.charge(Charge::SyncCall).unwrap();
    sample.charge(Charge::IoError).unwrap();
    assert_eq!(sample.counts().file_write_calls, 1);
    assert_eq!(sample.counts().io_errors, 1);
    let sample = Budget::sample();
    for _ in 0..256 {
        sample.artifact_file().unwrap();
    }
    assert!(sample.artifact_file().is_err());
    sample.artifact_bytes(32 * 1024 * 1024).unwrap();
    assert!(sample.artifact_bytes(1).is_err());
    // Real scratch writes in two distinct history contexts share one sample total.
    let sample = Budget::sample();
    let first = Context::for_sample("raft-rs", &sample);
    let second = Context::for_sample("raft-rs", &sample);
    Disk::create(first.root.join("node-1"), vec![0u8], &first).unwrap();
    Disk::create(second.root.join("node-1"), vec![0u8], &second).unwrap();
    let actual = sample.counts();
    assert_eq!(
        actual.file_write_bytes,
        first.counts.lock().unwrap().file_write_bytes
            + second.counts.lock().unwrap().file_write_bytes
    );
    assert_eq!(actual.file_write_calls, 6);
    assert_eq!(actual.sync_calls, 4);
    first.finish();
    second.finish();
}
