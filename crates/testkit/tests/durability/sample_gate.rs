//! Resource admission for complete samples sharing an ordinary test target.
//! Strict qualification already supplies exactly one sample per fresh worker.
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn acquire(file: &File, deadline: Duration) -> io::Result<()> {
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(TryLockError::Error(error)) => return Err(error),
            Err(TryLockError::WouldBlock) if started.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Native sample admission",
                ));
            }
        }
    }
}

fn open(root: &Path) -> File {
    assert!(root.is_dir() && !root.is_symlink());
    let path = root.join("hsearch-durability-sample.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    let handle = file.metadata().unwrap();
    let named = fs::symlink_metadata(path).unwrap();
    assert!(handle.is_file() && named.is_file());
    assert_eq!((handle.dev(), handle.ino()), (named.dev(), named.ino()));
    assert_eq!(handle.nlink(), 1);
    assert_eq!(handle.len(), 0);
    file
}

pub(super) fn enter(candidate: &str) -> File {
    let root = std::env::var_os("HSEARCH_NATIVE_ARTIFACT_ROOT")
        .or_else(|| std::env::var_os("CARGO_TARGET_DIR"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf()
        });
    assert!(root.is_absolute());
    fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let file = open(&root);
    let started = Instant::now();
    // Setup wait remains inside the unchanged 120s nextest test deadline.
    // The 90s sample and 15s histories start only once resources are admitted.
    acquire(&file, Duration::from_secs(90)).unwrap();
    println!(
        "NATIVE_DURABILITY_ADMISSION: {}",
        serde_json::json!({"candidate":candidate,"wait_us":started.elapsed().as_micros(),
            "scope":"ordinary-target setup admission; not sample runtime or comparative evidence"})
    );
    file
}

pub(super) fn controls() {
    let root = std::env::temp_dir().join(format!(
        "hsearch-sample-gate-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let first = open(&root);
    let second = open(&root);
    acquire(&first, Duration::ZERO).unwrap();
    assert_eq!(
        acquire(&second, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    drop(first);
    acquire(&second, Duration::ZERO).unwrap();
    drop(second);
    let unwind = std::panic::catch_unwind(|| {
        let held = open(&root);
        acquire(&held, Duration::ZERO).unwrap();
        panic!("intentional admission unwind control");
    });
    assert!(unwind.is_err());
    let after_unwind = open(&root);
    acquire(&after_unwind, Duration::ZERO).unwrap();
    drop(after_unwind);
    fs::remove_file(root.join("hsearch-durability-sample.lock")).unwrap();
    fs::remove_dir(root).unwrap();
}
