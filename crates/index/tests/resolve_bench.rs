//! Not a gate: prints the cost of preparing a full batch. Run with
//! `cargo test --release -p hsearch-index --test resolve_bench -- --ignored --nocapture`.
use hsearch_index::Field;
use hsearch_index::resolve::{Committed, CommittedState, Cond, Request, Resolver};
use std::collections::HashMap;
use std::time::Instant;
struct S(HashMap<String, Committed>);
impl CommittedState for S {
    fn head(&self) -> u64 {
        0
    }
    fn get(&self, id: &str) -> Option<Committed> {
        self.0.get(id).cloned()
    }
}
#[test]
#[ignore]
fn prepare_a_full_batch_of_partial_updates() {
    let docs: HashMap<String, Committed> = (0..4096)
        .map(|i| {
            (
                format!("k{i}"),
                Committed {
                    seq_no: 1,
                    primary_term: 1,
                    fields: vec![
                        Field::Str("x".repeat(200)),
                        Field::Int(i),
                        Field::Bool(true),
                    ],
                },
            )
        })
        .collect();
    let s = S(docs);
    let r = Resolver::new(
        1,
        &[
            ("title", hsearch_engine::Kind::Text),
            ("price", hsearch_engine::Kind::Integer),
            ("active", hsearch_engine::Kind::Boolean),
        ],
        1 << 20,
    );
    let reqs: Vec<Request> = (0..4096)
        .map(|i| Request::Update {
            id: format!("k{i}"),
            doc: vec![("price".into(), Field::Int(i + 1))],
            upsert: None,
            doc_as_upsert: false,
            detect_noop: true,
            cond: Cond::none(),
        })
        .collect();
    let _ = r.prepare(&s, 2, &reqs).unwrap();
    let t = Instant::now();
    let n = 50;
    for _ in 0..n {
        std::hint::black_box(r.prepare(&s, 2, &reqs).unwrap());
    }
    let per_batch = t.elapsed() / n;
    println!(
        "4096 partial updates (200 B title): {per_batch:?} per batch, {:?} per op",
        per_batch / 4096
    );
}
