//! Baseline for M02.03: which native files the pinned engine fetches, and when,
//! when it opens and queries remote-backed segments. At the pinned rev an open
//! file is a byte slice, so the trace is per file; byte ranges inside a file are
//! not observable until the upstream demand-filled input is adopted.
#[path = "support/remote_fixture.rs"]
mod fixture;

use fixture::{Scratch, schema, write_documents};
use hsearch_engine::{
    CachedDirectory, EngineDirectory, Query, RemoteError, RemoteFiles, Snapshot, Sort, Value,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

struct Traced {
    files: BTreeMap<String, Vec<u8>>,
    log: Mutex<Vec<String>>,
}
impl RemoteFiles for &Traced {
    fn fetch(&self, name: &str) -> Result<Vec<u8>, RemoteError> {
        self.log.lock().unwrap().push(name.to_owned());
        self.files.get(name).cloned().ok_or(RemoteError::NotFound)
    }
}

fn extension(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

#[test]
fn the_pinned_engine_fetches_every_file_whole_at_open_and_nothing_later() {
    let (_scratch, files) = write_documents();
    let total: usize = files.iter().map(|(_, b)| b.len()).sum();
    let store = Traced {
        files: files.into_iter().collect(),
        log: Mutex::default(),
    };
    let names: Vec<String> = store.files.keys().cloned().collect();
    let cache = Scratch::new("trace");
    let directory = EngineDirectory::new(
        CachedDirectory::new(&cache.0, &store, names.clone(), u64::MAX / 2).unwrap(),
    );
    let drain = |label: &str| -> Vec<String> {
        let fetched: Vec<String> = store.log.lock().unwrap().drain(..).collect();
        let bytes: usize = fetched.iter().map(|n| store.files[n].len()).sum();
        let kinds: BTreeSet<&str> = fetched.iter().map(|n| extension(n)).collect();
        eprintln!(
            "TRACE {label}: {} files, {bytes} bytes, kinds {kinds:?}",
            fetched.len()
        );
        fetched
    };
    eprintln!("TRACE manifest: {} files, {total} bytes", names.len());
    let snapshot = Snapshot::open_committed(&directory, schema()).unwrap();
    // Baseline to beat: every manifest file, once, whole, before the first query.
    let opened = drain("open");
    assert_eq!(opened.len(), names.len());
    assert_eq!(opened.iter().collect::<BTreeSet<_>>().len(), names.len());
    let term = Query::Term {
        field: "tag",
        value: Value::String("t1"),
    };
    let range = Query::Range {
        field: "n",
        lower: Value::Integer(5),
        upper: Value::Integer(17),
    };
    let by_n = [Sort::Field {
        name: "n",
        descending: false,
    }];
    snapshot.search(&term, &[], 10, &[]).unwrap();
    assert!(drain("term, no sort, no fields").is_empty());
    snapshot.search(&range, &[], 10, &[]).unwrap();
    assert!(drain("range, no sort, no fields").is_empty());
    snapshot.search(&Query::All, &by_n, 10, &[]).unwrap();
    assert!(drain("all, doc-values sort").is_empty());
    snapshot.search(&Query::All, &[], 10, &["n"]).unwrap();
    assert!(drain("all, stored-field projection").is_empty());
}
