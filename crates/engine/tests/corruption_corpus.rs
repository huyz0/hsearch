//! Corruption corpus: damaged native files must open to the right answers or fail,
//! never answer wrongly and never panic. Each committed file is truncated, has bytes
//! flipped at several offsets, and has its footer zeroed.
#[path = "support/remote_fixture.rs"]
mod fixture;

use fixture::{Scratch, answers, schema, write_documents};
use hsearch_engine::{EngineDirectory, Query, Snapshot, Sort, Value};
use lucene_store::FsDirectory;

type Answers = Vec<(u64, Vec<String>)>;

/// The fixture's answers, or None when the damage was refused (open or search).
fn attempt(dir: &std::path::Path, verify: bool) -> Option<Answers> {
    let directory = EngineDirectory::new(FsDirectory::open(dir));
    if verify {
        hsearch_engine::verify_directory(&FsDirectory::open(dir)).ok()?;
    }
    let snapshot = Snapshot::open_committed(&directory, schema()).ok()?;
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
    let mut out = Vec::new();
    for query in &queries {
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
            .ok()?;
        out.push((
            result.total,
            result.hits.into_iter().map(|h| h.id).collect(),
        ));
    }
    Some(out)
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

#[test]
fn damaged_files_are_refused_or_answer_exactly_never_wrongly() {
    let (written, files) = write_documents();
    let expected = answers(
        &Snapshot::open_committed(
            &EngineDirectory::new(FsDirectory::open(&written.0)),
            schema(),
        )
        .unwrap(),
    );
    assert_eq!(attempt(&written.0, true).as_ref(), Some(&expected));

    let mut refused = 0usize;
    let mut exact = 0usize;
    let mut wrong: Vec<String> = Vec::new();
    let mut open_only_wrong = 0usize;
    for (name, bytes) in &files {
        if bytes.is_empty() {
            continue;
        }
        let len = bytes.len();
        let mut variants: Vec<(String, Vec<u8>)> = Vec::new();
        variants.push(("truncate-half".into(), bytes[..len / 2].to_vec()));
        variants.push((
            "truncate-footer".into(),
            bytes[..len.saturating_sub(8)].to_vec(),
        ));
        variants.push(("empty".into(), Vec::new()));
        let mut zeroed = bytes.clone();
        let tail = len.saturating_sub(16);
        zeroed[tail..].fill(0);
        variants.push(("zero-footer".into(), zeroed));
        for at in [0, len / 4, len / 2, 3 * len / 4, len - 1] {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0xFF;
            variants.push((format!("flip@{at}"), flipped));
        }
        for (label, damaged) in variants {
            let scratch = Scratch::new("corrupt");
            copy_dir(&written.0, &scratch.0);
            std::fs::write(scratch.0.join(name), &damaged).unwrap();
            // Open and search alone, as Lucene does: a flip inside a data file can
            // slip through. Counted, not asserted, because it is Lucene's behaviour.
            if attempt(&scratch.0, false).is_some_and(|got| got != expected) {
                open_only_wrong += 1;
            }
            match attempt(&scratch.0, true) {
                None => refused += 1,
                Some(got) if got == expected => exact += 1,
                Some(_) => wrong.push(format!("{name} {label}")),
            }
        }
    }
    eprintln!(
        "corruption corpus: refused={refused} exact={exact} wrong={} open_only_wrong={open_only_wrong}",
        wrong.len()
    );
    // The corpus is only meaningful if open alone really is fooled by something.
    assert!(
        open_only_wrong > 0,
        "expected open-without-verify to be fooled by at least one flip"
    );
    assert!(
        wrong.is_empty(),
        "damage changed an answer without an error: {wrong:?}"
    );
}
