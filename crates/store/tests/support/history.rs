//! A history of batches written by a model that never touches the engine, shared by the tests that
//! recover an engine from the log.
#![allow(dead_code, unused_imports)]
use std::collections::BTreeMap;

use hsearch_commit::{Committer, Receipt, Request};
use hsearch_engine::{Kind, Mutation, Outcome, Schema, Value, encode_batch};

use super::support::*;

pub fn schema() -> Schema {
    Schema::new(1, &[("tag", Kind::Keyword)]).unwrap()
}

/// One batch of the history, written by a model that never touches the engine.
pub struct Step {
    pub id: &'static str,
    pub version: i64,
    /// `None` deletes.
    pub tag: Option<&'static str>,
}

pub fn history() -> Vec<Vec<Step>> {
    let put = |id, version, tag| Step {
        id,
        version,
        tag: Some(tag),
    };
    vec![
        vec![put("a", 1, "x"), put("b", 1, "y")],
        vec![put("a", 2, "z")],
        vec![Step {
            id: "b",
            version: 3,
            tag: None,
        }],
        vec![put("c", 4, "w"), put("d", 4, "w")],
        vec![Step {
            id: "never",
            version: 5,
            tag: None,
        }],
    ]
}

pub fn body(i: usize, steps: &[Step]) -> Vec<u8> {
    let fields: Vec<[(&str, Value<'_>); 1]> = steps
        .iter()
        .map(|s| [("tag", Value::String(s.tag.unwrap_or("")))])
        .collect();
    let mutations: Vec<Mutation<'_>> = steps
        .iter()
        .zip(&fields)
        .map(|(s, f)| match s.tag {
            Some(_) => Mutation::Put {
                id: s.id,
                version: s.version,
                fields: f,
            },
            None => Mutation::Delete {
                id: s.id,
                version: s.version,
            },
        })
        .collect();
    encode_batch(&format!("r{i}"), &mutations).unwrap()
}

pub fn write(
    c: &mut Committer<Crash, hsearch_commit::AnyDependencies>,
    batches: &[Vec<u8>],
) -> Vec<u64> {
    batches
        .iter()
        .enumerate()
        .map(|(seq, b)| {
            let r: Receipt = commit(
                c,
                Request {
                    body: b.clone(),
                    ..req(INDEX, seq as u64, "")
                },
            );
            r.ordinal
        })
        .collect()
}

/// The outcome each step must report, from a model of live identities that never uses the engine.
pub fn outcomes(history: &[Vec<Step>]) -> Vec<Vec<Outcome>> {
    let mut live = std::collections::BTreeSet::new();
    history
        .iter()
        .map(|batch| {
            batch
                .iter()
                .map(|s| match s.tag {
                    Some(_) if live.insert(s.id) => Outcome::Created,
                    Some(_) => Outcome::Updated,
                    None if live.remove(s.id) => Outcome::Deleted,
                    None => Outcome::NotFound,
                })
                .collect()
        })
        .collect()
}

/// The last step per identity: its version and tag, `None` tag for a delete.
pub fn ledger(history: &[Vec<Step>]) -> BTreeMap<&'static str, (i64, Option<&'static str>)> {
    let mut live = BTreeMap::new();
    for step in history.iter().flatten() {
        live.insert(step.id, (step.version, step.tag));
    }
    live
}
