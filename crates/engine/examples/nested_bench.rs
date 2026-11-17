//! Write cost and query latency of nested objects and has_child / has_parent, against the
//! same roots with no nested objects. One process; provisional: one shared VM, in-memory
//! directory, one thread, no ARM.
//!
//!     cargo run --release -p hsearch-engine --example nested_bench -- [PRODUCTS] [QUESTIONS]
use std::time::Instant;

use hsearch_engine::{
    Engine, EngineDirectory, JoinScore, Kind, Limits, Mutation, Query, Schema, Snapshot, Sort,
    Value,
};
use lucene_store::ByteBuffersDirectory;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const AUTHORS: [&str; 5] = ["alice", "bob", "carol", "dave", "erin"];

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p) as usize]
}

/// Median and p99 of `runs` searches, in microseconds, and the hits of the last.
fn time(snapshot: &Snapshot, query: &Query<'_>, runs: usize) -> (f64, f64, u64) {
    let mut micros = Vec::with_capacity(runs);
    let mut total = 0;
    for _ in 0..runs {
        let start = Instant::now();
        total = snapshot
            .search(query, &[Sort::Score], 10, &[])
            .unwrap()
            .total;
        micros.push(start.elapsed().as_secs_f64() * 1e6);
    }
    micros.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (percentile(&micros, 0.5), percentile(&micros, 0.99), total)
}

fn load_products(nested: bool, n: usize) -> (f64, usize, Snapshot, Schema) {
    let mut rng = Rng(42);
    let base = Schema::new(1, &[("brand", Kind::Keyword), ("title", Kind::Text)]).unwrap();
    let schema = if nested {
        base.with_nested(
            "reviews",
            &[("author", Kind::Keyword), ("stars", Kind::Integer)],
        )
        .unwrap()
    } else {
        base
    }
    .with_limits(Limits::scale())
    .unwrap();
    let directory: &'static ByteBuffersDirectory = Box::leak(Box::new(ByteBuffersDirectory::new()));
    let observed: &'static EngineDirectory<&'static ByteBuffersDirectory> =
        Box::leak(Box::new(EngineDirectory::new(directory)));
    let engine = Engine::open(observed, schema.clone(), None).unwrap();
    let ids: Vec<String> = (0..n).map(|i| format!("p{i}")).collect();
    let mut objects_total = 0;
    let start = Instant::now();
    for (b, chunk) in ids.chunks(16).enumerate() {
        let reviews: Vec<Vec<[(&str, Value); 2]>> = chunk
            .iter()
            .map(|_| {
                (0..5)
                    .map(|_| {
                        [
                            ("author", Value::String(AUTHORS[rng.below(5) as usize])),
                            ("stars", Value::Integer(1 + rng.below(5) as i64)),
                        ]
                    })
                    .collect()
            })
            .collect();
        let slices: Vec<Vec<&[(&str, Value)]>> = reviews
            .iter()
            .map(|r| r.iter().map(|o| o.as_slice()).collect())
            .collect();
        objects_total += slices.iter().map(Vec::len).sum::<usize>();
        let fields: Vec<Vec<(&str, Value)>> = slices
            .iter()
            .map(|s| {
                let mut f = vec![
                    ("brand", Value::String("acme")),
                    ("title", Value::String("a widget of some kind")),
                ];
                if nested {
                    f.push(("reviews", Value::Nested(s)));
                }
                f
            })
            .collect();
        let mutations: Vec<Mutation> = chunk
            .iter()
            .zip(&fields)
            .map(|(id, f)| Mutation::Put {
                id,
                version: 1,
                fields: f,
            })
            .collect();
        engine
            .apply(schema.prepare(&format!("b{b}"), &mutations).unwrap())
            .unwrap();
    }
    let snapshot = engine.refresh().unwrap();
    (
        start.elapsed().as_secs_f64(),
        if nested { objects_total } else { 0 },
        snapshot,
        schema,
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let products: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    let questions: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(5_000);

    println!("== nested: {products} roots, 5 objects each");
    let (flat_s, _, flat, _) = load_products(false, products);
    let (nest_s, objects, nest, _) = load_products(true, products);
    println!(
        "write: flat {:.2}s ({:.0} roots/s); nested {:.2}s ({:.0} roots/s, {:.0} native docs/s), x{:.2}",
        flat_s,
        products as f64 / flat_s,
        nest_s,
        products as f64 / nest_s,
        (products + objects) as f64 / nest_s,
        nest_s / flat_s
    );
    let brand = Query::Term {
        field: "brand",
        value: Value::String("acme"),
    };
    for (name, snapshot) in [("flat", &flat), ("nested schema", &nest)] {
        let (p50, p99, total) = time(snapshot, &brand, 300);
        println!("root term query on {name}: p50 {p50:.0} us, p99 {p99:.0} us, {total} hits");
    }
    let inner = [
        Query::Term {
            field: "reviews.author",
            value: Value::String("alice"),
        },
        Query::Range {
            field: "reviews.stars",
            lower: Value::Integer(5),
            upper: Value::Integer(5),
        },
    ];
    let both = Query::Bool {
        must: &inner,
        filter: &[],
        should: &[],
        must_not: &[],
        minimum_should_match: 0,
    };
    for mode in [JoinScore::None, JoinScore::Avg] {
        let q = Query::Nested {
            path: "reviews",
            query: &both,
            score_mode: mode,
        };
        let (p50, p99, total) = time(&nest, &q, 300);
        println!("nested author+stars ({mode:?}): p50 {p50:.0} us, p99 {p99:.0} us, {total} hits");
    }

    println!("== join: {questions} questions, {} answers", questions * 4);
    let schema = Schema::new(1, &[("topic", Kind::Keyword), ("votes", Kind::Integer)])
        .unwrap()
        .with_join("qa", "question", &["answer"])
        .unwrap()
        .with_limits(Limits::scale())
        .unwrap();
    let directory: &'static ByteBuffersDirectory = Box::leak(Box::new(ByteBuffersDirectory::new()));
    let observed: &'static EngineDirectory<&'static ByteBuffersDirectory> =
        Box::leak(Box::new(EngineDirectory::new(directory)));
    let engine = Engine::open(observed, schema.clone(), None).unwrap();
    let mut rng = Rng(7);
    let mut posts: Vec<(String, Option<String>, i64)> =
        (0..questions).map(|i| (format!("q{i}"), None, 0)).collect();
    posts.extend((0..questions * 4).map(|i| {
        (
            format!("a{i}"),
            Some(format!("q{}", rng.below(questions as u64))),
            rng.below(6) as i64,
        )
    }));
    let segments = 8;
    let per = posts.len().div_ceil(segments);
    let start = Instant::now();
    for (s, part) in posts.chunks(per).enumerate() {
        for (b, chunk) in part.chunks(16).enumerate() {
            let fields: Vec<[(&str, Value); 3]> = chunk
                .iter()
                .map(|(_, parent, votes)| {
                    [
                        ("topic", Value::String("t")),
                        ("votes", Value::Integer(*votes)),
                        (
                            "qa",
                            Value::Join {
                                relation: if parent.is_some() {
                                    "answer"
                                } else {
                                    "question"
                                },
                                parent: parent.as_deref(),
                            },
                        ),
                    ]
                })
                .collect();
            let mutations: Vec<Mutation> = chunk
                .iter()
                .zip(&fields)
                .map(|((id, _, _), f)| Mutation::Put {
                    id,
                    version: 1,
                    fields: f,
                })
                .collect();
            engine
                .apply(schema.prepare(&format!("s{s}b{b}"), &mutations).unwrap())
                .unwrap();
        }
        drop(engine.refresh().unwrap());
    }
    println!(
        "write: {:.2}s for {} docs",
        start.elapsed().as_secs_f64(),
        posts.len()
    );
    let inner = Query::Range {
        field: "votes",
        lower: Value::Integer(5),
        upper: Value::Integer(5),
    };
    let child = Query::HasChild {
        relation: "answer",
        query: &inner,
        score_mode: JoinScore::None,
        min_children: 1,
        max_children: 0,
    };
    let any = Query::Term {
        field: "topic",
        value: Value::String("t"),
    };
    let parent = Query::HasParent {
        relation: "question",
        query: &any,
        score: false,
    };
    let multi = engine.refresh().unwrap();
    for (label, snapshot) in [("multi-segment", multi)] {
        for (name, q) in [("has_child", &child), ("has_parent", &parent)] {
            let (p50, p99, total) = time(&snapshot, q, 100);
            println!("{name} ({label}): p50 {p50:.0} us, p99 {p99:.0} us, {total} hits");
        }
    }
    engine.merge().unwrap();
    let single = engine.refresh().unwrap();
    for (name, q) in [("has_child", &child), ("has_parent", &parent)] {
        let (p50, p99, total) = time(&single, q, 100);
        println!("{name} (1 segment): p50 {p50:.0} us, p99 {p99:.0} us, {total} hits");
    }
    let (p50, p99, total) = time(&single, &any, 300);
    println!("plain term, same index: p50 {p50:.0} us, p99 {p99:.0} us, {total} hits");
}
