//! Shared helpers: schema, operations and scratch directories.
#![allow(dead_code)]
use hsearch_engine::{Kind, Query, Schema, Snapshot, Sort, Value};
use hsearch_index::{Field, Op};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

pub struct Scratch(pub PathBuf);
impl Scratch {
    pub fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "hsearch-index-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
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

pub fn put(id: &str, tag: &str, n: i64) -> Op {
    Op::Put {
        id: id.to_string(),
        fields: vec![Field::Str(tag.to_string()), Field::Int(n)],
    }
}

pub fn delete(id: &str) -> Op {
    Op::Delete { id: id.to_string() }
}

/// Ids of every document, ordered by `n`, and the exact count.
pub fn all(snapshot: &Snapshot) -> (u64, Vec<String>) {
    let result = snapshot
        .search(
            &Query::All,
            &[Sort::Field {
                name: "n",
                descending: false,
            }],
            128,
            &[],
        )
        .unwrap();
    (
        result.total,
        result.hits.into_iter().map(|h| h.id).collect(),
    )
}

pub fn tagged(snapshot: &Snapshot, tag: &str) -> u64 {
    snapshot
        .search(
            &Query::Term {
                field: "tag",
                value: Value::String(tag),
            },
            &[],
            0,
            &[],
        )
        .unwrap()
        .total
}
