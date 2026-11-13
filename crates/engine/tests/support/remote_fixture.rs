//! Shared fixture: write documents with the engine, and answer a fixed query set.
#![allow(dead_code)]
use hsearch_engine::{
    Engine, EngineDirectory, Kind, Mutation, Query, Schema, Snapshot, Sort, Value,
};
use lucene_store::{Directory, FsDirectory};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

pub struct Scratch(pub PathBuf);
impl Scratch {
    pub fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "hsearch-remote-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn schema() -> Schema {
    Schema::new(1, &[("tag", Kind::Keyword), ("n", Kind::Integer)]).unwrap()
}

/// Write 40 documents with the engine into a local directory, commit, and
/// upload every native file except the lock to `store`.
pub fn write_documents() -> (Scratch, Vec<(String, Vec<u8>)>) {
    let scratch = Scratch::new("write");
    std::fs::create_dir_all(&scratch.0).unwrap();
    let directory = EngineDirectory::new(FsDirectory::open(&scratch.0));
    let schema = schema();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    for batch in 0..4 {
        let ids: Vec<String> = (0..10).map(|i| format!("d{}", batch * 10 + i)).collect();
        let tags: Vec<String> = (0..10).map(|i| format!("t{}", i % 3)).collect();
        let owned: Vec<(String, i64, String, i64)> = (0..10)
            .map(|i| {
                (
                    ids[i].clone(),
                    1,
                    tags[i].clone(),
                    i64::try_from(batch * 10 + i).unwrap(),
                )
            })
            .collect();
        let fields: Vec<[(&str, Value<'_>); 2]> = owned
            .iter()
            .map(|(_, _, tag, n)| [("tag", Value::String(tag)), ("n", Value::Integer(*n))])
            .collect();
        let mutations: Vec<Mutation<'_>> = owned
            .iter()
            .zip(&fields)
            .map(|((id, version, _, _), f)| Mutation::Put {
                id,
                version: *version,
                fields: f,
            })
            .collect();
        engine
            .apply(
                schema
                    .prepare(&format!("batch-{batch}"), &mutations)
                    .unwrap(),
            )
            .unwrap();
    }
    engine.commit().unwrap();
    engine.close().unwrap();
    let local = FsDirectory::open(&scratch.0);
    let mut names = Vec::new();
    for name in local.list_all().unwrap() {
        if name == "write.lock" {
            continue;
        }
        let bytes = std::fs::read(scratch.0.join(&name)).unwrap();
        names.push((name, bytes));
    }
    (scratch, names)
}

pub fn answers(snapshot: &Snapshot) -> Vec<(u64, Vec<String>)> {
    let queries = [
        Query::All,
        Query::Term {
            field: "tag",
            value: Value::String("t1"),
        },
        Query::Range {
            field: "n",
            lower: Value::Integer(5),
            upper: Value::Integer(17),
        },
    ];
    queries
        .iter()
        .map(|query| {
            let result = snapshot
                .search(
                    query,
                    &[Sort::Field {
                        name: "n",
                        descending: false,
                    }],
                    100,
                    &["n"],
                )
                .unwrap();
            (
                result.total,
                result.hits.into_iter().map(|h| h.id).collect(),
            )
        })
        .collect()
}
