//! The early conformance corpus (`fixtures/api/conformance/early-v1`) replayed
//! against a real node over HTTP.
//!
//! Three classes of case: `reference` (OpenSearch behaviour hsearch matches),
//! `difference` (an intentional departure the profile records) and
//! `unsupported` (a request outside core-v1 that must be refused by name and
//! change nothing). A case is `enforced` and must pass, or `pending` (the
//! profile promises it, the node does not serve it yet) and must still fail:
//! the run that finds a pending case passing fails until it is promoted.
//!
//! The corpus is checked structurally before anything runs: every area exists
//! in `docs/plan/api-profile.json`, every owner exists in `tasks.json`, every
//! `reference_cases` entry exists in the core-v1 fixtures, ids are unique, and
//! every profile area has a case. No case claims an SDK or a drop-in server.
mod support;

use hsearch_node::{Node, serve};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use support::*;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn corpus_dir() -> PathBuf {
    repo().join("fixtures/api/conformance/early-v1")
}

/// Every `id` string reachable in a JSON tree under an object key `id`.
fn ids_in(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("id") {
                out.insert(id.clone());
            }
            map.values().for_each(|v| ids_in(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| ids_in(v, out)),
        _ => {}
    }
}

struct Corpus {
    dataset: Value,
    cases: Vec<(String, Value)>,
}

fn load() -> Corpus {
    let dir = corpus_dir();
    let mut cases = Vec::new();
    for class in ["reference", "difference", "unsupported"] {
        let file = read(&dir.join(format!("{class}.json")));
        assert_eq!(file["class"], class, "{class}.json names its class");
        for case in file["cases"].as_array().expect("cases").clone() {
            cases.push((class.to_string(), case));
        }
    }
    Corpus {
        dataset: read(&dir.join("dataset.json")),
        cases,
    }
}

fn text<'a>(case: &'a Value, key: &str) -> &'a str {
    case[key]
        .as_str()
        .unwrap_or_else(|| panic!("case {} needs a string [{key}]", case["id"]))
}

#[test]
fn the_corpus_is_well_formed_and_covers_every_profile_area() {
    let corpus = load();
    let profile = read(&repo().join("docs/plan/api-profile.json"));
    let areas: BTreeSet<String> = profile["matrix"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_string())
        .collect();
    let mut tasks = BTreeSet::new();
    ids_in(&read(&repo().join("docs/plan/tasks.json")), &mut tasks);
    let mut reference_cases = BTreeSet::new();
    for file in ["search-cases", "mutation-cases"] {
        ids_in(
            &read(&repo().join(format!("fixtures/api/core-v1/{file}.json"))),
            &mut reference_cases,
        );
    }

    let (mut seen, mut covered) = (BTreeSet::new(), BTreeSet::new());
    for (class, case) in &corpus.cases {
        let id = text(case, "id");
        assert!(seen.insert(id.to_string()), "duplicate case id {id}");
        let area = text(case, "area");
        assert!(areas.contains(area), "{id}: unknown area {area}");
        covered.insert(area.to_string());
        let oracle = &case["oracle"];
        assert!(
            ["opensearch-docs", "hsearch-profile"].contains(&text(oracle, "kind")),
            "{id}: oracle kind"
        );
        assert!(!text(oracle, "ref").is_empty(), "{id}: oracle ref");
        let state = text(case, "state");
        match state {
            "enforced" => assert!(case.get("owner").is_none(), "{id}: owner only when pending"),
            "pending" => {
                let owner = text(case, "owner");
                assert!(tasks.contains(owner), "{id}: unknown owner task {owner}");
            }
            other => panic!("{id}: unknown state {other}"),
        }
        if let Some(until) = case.get("temporary_until") {
            let until = until.as_str().unwrap();
            assert!(tasks.contains(until), "{id}: unknown task {until}");
        }
        for name in case
            .get("reference_cases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = name.as_str().unwrap();
            assert!(
                reference_cases.contains(name),
                "{id}: no core-v1 case {name}"
            );
        }
        if class == "difference" || class == "unsupported" {
            // A departure is explained: the profile row, and for a difference
            // the OpenSearch behaviour it departs from.
            assert!(
                text(oracle, "kind") == "hsearch-profile",
                "{id}: oracle kind for {class}"
            );
        }
        if class == "difference" && state == "enforced" {
            assert!(!text(case, "opensearch").is_empty(), "{id}: opensearch");
        }
        let steps = case["steps"].as_array().expect("steps");
        assert!(!steps.is_empty(), "{id}: no steps");
        for step in steps {
            assert!(
                ["GET", "POST", "PUT", "DELETE"].contains(&text(step, "method")),
                "{id}: method"
            );
            assert!(text(step, "path").starts_with('/'), "{id}: path");
            assert!(
                step["expect"].get("status").is_some() ^ step["expect"].get("status_in").is_some(),
                "{id}: exactly one of status and status_in"
            );
            assert!(
                step.get("body").is_none() || step.get("body_text").is_none(),
                "{id}: body or body_text"
            );
        }
    }
    let missing: Vec<_> = areas.difference(&covered).collect();
    assert!(
        missing.is_empty(),
        "profile areas with no case: {missing:?}"
    );
}

/// Does `actual` satisfy `expected`? Objects must contain the expected keys,
/// arrays must match in length and position, `"<any>"` accepts any present
/// value, `"<contains:x>"` accepts a string holding x, `"<absent>"` demands the key be missing.
fn matches(expected: &Value, actual: Option<&Value>, path: &str) -> Result<(), String> {
    if expected == "<absent>" {
        return actual.map_or(Ok(()), |v| Err(format!("{path}: expected absent, got {v}")));
    }
    let Some(actual) = actual else {
        return Err(format!("{path}: missing"));
    };
    match expected {
        Value::String(s) if s == "<any>" => Ok(()),
        Value::String(s) if s.starts_with("<contains:") && s.ends_with('>') => {
            let needle = &s["<contains:".len()..s.len() - 1];
            if actual.as_str().is_some_and(|a| a.contains(needle)) {
                Ok(())
            } else {
                Err(format!("{path}: {actual} does not contain {needle:?}"))
            }
        }
        Value::Object(want) => {
            let Value::Object(have) = actual else {
                return Err(format!("{path}: expected an object, got {actual}"));
            };
            want.iter()
                .try_for_each(|(k, v)| matches(v, have.get(k), &format!("{path}.{k}")))
        }
        Value::Array(want) => {
            let Value::Array(have) = actual else {
                return Err(format!("{path}: expected an array, got {actual}"));
            };
            if want.len() != have.len() {
                return Err(format!(
                    "{path}: {} items, expected {}",
                    have.len(),
                    want.len()
                ));
            }
            want.iter()
                .zip(have)
                .enumerate()
                .try_for_each(|(i, (w, h))| matches(w, Some(h), &format!("{path}[{i}]")))
        }
        _ if expected == actual => Ok(()),
        _ => Err(format!("{path}: expected {expected}, got {actual}")),
    }
}

fn start() -> (Arc<MemoryStore>, Scratch, hsearch_node::Running) {
    let store = Arc::new(
        MemoryStore::new(MemoryLimits {
            objects: 4096,
            versions: 16384,
            version_bytes: 64 << 20,
            identities: 16384,
            identity_bytes: 64 << 20,
        })
        .unwrap(),
    );
    let dir = Scratch::new("conformance");
    let node = Arc::new(Node::new(Arc::clone(&store), dir.0.clone()));
    let running = serve(node, "127.0.0.1:0", 2, false).unwrap();
    (store, dir, running)
}

fn load_dataset(addr: std::net::SocketAddr, dataset: &Value) {
    let index = dataset["index"].as_str().unwrap();
    let (status, body) = call(
        addr,
        "PUT",
        &format!("/{index}"),
        &dataset["mapping"].to_string(),
    );
    assert_eq!(status, 200, "dataset mapping: {body}");
    let mut ndjson = String::new();
    for document in dataset["documents"].as_array().unwrap() {
        let mut source = document.clone();
        let id = source.as_object_mut().unwrap().remove("_id").unwrap();
        ndjson += &format!("{}\n{source}\n", json!({"index": {"_id": id}}));
    }
    let (status, body) = call(
        addr,
        "POST",
        &format!("/{index}/_bulk?refresh=true"),
        &ndjson,
    );
    assert_eq!(
        (status, &body["errors"]),
        (200, &json!(false)),
        "dataset load: {body}"
    );
}

/// Run a case on a fresh node; the first mismatch, if any.
fn run(case: &Value, dataset: &Value) -> Result<(), String> {
    let (_store, _dir, node) = start();
    let addr = node.addr;
    if case["dataset"] == json!(true) {
        load_dataset(addr, dataset);
    }
    let mut outcome = Ok(());
    for (n, step) in case["steps"].as_array().unwrap().iter().enumerate() {
        let body = match (step.get("body"), step.get("body_text")) {
            (Some(json), _) => json.to_string(),
            (None, Some(raw)) => raw.as_str().unwrap().to_string(),
            (None, None) => String::new(),
        };
        let (status, got) = call(addr, text(step, "method"), text(step, "path"), &body);
        let expect = &step["expect"];
        let allowed: Vec<u64> = match (expect.get("status"), expect.get("status_in")) {
            (Some(one), _) => vec![one.as_u64().unwrap()],
            (None, Some(many)) => many
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_u64)
                .collect(),
            (None, None) => unreachable!("checked structurally"),
        };
        let verdict = if allowed.contains(&u64::from(status)) {
            expect
                .get("body")
                .map_or(Ok(()), |want| matches(want, Some(&got), "body"))
        } else {
            Err(format!(
                "status {status}, expected one of {allowed:?}; body {got}"
            ))
        };
        if let Err(why) = verdict {
            outcome = Err(format!(
                "step {n} {} {}: {why}",
                text(step, "method"),
                text(step, "path")
            ));
            break;
        }
    }
    node.shutdown();
    outcome
}

#[test]
fn enforced_cases_pass_and_pending_cases_still_fail() {
    let corpus = load();
    let (mut failures, mut promote) = (Vec::new(), Vec::new());
    let (mut enforced, mut pending) = (0, 0);
    for (class, case) in &corpus.cases {
        let id = text(case, "id");
        let result = run(case, &corpus.dataset);
        match (text(case, "state"), result) {
            ("enforced", Ok(())) => enforced += 1,
            ("enforced", Err(why)) => failures.push(format!("{class}/{id}: {why}")),
            (_, Err(why)) => {
                eprintln!("pending {class}/{id}: {why}");
                pending += 1;
            }
            (_, Ok(())) => promote.push(format!("{class}/{id}")),
        }
    }
    eprintln!("conformance: {enforced} enforced passing, {pending} pending still failing");
    assert!(
        failures.is_empty(),
        "enforced cases failed:\n{}",
        failures.join("\n")
    );
    assert!(
        promote.is_empty(),
        "pending cases now pass; set state to enforced and drop the owner: {promote:?}"
    );
}

/// A pending case cannot hide a regression forever: every pending owner must
/// still be an unfinished task.
#[test]
fn a_pending_case_names_a_task_that_is_not_done() {
    let corpus = load();
    let tasks = read(&repo().join("docs/plan/tasks.json"));
    let status_of = |wanted: &str| -> Option<String> {
        fn find(value: &Value, wanted: &str) -> Option<String> {
            match value {
                Value::Object(map) => {
                    if map.get("id").and_then(Value::as_str) == Some(wanted) {
                        return map.get("status").and_then(Value::as_str).map(String::from);
                    }
                    map.values().find_map(|v| find(v, wanted))
                }
                Value::Array(items) => items.iter().find_map(|v| find(v, wanted)),
                _ => None,
            }
        }
        find(&tasks, wanted)
    };
    for (_, case) in corpus.cases.iter().filter(|(_, c)| c["state"] == "pending") {
        let owner = text(case, "owner");
        let status = status_of(owner).unwrap_or_default();
        assert_ne!(
            status, "done",
            "{}: owner {owner} is done but the case is pending",
            case["id"]
        );
    }
}
