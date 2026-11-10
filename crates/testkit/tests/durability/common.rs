//! Bounded research scratch and raw wire records, never a production persistence format.
use super::budget::{Budget, Charge};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const FILE_CAP: usize = 1024 * 1024;
const ARTIFACT_CAP: u64 = 32 * 1024 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

pub(super) fn sample_runtime(candidate: &str) -> Value {
    let run = std::env::var("NEXTEST_RUN_ID").ok();
    let strict = std::env::var("HSEARCH_DURABILITY_STRICT_SAMPLE").ok();
    if strict.is_none() {
        return json!({"strict":false,"nextest_run_id":run,"scope":"ordinary_execution_not_strict_matrix"});
    }
    assert_eq!(strict.as_deref(), Some("1"));
    const { assert!(cfg!(target_os = "linux") && cfg!(target_env = "gnu")) };
    let run = run.expect("Strict sample must be an actual nextest child");
    assert_eq!(run.len(), 36);
    assert!(run.split('-').map(str::len).eq([8, 4, 4, 4, 12]));
    assert!(
        run.bytes()
            .all(|byte| byte == b'-' || byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    let text = std::env::var("HSEARCH_DURABILITY_BINDING").unwrap();
    assert!(text.len() <= 4096);
    let mut binding: Value = serde_json::from_str(&text).unwrap();
    let keys = [
        "source_digest",
        "source_head",
        "lock_sha256",
        "fixture_sha256",
        "envelope_sha256",
        "image",
        "target",
        "profile",
        "candidate",
        "repetition",
        "execution",
        "nextest_run_id",
        "protoc_sha256",
    ];
    let object = binding.as_object().unwrap();
    assert_eq!(object.len(), keys.len());
    assert!(keys.iter().all(|key| object.contains_key(*key)));
    assert_eq!(binding["candidate"], candidate);
    assert_eq!(binding["execution"], "strict-nextest");
    assert!(binding["nextest_run_id"].is_null());
    assert!((1..=3).contains(&binding["repetition"].as_u64().unwrap()));
    assert_eq!(
        binding["profile"],
        if cfg!(debug_assertions) {
            "dev"
        } else {
            "release"
        }
    );
    assert_eq!(
        binding["target"],
        format!("{}-unknown-linux-gnu", std::env::consts::ARCH)
    );
    for key in [
        "source_digest",
        "source_head",
        "lock_sha256",
        "fixture_sha256",
        "envelope_sha256",
        "protoc_sha256",
    ] {
        let value = binding[key].as_str().unwrap();
        assert_eq!(value.len(), if key == "source_head" { 40 } else { 64 });
        assert!(
            value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }
    let image = binding["image"].as_str().unwrap();
    assert_eq!(image.len(), 71);
    assert!(image.starts_with("sha256:"));
    assert!(
        image[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    let mut caps = serde_json::Map::new();
    for name in ["memory.max", "memory.swap.max", "cpu.max", "pids.max"] {
        let mut raw = String::new();
        File::open(format!("/sys/fs/cgroup/{name}"))
            .unwrap()
            .take(4097)
            .read_to_string(&mut raw)
            .unwrap();
        assert!(raw.len() <= 4096);
        caps.insert(name.into(), json!(raw.trim()));
    }
    assert_eq!(caps["memory.max"], "4294967296");
    assert_eq!(caps["memory.swap.max"], "0");
    assert_eq!(caps["pids.max"], "256");
    let cpu = caps["cpu.max"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .map(|part| part.parse::<u64>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(cpu.len(), 2);
    assert!(cpu[1] > 0 && cpu[1].checked_mul(2) == Some(cpu[0]));
    let mut status = String::new();
    File::open("/proc/self/status")
        .unwrap()
        .take(65537)
        .read_to_string(&mut status)
        .unwrap();
    assert!(status.len() <= 65536);
    for key in ["Uid:", "Gid:"] {
        let row = status
            .lines()
            .find(|line| line.starts_with(key))
            .unwrap()
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>();
        assert_eq!(row, vec!["10001"; 4]);
    }
    binding["nextest_run_id"] = json!(run);
    json!({"strict":true,"binding":binding,"caps":caps,"uid":10001,"gid":10001,"architecture":std::env::consts::ARCH,
        "scope":"Actual native nextest child/kernel limits; controller/source/matrix checks remain required"})
}

pub(super) fn process_memory() -> Value {
    let mut text = String::new();
    File::open("/proc/self/status")
        .unwrap()
        .take(65537)
        .read_to_string(&mut text)
        .unwrap();
    assert!(text.len() <= 65536);
    let mut result = serde_json::Map::new();
    for (field, label) in [("VmRSS:", "rss_bytes"), ("VmHWM:", "high_water_bytes")] {
        let line = text.lines().find(|line| line.starts_with(field)).unwrap();
        let parts = line.split_whitespace().collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2], "kB");
        let bytes = parts[1].parse::<u64>().unwrap().checked_mul(1024).unwrap();
        result.insert(label.into(), json!(bytes));
    }
    Value::Object(result)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Operation {
    pub(super) id: String,
    pub(super) tenant: u64,
    pub(super) key: u64,
    pub(super) stream: u64,
    pub(super) payload: String,
}

impl Operation {
    pub(super) fn fixture(id: &str) -> Self {
        let mut payload = format!("id={id};tenant=0;key=0;stream=0;");
        assert!(payload.len() <= 1024);
        payload.extend(std::iter::repeat_n('#', 1024 - payload.len()));
        Self {
            id: id.into(),
            tenant: 0,
            key: 0,
            stream: 0,
            payload,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Batch {
    pub(super) operations: Vec<Operation>,
}

impl Batch {
    pub(super) fn one(id: &str) -> Self {
        Self {
            operations: vec![Operation::fixture(id)],
        }
    }
    pub(super) fn validate(&self) {
        assert!(!self.operations.is_empty() && self.operations.len() <= 16);
        let first = &self.operations[0];
        for operation in &self.operations {
            assert!(operation.id.len() <= 128 && !operation.id.is_empty());
            assert_eq!((operation.tenant, operation.key), (first.tenant, first.key));
            assert_eq!(operation.payload.len(), 1024);
        }
        assert!(serde_json::to_vec(self).unwrap().len() <= 32768);
    }
}

#[derive(Default, Clone, Debug, Serialize)]
pub(super) struct Counts {
    pub(super) attempts: u64,
    pub(super) serialized_bytes: u64,
    pub(super) dropped: u64,
    pub(super) file_write_bytes: u64,
    pub(super) file_write_calls: u64,
    pub(super) sync_calls: u64,
    pub(super) io_errors: u64,
    pub(super) write_reserved_bytes: u64,
    pub(super) sync_reserved_calls: u64,
}

struct Artifacts {
    event_file: File,
    event_index: usize,
    event_bytes: usize,
    wire_file: File,
    wire_index: usize,
    wire_bytes: usize,
    total: u64,
    sequence: u64,
    directory: PathBuf,
    budget: Option<Budget>,
}

impl Artifacts {
    fn new(directory: PathBuf, budget: Option<Budget>) -> Self {
        fs::create_dir(&directory).unwrap();
        if let Some(budget) = &budget {
            budget.artifact_file().unwrap();
            budget.artifact_file().unwrap();
        }
        let event_file = Self::file(&directory, "events", 0, "ndjson");
        let wire_file = Self::file(&directory, "wire", 0, "bin");
        Self {
            event_file,
            event_index: 0,
            event_bytes: 0,
            wire_file,
            wire_index: 0,
            wire_bytes: 0,
            total: 0,
            sequence: 0,
            directory,
            budget,
        }
    }
    fn file(directory: &Path, stem: &str, index: usize, extension: &str) -> File {
        assert!(index < 128);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(format!("{stem}-{index:03}.{extension}")))
            .unwrap()
    }
    fn event(&mut self, mut value: Value) {
        value["sequence"] = json!(self.sequence);
        self.sequence += 1;
        let mut raw = serde_json::to_vec(&value).unwrap();
        raw.push(b'\n');
        assert!(raw.len() <= 4096);
        if self.event_bytes + raw.len() > FILE_CAP {
            if let Some(budget) = &self.budget {
                budget.artifact_file().unwrap();
            }
            self.event_index += 1;
            self.event_file = Self::file(&self.directory, "events", self.event_index, "ndjson");
            self.event_bytes = 0;
        }
        self.total += raw.len() as u64;
        assert!(self.total <= ARTIFACT_CAP);
        if let Some(budget) = &self.budget {
            budget.artifact_bytes(raw.len()).unwrap();
        }
        self.event_file.write_all(&raw).unwrap();
        self.event_bytes += raw.len();
    }
    fn wire(&mut self, raw: &[u8]) -> Value {
        assert!(!raw.is_empty() && raw.len() <= FILE_CAP);
        if self.wire_bytes + raw.len() > FILE_CAP {
            if let Some(budget) = &self.budget {
                budget.artifact_file().unwrap();
            }
            self.wire_index += 1;
            self.wire_file = Self::file(&self.directory, "wire", self.wire_index, "bin");
            self.wire_bytes = 0;
        }
        let record = json!({"file": format!("wire-{:03}.bin", self.wire_index),
                            "offset": self.wire_bytes, "bytes": raw.len()});
        self.total += raw.len() as u64;
        assert!(self.total <= ARTIFACT_CAP);
        if let Some(budget) = &self.budget {
            budget.artifact_bytes(raw.len()).unwrap();
        }
        self.wire_file.write_all(raw).unwrap();
        self.wire_bytes += raw.len();
        record
    }
}

#[derive(Clone)]
pub(super) struct Context {
    pub(super) root: PathBuf,
    pub(super) counts: Arc<Mutex<Counts>>,
    artifacts: Arc<Mutex<Artifacts>>,
    started: Instant,
    budget: Option<Budget>,
    disks: Arc<Mutex<BTreeMap<String, CompletedDisk>>>,
}

impl Context {
    pub(super) fn new(candidate: &str) -> Self {
        Self::construct(candidate, None)
    }
    pub(super) fn for_sample(candidate: &str, budget: &Budget) -> Self {
        Self::construct(candidate, Some(budget.clone()))
    }
    fn construct(candidate: &str, budget: Option<Budget>) -> Self {
        const {
            assert!(
                cfg!(target_os = "linux"),
                "Research scratch is qualified only in native Linux"
            )
        };
        assert!(matches!(candidate, "raft-rs" | "openraft"));
        let target = std::env::var_os("HSEARCH_NATIVE_ARTIFACT_ROOT")
            .or_else(|| std::env::var_os("CARGO_TARGET_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        assert!(target.is_absolute());
        fs::create_dir_all(&target).unwrap();
        let root = target.canonicalize().unwrap().join(format!(
            "hsearch-durability-{candidate}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let artifacts = Arc::new(Mutex::new(Artifacts::new(
            root.join("artifacts"),
            budget.clone(),
        )));
        let this = Self {
            root,
            counts: Arc::new(Mutex::new(Counts::default())),
            artifacts,
            started: Instant::now(),
            budget,
            disks: Arc::new(Mutex::new(BTreeMap::new())),
        };
        this.event(json!({"kind":"begin", "candidate":candidate, "scope":"partial_adapter_probe_not_M02.10_acceptance","runtime":sample_runtime(candidate),
                          "architecture":std::env::consts::ARCH, "pid":std::process::id()}));
        this
    }
    pub(super) fn event(&self, mut value: Value) {
        value["at_us"] = json!(self.started.elapsed().as_micros());
        self.artifacts.lock().unwrap().event(value);
    }
    pub(super) fn relative_us(&self, instant: Instant) -> u64 {
        u64::try_from(instant.duration_since(self.started).as_micros()).unwrap()
    }
    pub(super) fn state_witness(&self, directory: &str, label: &str) {
        assert!((1..=4).any(|id| {
            [
                format!("node-{id}"),
                format!("node-{id}-log"),
                format!("node-{id}-machine"),
            ]
            .contains(&directory.to_owned())
        }));
        assert!(!label.is_empty() && label.len() <= 64 && label.is_ascii());
        let mut raw = vec![];
        File::open(self.root.join(directory).join("state.bin"))
            .unwrap()
            .take(FILE_CAP as u64 + 1)
            .read_to_end(&mut raw)
            .unwrap();
        assert!((16..=FILE_CAP).contains(&raw.len()));
        let mut artifacts = self.artifacts.lock().unwrap();
        let location = artifacts.wire(&raw);
        artifacts.event(
            json!({"kind":"state_witness","directory":directory,"label":label,"wire":location}),
        );
    }
    pub(super) fn wire(
        &self,
        source: u64,
        target: u64,
        kind: &str,
        raw: &[u8],
        dropped: bool,
    ) -> u64 {
        self.charge(Charge::Network {
            bytes: raw.len(),
            dropped,
        })
        .unwrap();
        let mut counts = self.counts.lock().unwrap();
        counts.attempts += 1;
        counts.serialized_bytes += raw.len() as u64;
        counts.dropped += u64::from(dropped);
        assert!(counts.attempts <= 4096 && counts.serialized_bytes <= 16 * 1024 * 1024);
        let edge = counts.attempts;
        drop(counts);
        let mut artifacts = self.artifacts.lock().unwrap();
        let location = artifacts.wire(raw);
        artifacts.event(
            json!({"kind":"wire", "edge":edge, "source":source, "target":target, "message":kind,
                               "dropped":dropped, "wire":location}),
        );
        edge
    }
    pub(super) fn deadline(&self) {
        if let Some(budget) = &self.budget {
            budget.check().unwrap();
        }
        assert!(
            self.started.elapsed().as_secs_f64() < 15.0,
            "Native history deadline"
        );
    }
    fn charge(&self, charge: Charge) -> io::Result<()> {
        if let Some(budget) = &self.budget {
            budget.charge(charge)?;
        }
        Ok(())
    }
    pub(super) fn finish(&self) -> Value {
        self.deadline();
        let mut result = json!({"counts":self.counts.lock().unwrap().clone(), "elapsed_us":self.started.elapsed().as_micros(),
                            "directory":self.root});
        self.event(json!({"kind":"end", "result":result}));
        result["artifact_bytes"] = json!(self.artifacts.lock().unwrap().total);
        result
    }
}

pub(super) struct Disk<T> {
    pub(super) state: T,
    path: PathBuf,
    context: Context,
    poisoned: bool,
    pub(super) fail_next: bool,
    incarnation: u64,
    transaction: Value,
}

#[derive(Default)]
struct CompletedDisk {
    incarnation: u64,
    attempted: u64,
    completed: u64,
    raw: Vec<u8>,
    token: Value,
}

// Research evidence only. Copy spans refer exclusively to this disk's last
// completed frame. Literal spans contain the actual new serialized bytes.
fn content_delta(previous: &[u8], next: &[u8]) -> io::Result<Vec<u8>> {
    const BLOCK: usize = 64;
    let mut blocks = HashMap::new();
    for (index, block) in previous.chunks_exact(BLOCK).enumerate() {
        blocks.entry(block).or_insert(index * BLOCK);
    }
    let mut delta = b"HSRCHW01".to_vec();
    delta.extend_from_slice(&(previous.len() as u32).to_le_bytes());
    delta.extend_from_slice(&(next.len() as u32).to_le_bytes());
    let mut cursor = 0;
    let mut records = 0;
    while cursor < next.len() {
        records += 1;
        if records > 4096 {
            return Err(io::Error::other("Research witness record bound"));
        }
        let matched = if cursor + BLOCK <= next.len() {
            blocks.get(&next[cursor..cursor + BLOCK]).copied()
        } else {
            None
        };
        if let Some(offset) = matched {
            let mut length = BLOCK;
            while cursor + length < next.len()
                && offset + length < previous.len()
                && next[cursor + length] == previous[offset + length]
            {
                length += 1;
            }
            delta.push(0);
            delta.extend_from_slice(&(offset as u32).to_le_bytes());
            delta.extend_from_slice(&(length as u32).to_le_bytes());
            cursor += length;
        } else {
            let start = cursor;
            cursor += 1;
            while cursor < next.len()
                && (cursor + BLOCK > next.len()
                    || !blocks.contains_key(&next[cursor..cursor + BLOCK]))
            {
                cursor += 1;
            }
            delta.push(1);
            delta.extend_from_slice(&((cursor - start) as u32).to_le_bytes());
            delta.extend_from_slice(&next[start..cursor]);
        }
        if delta.len() > FILE_CAP {
            return Err(io::Error::other("Research witness byte bound"));
        }
    }
    Ok(delta)
}

impl<T: Serialize + DeserializeOwned + Clone> Disk<T> {
    pub(super) fn create(path: PathBuf, state: T, context: &Context) -> io::Result<Self> {
        fs::create_dir(&path)?;
        let directory = path.file_name().unwrap().to_str().unwrap().to_owned();
        {
            let mut disks = context.disks.lock().unwrap();
            assert!(disks.len() < 12 && !disks.contains_key(&directory));
            disks.insert(
                directory.clone(),
                CompletedDisk {
                    incarnation: 1,
                    ..CompletedDisk::default()
                },
            );
        }
        context.event(
            json!({"kind":"disk_open","directory":directory,"incarnation":1,"predecessor":0}),
        );
        let mut this = Self {
            state: state.clone(),
            path,
            context: context.clone(),
            poisoned: false,
            fail_next: false,
            incarnation: 1,
            transaction: Value::Null,
        };
        this.commit(state)?;
        Ok(this)
    }
    pub(super) fn reopen(path: PathBuf, context: &Context) -> io::Result<Self> {
        let directory = path.file_name().unwrap().to_str().unwrap().to_owned();
        let raw = Self::read_frame(&path)?;
        let state = serde_json::from_slice(&raw[16..]).map_err(io::Error::other)?;
        let (incarnation, predecessor, delta, transaction) = {
            let mut disks = context.disks.lock().unwrap();
            let disk = disks
                .get_mut(&directory)
                .ok_or_else(|| io::Error::other("Unwitnessed research disk"))?;
            if raw != disk.raw || disk.incarnation >= 4096 {
                return Err(io::Error::other(
                    "Research reopen differs from completed frame",
                ));
            }
            disk.incarnation += 1;
            (
                disk.incarnation,
                disk.completed,
                content_delta(&disk.raw, &raw)?,
                disk.token.clone(),
            )
        };
        let mut artifacts = context.artifacts.lock().unwrap();
        let location = artifacts.wire(&delta);
        artifacts.event(
            json!({"kind":"disk_reopened","directory":directory,"incarnation":incarnation,
                              "predecessor":predecessor,"wire":location}),
        );
        drop(artifacts);
        Ok(Self {
            state,
            path,
            context: context.clone(),
            poisoned: false,
            fail_next: false,
            incarnation,
            transaction,
        })
    }
    fn read_frame(path: &Path) -> io::Result<Vec<u8>> {
        let mut raw = Vec::new();
        File::open(path.join("state.bin"))?
            .take(FILE_CAP as u64 + 1)
            .read_to_end(&mut raw)?;
        if raw.len() > FILE_CAP
            || raw.len() < 16
            || &raw[..8] != b"HSRCHD01"
            || u64::from_le_bytes(raw[8..16].try_into().unwrap()) as usize != raw.len() - 16
        {
            return Err(io::Error::other("Research state framing refused"));
        }
        Ok(raw)
    }
    pub(super) fn read(path: &Path) -> io::Result<T> {
        serde_json::from_slice(&Self::read_frame(path)?[16..]).map_err(io::Error::other)
    }
    pub(super) fn transaction(&self) -> Value {
        self.transaction.clone()
    }
    fn io_event(&self, mut value: Value) {
        value["transaction"] = self.transaction.clone();
        value["directory"] = json!(self.path.file_name().unwrap().to_str().unwrap());
        self.context.event(value);
    }
    pub(super) fn commit(&mut self, next: T) -> io::Result<()> {
        let result = self.write(&next);
        if result.is_err() {
            self.poisoned = true;
            self.context.counts.lock().unwrap().io_errors += 1;
            self.context.charge(Charge::IoError).unwrap();
            self.io_event(json!({"kind":"disk_error", "directory":self.path.file_name().unwrap().to_str().unwrap(),
                                      "poisoned_until_reload":true}));
        }
        if result.is_ok() {
            self.state = next;
        }
        result
    }
    fn write(&mut self, next: &T) -> io::Result<()> {
        let directory = self.path.file_name().unwrap().to_str().unwrap().to_owned();
        let previous = {
            let mut disks = self.context.disks.lock().unwrap();
            let disk = disks.get_mut(&directory).unwrap();
            if disk.incarnation != self.incarnation {
                return Err(io::Error::other("Stale research disk handle"));
            }
            disk.attempted += 1;
            assert!(disk.attempted <= 65536);
            self.transaction = json!({"disk":directory,"incarnation":self.incarnation,
                                      "id":disk.attempted,"predecessor":disk.completed});
            disk.raw.clone()
        };
        self.io_event(json!({"kind":"disk_transaction"}));
        if self.poisoned || self.fail_next {
            self.fail_next = false;
            return Err(io::Error::other("Poisoned/injected research IO failure"));
        }
        let raw = serde_json::to_vec(next)?;
        if raw.len() + 16 > FILE_CAP {
            return Err(io::Error::other("Research state byte bound"));
        }
        let mut frame = b"HSRCHD01".to_vec();
        frame.extend_from_slice(&(raw.len() as u64).to_le_bytes());
        frame.extend_from_slice(&raw);
        let delta = content_delta(&previous, &frame)?;
        self.context.charge(Charge::ReserveWrite(raw.len() + 16))?;
        {
            let mut counts = self.context.counts.lock().unwrap();
            if counts.write_reserved_bytes + raw.len() as u64 + 16 > 512 * 1024 * 1024
                || counts.sync_reserved_calls + 2 > 65536
            {
                return Err(io::Error::other("Research write/sync budget"));
            }
            counts.write_reserved_bytes += raw.len() as u64 + 16;
            counts.sync_reserved_calls += 2;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path.join("state.next"))?;
        let length = (raw.len() as u64).to_le_bytes();
        for bytes in [b"HSRCHD01".as_slice(), length.as_slice(), raw.as_slice()] {
            let mut offset = 0;
            while offset < bytes.len() {
                self.context.charge(Charge::WriteCall)?;
                {
                    let mut counts = self.context.counts.lock().unwrap();
                    if counts.file_write_calls >= 65536 {
                        return Err(io::Error::other("Research file write call budget"));
                    }
                    counts.file_write_calls += 1;
                }
                self.io_event(json!({"kind":"file_write_attempt"}));
                let written = file.write(&bytes[offset..])?;
                if written == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "Research write made no progress",
                    ));
                }
                self.context.counts.lock().unwrap().file_write_bytes += written as u64;
                self.context.charge(Charge::WriteBytes(written)).unwrap();
                self.io_event(json!({"kind":"file_write","bytes":written}));
                offset += written;
            }
        }
        self.context.charge(Charge::SyncCall)?;
        self.context.counts.lock().unwrap().sync_calls += 1;
        self.io_event(json!({"kind":"sync_attempt","target":"file"}));
        file.sync_all()?;
        self.io_event(json!({"kind":"sync_complete","target":"file"}));
        fs::rename(self.path.join("state.next"), self.path.join("state.bin"))?;
        self.io_event(json!({"kind":"rename_complete"}));
        let directory_file = File::open(&self.path)?;
        self.context.charge(Charge::SyncCall)?;
        self.context.counts.lock().unwrap().sync_calls += 1;
        self.io_event(json!({"kind":"sync_attempt","target":"directory"}));
        directory_file.sync_all()?;
        self.io_event(json!({"kind":"sync_complete","target":"directory"}));
        // Derive the application witness from the exact serialized bytes that
        // were written and synced, rather than an applier's later declaration.
        let encoded: Value = serde_json::from_slice(&raw).map_err(io::Error::other)?;
        let application = if let Some(applied) = encoded.get("application") {
            let operations = applied["operations"].as_object().unwrap();
            assert!(operations.len() <= 128);
            json!({"candidate":"raft-rs","index":applied["applied"],"ids":operations.keys().collect::<Vec<_>>()})
        } else if let Some(applied) = encoded.get("applied") {
            let operations = applied["operations"].as_object().unwrap();
            assert!(operations.len() <= 128);
            json!({"candidate":"openraft","log_id":applied["last"],"ids":operations.keys().collect::<Vec<_>>()})
        } else {
            Value::Null
        };
        let mut artifacts = self.context.artifacts.lock().unwrap();
        let location = artifacts.wire(&delta);
        artifacts.event(json!({"kind":"disk_persisted", "directory":directory,"transaction":self.transaction,
                              "wire":location,"bytes":frame.len(), "file_sync":true, "directory_sync":true,"application":application}));
        drop(artifacts);
        let mut disks = self.context.disks.lock().unwrap();
        let disk = disks.get_mut(&directory).unwrap();
        disk.completed = disk.attempted;
        disk.raw = frame;
        disk.token = self.transaction.clone();
        Ok(())
    }
}
