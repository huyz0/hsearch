//! The scratch local engine and the read-only remote snapshot opened from its files.
use hsearch_engine::{
    Error, Kind, Mutation, Query, RemoteError, RemoteFiles, Schema, Snapshot, Value,
    with_local_engine,
};
use std::collections::BTreeMap;

struct Files(BTreeMap<String, Vec<u8>>);
impl RemoteFiles for Files {
    fn fetch(&self, name: &str) -> Result<Vec<u8>, RemoteError> {
        self.0.get(name).cloned().ok_or(RemoteError::NotFound)
    }
}

fn schema() -> Schema {
    Schema::new(1, &[("tag", Kind::Keyword)]).unwrap()
}

fn root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hsearch-local-{label}-{}", std::process::id()))
}

#[test]
fn committed_local_files_open_as_a_remote_snapshot() {
    let dir = root("files");
    let schema = schema();
    let files = with_local_engine(&dir, schema.clone(), |engine, local| {
        let fields = [("tag", Value::String("x"))];
        engine
            .apply(
                schema
                    .prepare(
                        "r",
                        &[
                            Mutation::Put {
                                id: "a",
                                version: 1,
                                fields: &fields,
                            },
                            Mutation::Put {
                                id: "b",
                                version: 2,
                                fields: &fields,
                            },
                        ],
                    )
                    .unwrap(),
            )
            .unwrap();
        engine.commit().unwrap();
        let names = local.names().unwrap();
        assert!(!names.iter().any(|n| n == "write.lock"));
        for bad in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            assert_eq!(local.read(bad), Err(Error::Native));
        }
        assert_eq!(local.read("missing"), Err(Error::Native));
        names
            .into_iter()
            .map(|n| {
                let bytes = local.read(&n).unwrap();
                (n, bytes)
            })
            .collect::<BTreeMap<_, _>>()
    })
    .unwrap();
    assert!(!files.is_empty());
    let names: Vec<String> = files.keys().cloned().collect();
    let cache = root("cache");
    let snapshot = Snapshot::open_remote(
        &cache,
        Files(files.clone()),
        names.clone(),
        1 << 20,
        schema.clone(),
    )
    .unwrap();
    assert_eq!(snapshot.search(&Query::All, &[], 0, &[]).unwrap().total, 2);
    // A name outside the manifest or a missing remote file is a failure, not a guess.
    let mut missing = files;
    missing.remove(&names[0]);
    assert!(Snapshot::open_remote(&cache, Files(missing), names, 1 << 20, schema.clone()).is_err());
    assert!(
        Snapshot::open_remote(
            &cache,
            Files(BTreeMap::new()),
            vec!["../escape".into()],
            1 << 20,
            schema
        )
        .is_err()
    );
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(cache);
}

#[test]
fn an_existing_scratch_directory_is_emptied_first() {
    let dir = root("reuse");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stale"), b"junk").unwrap();
    with_local_engine(&dir, schema(), |_, local| {
        assert!(!local.names().unwrap().iter().any(|n| n == "stale"));
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(dir);
}
