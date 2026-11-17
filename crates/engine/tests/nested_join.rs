//! Nested objects and parent-child joins through the adapter (M21.07, PR-41).
//!
//! Each query is run against the engine and against a plain-Rust oracle that
//! applies OpenSearch's documented semantics to the same corpus: a nested query
//! matches inside one object and never across two; `has_child` counts matching
//! children per parent; `has_parent` takes the parent's match. The oracle is
//! not the Java reference. Matching the pinned OpenSearch results is an open
//! item for the M02.01 reference pin, and the evidence file says so.
use hsearch_engine::{
    Capability, Engine, EngineDirectory, EngineFactory, Error, JoinScore, Kind, Limits, Mutation,
    Query, Refusal, Schema, Snapshot, Sort, Support, Value,
};
use lucene_store::ByteBuffersDirectory;
use std::collections::BTreeSet;

/// Deterministic xorshift, so every run builds the same corpus.
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
const WORDS: [&str; 6] = ["fast", "slow", "cheap", "solid", "loud", "quiet"];

struct Review {
    author: &'static str,
    stars: i64,
    body: String,
}
struct Product {
    id: String,
    brand: &'static str,
    title: String,
    reviews: Vec<Review>,
}

fn products(n: usize, seed: u64) -> Vec<Product> {
    let mut rng = Rng(seed);
    (0..n)
        .map(|i| Product {
            id: format!("p{i}"),
            brand: ["acme", "globex", "initech"][i % 3],
            title: format!("widget {}", WORDS[rng.below(6) as usize]),
            reviews: (0..rng.below(6))
                .map(|_| Review {
                    author: AUTHORS[rng.below(5) as usize],
                    stars: 1 + rng.below(5) as i64,
                    body: format!(
                        "{} {}",
                        WORDS[rng.below(6) as usize],
                        WORDS[rng.below(6) as usize]
                    ),
                })
                .collect(),
        })
        .collect()
}

fn product_schema() -> Schema {
    Schema::new(1, &[("brand", Kind::Keyword), ("title", Kind::Text)])
        .unwrap()
        .with_nested(
            "reviews",
            &[
                ("author", Kind::Keyword),
                ("stars", Kind::Integer),
                ("body", Kind::Text),
            ],
        )
        .unwrap()
        .with_limits(Limits::scale())
        .unwrap()
}

fn put_products(engine: &Engine<'_>, schema: &Schema, items: &[Product], version: i64) {
    for (b, chunk) in items.chunks(16).enumerate() {
        let objects: Vec<Vec<[(&str, Value); 3]>> = chunk
            .iter()
            .map(|p| {
                p.reviews
                    .iter()
                    .map(|r| {
                        [
                            ("author", Value::String(r.author)),
                            ("stars", Value::Integer(r.stars)),
                            ("body", Value::String(&r.body)),
                        ]
                    })
                    .collect()
            })
            .collect();
        let slices: Vec<Vec<&[(&str, Value)]>> = objects
            .iter()
            .map(|os| os.iter().map(|o| o.as_slice()).collect())
            .collect();
        let fields: Vec<[(&str, Value); 3]> = chunk
            .iter()
            .zip(&slices)
            .map(|(p, s)| {
                [
                    ("brand", Value::String(p.brand)),
                    ("title", Value::String(&p.title)),
                    ("reviews", Value::Nested(s)),
                ]
            })
            .collect();
        let mutations: Vec<Mutation> = chunk
            .iter()
            .zip(&fields)
            .map(|(p, f)| Mutation::Put {
                id: &p.id,
                version,
                fields: f,
            })
            .collect();
        engine
            .apply(
                schema
                    .prepare(&format!("put-{b}-{version}-{}", chunk[0].id), &mutations)
                    .unwrap(),
            )
            .unwrap();
    }
}

fn open<'d>(
    directory: &'d EngineDirectory<&'d ByteBuffersDirectory>,
    schema: &Schema,
) -> Engine<'d> {
    Engine::open(directory, schema.clone(), None).unwrap()
}

fn ids(snapshot: &Snapshot, query: &Query<'_>) -> BTreeSet<String> {
    let result = snapshot.search(query, &[Sort::Score], 10_000, &[]).unwrap();
    assert_eq!(result.total as usize, result.hits.len());
    result.hits.into_iter().map(|h| h.id).collect()
}

fn author_stars<'a>(author: &'a str, lo: i64, hi: i64, bounds: &'a [Query<'a>; 2]) -> Query<'a> {
    let _ = (author, lo, hi);
    Query::Bool {
        must: bounds,
        filter: &[],
        should: &[],
        must_not: &[],
        minimum_should_match: 0,
    }
}

#[test]
fn nested_matches_inside_one_object_like_the_oracle() {
    let schema = product_schema();
    let items = products(200, 7);
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    put_products(&engine, &schema, &items, 1);
    let snapshot = engine.refresh().unwrap();

    // A top-level query sees roots only, never the objects.
    assert_eq!(
        snapshot.search(&Query::All, &[], 0, &[]).unwrap().total,
        items.len() as u64
    );

    for (author, lo, hi) in [
        ("alice", 4, 5),
        ("bob", 1, 2),
        ("erin", 3, 3),
        ("carol", 5, 5),
    ] {
        let inner = [
            Query::Term {
                field: "reviews.author",
                value: Value::String(author),
            },
            Query::Range {
                field: "reviews.stars",
                lower: Value::Integer(lo),
                upper: Value::Integer(hi),
            },
        ];
        let inner = author_stars(author, lo, hi, &inner);
        let query = Query::Nested {
            path: "reviews",
            query: &inner,
            score_mode: JoinScore::Avg,
        };
        let expected: BTreeSet<String> = items
            .iter()
            .filter(|p| {
                p.reviews
                    .iter()
                    .any(|r| r.author == author && (lo..=hi).contains(&r.stars))
            })
            .map(|p| p.id.clone())
            .collect();
        assert!(
            !expected.is_empty(),
            "{author}: the corpus must exercise it"
        );
        assert_eq!(ids(&snapshot, &query), expected, "{author} {lo}..{hi}");

        // The same two conditions across objects match more roots than inside one.
        let across: BTreeSet<String> = items
            .iter()
            .filter(|p| {
                p.reviews.iter().any(|r| r.author == author)
                    && p.reviews.iter().any(|r| (lo..=hi).contains(&r.stars))
            })
            .map(|p| p.id.clone())
            .collect();
        assert!(
            expected.len() < across.len(),
            "{author}: objects must differ"
        );
    }
    drop(snapshot);
    engine.close().unwrap();
}

#[test]
fn nested_text_scores_and_score_modes_agree_on_the_match_set() {
    let schema = product_schema();
    let items = products(120, 11);
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    put_products(&engine, &schema, &items, 1);
    let snapshot = engine.refresh().unwrap();
    let inner = Query::Match {
        field: "reviews.body",
        text: "fast cheap",
        phrase: false,
    };
    let expected: BTreeSet<String> = items
        .iter()
        .filter(|p| {
            p.reviews
                .iter()
                .any(|r| r.body.split(' ').any(|w| w == "fast" || w == "cheap"))
        })
        .map(|p| p.id.clone())
        .collect();
    assert!(!expected.is_empty());
    for mode in [
        JoinScore::None,
        JoinScore::Avg,
        JoinScore::Max,
        JoinScore::Sum,
        JoinScore::Min,
    ] {
        let query = Query::Nested {
            path: "reviews",
            query: &inner,
            score_mode: mode,
        };
        assert_eq!(ids(&snapshot, &query), expected, "{mode:?}");
    }
    // A nested match composes with root conditions in a bool.
    let nested = Query::Nested {
        path: "reviews",
        query: &inner,
        score_mode: JoinScore::Max,
    };
    let brand = Query::Term {
        field: "brand",
        value: Value::String("acme"),
    };
    let both = Query::Bool {
        must: &[],
        filter: &[nested, brand],
        should: &[],
        must_not: &[],
        minimum_should_match: 0,
    };
    let want: BTreeSet<String> = expected
        .iter()
        .filter(|id| items.iter().any(|p| &p.id == *id && p.brand == "acme"))
        .cloned()
        .collect();
    assert_eq!(ids(&snapshot, &both), want);
    drop(snapshot);
    engine.close().unwrap();
}

#[test]
fn replacing_or_deleting_a_root_takes_its_objects_with_it() {
    let schema = product_schema();
    let mut items = products(40, 3);
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    put_products(&engine, &schema, &items, 1);

    let by = |author: &'static str| Query::Term {
        field: "reviews.author",
        value: Value::String(author),
    };
    let nested = |q: &Query<'_>| -> BTreeSet<String> {
        let snapshot = engine.refresh().unwrap();
        let query = Query::Nested {
            path: "reviews",
            query: q,
            score_mode: JoinScore::None,
        };
        ids(&snapshot, &query)
    };
    let oracle = |items: &[Product], author: &str| -> BTreeSet<String> {
        items
            .iter()
            .filter(|p| p.reviews.iter().any(|r| r.author == author))
            .map(|p| p.id.clone())
            .collect()
    };
    let alice = by("alice");
    assert_eq!(nested(&alice), oracle(&items, "alice"));

    // Replace every product with one review by a different author.
    for p in &mut items {
        p.reviews = vec![Review {
            author: "zed",
            stars: 5,
            body: "replaced".into(),
        }];
    }
    put_products(&engine, &schema, &items, 2);
    assert!(nested(&alice).is_empty(), "old objects must be gone");
    assert_eq!(nested(&by("zed")).len(), items.len());
    let snapshot = engine.refresh().unwrap();
    assert_eq!(
        snapshot.search(&Query::All, &[], 0, &[]).unwrap().total,
        items.len() as u64
    );
    drop(snapshot);

    // Delete a few: their objects go too.
    let gone: Vec<String> = items.iter().take(5).map(|p| p.id.clone()).collect();
    let deletes: Vec<Mutation> = gone
        .iter()
        .map(|id| Mutation::Delete { id, version: 3 })
        .collect();
    engine
        .apply(schema.prepare("del", &deletes).unwrap())
        .unwrap();
    assert_eq!(nested(&by("zed")).len(), items.len() - 5);
    engine.close().unwrap();
}

#[test]
fn nested_survives_merge_commit_and_reopen() {
    let schema = product_schema();
    let items = products(150, 19);
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    // Several segments, with blocks replaced across them.
    put_products(&engine, &schema, &items[..60], 1);
    drop(engine.refresh().unwrap());
    put_products(&engine, &schema, &items[60..], 1);
    drop(engine.refresh().unwrap());
    put_products(&engine, &schema, &items[..30], 2);
    let inner = Query::Term {
        field: "reviews.author",
        value: Value::String("dave"),
    };
    let query = Query::Nested {
        path: "reviews",
        query: &inner,
        score_mode: JoinScore::None,
    };
    let before = ids(&engine.refresh().unwrap(), &query);
    engine.merge().unwrap();
    assert_eq!(ids(&engine.refresh().unwrap(), &query), before);
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();

    // Reopening verifies every root and every object against the retry digests.
    let engine = Engine::open(&observed, schema.clone(), Some(checkpoint)).unwrap();
    assert_eq!(ids(&engine.refresh().unwrap(), &query), before);
    engine.close().unwrap();
    let committed = Snapshot::open_committed(&observed, schema.clone()).unwrap();
    assert_eq!(ids(&committed, &query), before);
    assert_eq!(
        committed.search(&Query::All, &[], 0, &[]).unwrap().total,
        items.len() as u64
    );
}

#[test]
fn nested_misuse_is_refused_with_a_named_reason() {
    let schema = product_schema();
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    put_products(&engine, &schema, &products(5, 1), 1);
    let snapshot = engine.refresh().unwrap();
    let run = |q: &Query<'_>| snapshot.search(q, &[], 10, &[]).unwrap_err();

    let author = Query::Term {
        field: "reviews.author",
        value: Value::String("alice"),
    };
    // A nested field outside a nested query.
    assert_eq!(run(&author), Error::Refused(Refusal::WrongScope));
    // A path the schema does not map as nested.
    let unmapped = Query::Nested {
        path: "comments",
        query: &author,
        score_mode: JoinScore::None,
    };
    assert_eq!(run(&unmapped), Error::Refused(Refusal::NotNested));
    // A root field inside a nested query.
    let brand = Query::Term {
        field: "brand",
        value: Value::String("acme"),
    };
    let rooted = Query::Nested {
        path: "reviews",
        query: &brand,
        score_mode: JoinScore::None,
    };
    assert_eq!(run(&rooted), Error::Refused(Refusal::WrongScope));
    // Nested inside nested.
    let inner = Query::Nested {
        path: "reviews",
        query: &author,
        score_mode: JoinScore::None,
    };
    let twice = Query::Nested {
        path: "reviews",
        query: &inner,
        score_mode: JoinScore::None,
    };
    assert_eq!(run(&twice), Error::Refused(Refusal::NestedTooDeep));
    // Identity and nested clauses beyond the bound.
    let by_id = Query::Nested {
        path: "reviews",
        query: &Query::Id("p1"),
        score_mode: JoinScore::None,
    };
    assert_eq!(run(&by_id), Error::Refused(Refusal::WrongScope));
    let one = Query::Nested {
        path: "reviews",
        query: &author,
        score_mode: JoinScore::None,
    };
    let five = [&one, &one, &one, &one, &one].map(|_| Query::Nested {
        path: "reviews",
        query: &author,
        score_mode: JoinScore::None,
    });
    let many = Query::Bool {
        must: &five,
        filter: &[],
        should: &[],
        must_not: &[],
        minimum_should_match: 0,
    };
    assert_eq!(run(&many), Error::Limit);
    // A nested field cannot be projected.
    assert_eq!(
        snapshot
            .search(&Query::All, &[], 1, &["reviews"])
            .unwrap_err(),
        Error::Unsupported
    );
    drop(snapshot);

    // Too many objects in one document, and a malformed object.
    let many_objects: Vec<[(&str, Value); 3]> = (0..hsearch_engine::MAX_NESTED_OBJECTS + 1)
        .map(|_| {
            [
                ("author", Value::String("a")),
                ("stars", Value::Integer(1)),
                ("body", Value::String("x")),
            ]
        })
        .collect();
    let slices: Vec<&[(&str, Value)]> = many_objects.iter().map(|o| o.as_slice()).collect();
    let fields = [
        ("brand", Value::String("acme")),
        ("title", Value::String("t")),
        ("reviews", Value::Nested(&slices)),
    ];
    let put = |fields: &[(&str, Value)]| {
        schema.prepare(
            "r",
            &[Mutation::Put {
                id: "x",
                version: 9,
                fields,
            }],
        )
    };
    assert!(matches!(put(&fields), Err(Error::Limit)));
    let swapped = [[
        ("stars", Value::Integer(1)),
        ("author", Value::String("a")),
        ("body", Value::String("x")),
    ]];
    let swapped: Vec<&[(&str, Value)]> = swapped.iter().map(|o| o.as_slice()).collect();
    let fields = [
        ("brand", Value::String("acme")),
        ("title", Value::String("t")),
        ("reviews", Value::Nested(&swapped)),
    ];
    assert!(matches!(put(&fields), Err(Error::Document)));
    // A nested value where a scalar is declared.
    let fields = [
        ("brand", Value::Nested(&[])),
        ("title", Value::String("t")),
        ("reviews", Value::Nested(&[])),
    ];
    assert!(matches!(put(&fields), Err(Error::Document)));
    engine.close().unwrap();
}

#[test]
fn nested_schema_declarations_are_bounded() {
    let base = || Schema::new(1, &[("a", Kind::Keyword)]).unwrap();
    // Nested inside nested has no spelling: an object holds scalars only.
    assert!(base().with_nested("n", &[("x", Kind::Nested)]).is_err());
    assert!(base().with_nested("n", &[]).is_err());
    assert!(
        base()
            .with_nested("n", &[("x", Kind::Keyword), ("x", Kind::Keyword)])
            .is_err()
    );
    assert!(base().with_nested("a", &[("x", Kind::Keyword)]).is_err());
    assert!(base().with_nested("_n", &[("x", Kind::Keyword)]).is_err());
    assert!(Schema::new(1, &[("n", Kind::Nested)]).is_err());
    assert!(Schema::new(1, &[("j", Kind::Join)]).is_err());
    let mut schema = base();
    for i in 0..hsearch_engine::MAX_NESTED_PATHS {
        schema = schema
            .with_nested(&format!("n{i}"), &[("x", Kind::Keyword)])
            .unwrap();
    }
    assert!(
        schema
            .with_nested("one_more", &[("x", Kind::Keyword)])
            .is_err()
    );
    assert!(
        base()
            .with_join("j", "q", &["a1"])
            .unwrap()
            .with_join("k", "q", &["a1"])
            .is_err()
    );
    assert!(base().with_join("j", "q", &["q"]).is_err());
    assert!(base().with_join("j", "q", &[]).is_err());
    assert!(base().with_join("j", "q", &["a", "a"]).is_err());
}

// ---------------------------------------------------------------- join

struct Post {
    id: String,
    /// `None` for a question, else the parent's id.
    parent: Option<String>,
    topic: &'static str,
    votes: i64,
}

const TOPICS: [&str; 4] = ["rust", "search", "storage", "network"];

fn posts(questions: usize, answers: usize, seed: u64) -> Vec<Post> {
    let mut rng = Rng(seed);
    let mut out: Vec<Post> = (0..questions)
        .map(|i| Post {
            id: format!("q{i}"),
            parent: None,
            topic: TOPICS[i % 4],
            votes: 0,
        })
        .collect();
    out.extend((0..answers).map(|i| Post {
        id: format!("a{i}"),
        parent: Some(format!("q{}", rng.below(questions as u64))),
        topic: TOPICS[rng.below(4) as usize],
        votes: rng.below(6) as i64,
    }));
    out
}

fn post_schema() -> Schema {
    Schema::new(1, &[("topic", Kind::Keyword), ("votes", Kind::Integer)])
        .unwrap()
        .with_join("qa", "question", &["answer"])
        .unwrap()
        .with_limits(Limits::scale())
        .unwrap()
}

fn put_posts(engine: &Engine<'_>, schema: &Schema, items: &[Post], version: i64, tag: &str) {
    for (b, chunk) in items.chunks(16).enumerate() {
        let fields: Vec<[(&str, Value); 3]> = chunk
            .iter()
            .map(|p| {
                [
                    ("topic", Value::String(p.topic)),
                    ("votes", Value::Integer(p.votes)),
                    (
                        "qa",
                        Value::Join {
                            relation: if p.parent.is_some() {
                                "answer"
                            } else {
                                "question"
                            },
                            parent: p.parent.as_deref(),
                        },
                    ),
                ]
            })
            .collect();
        let mutations: Vec<Mutation> = chunk
            .iter()
            .zip(&fields)
            .map(|(p, f)| Mutation::Put {
                id: &p.id,
                version,
                fields: f,
            })
            .collect();
        engine
            .apply(
                schema
                    .prepare(&format!("{tag}-{b}-{version}"), &mutations)
                    .unwrap(),
            )
            .unwrap();
    }
}

fn votes_in(lo: i64, hi: i64) -> Query<'static> {
    Query::Range {
        field: "votes",
        lower: Value::Integer(lo),
        upper: Value::Integer(hi),
    }
}

/// Parents whose matching children number between `min` and `max` (0: no bound).
fn parents_with(
    items: &[Post],
    matches: impl Fn(&Post) -> bool,
    min: usize,
    max: usize,
) -> BTreeSet<String> {
    items
        .iter()
        .filter(|p| p.parent.is_none())
        .filter(|q| {
            let n = items
                .iter()
                .filter(|a| a.parent.as_deref() == Some(&q.id) && matches(a))
                .count();
            n >= min && (max == 0 || n <= max)
        })
        .map(|q| q.id.clone())
        .collect()
}

#[test]
fn has_child_and_has_parent_match_the_oracle_across_segments() {
    let schema = post_schema();
    let items = posts(60, 240, 5);
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    // Children first, parents later, in several segments: a join does not depend on order.
    put_posts(&engine, &schema, &items[60..150], 1, "a1");
    drop(engine.refresh().unwrap());
    put_posts(&engine, &schema, &items[..60], 1, "q");
    drop(engine.refresh().unwrap());
    put_posts(&engine, &schema, &items[150..], 1, "a2");
    let snapshot = engine.refresh().unwrap();

    for (lo, hi, min, max) in [
        (4, 5, 1, 0),
        (4, 5, 2, 0),
        (0, 2, 1, 3),
        (5, 5, 1, 1),
        (3, 5, 3, 0),
    ] {
        let inner = votes_in(lo, hi);
        let query = Query::HasChild {
            relation: "answer",
            query: &inner,
            score_mode: JoinScore::Max,
            min_children: min,
            max_children: max,
        };
        let expected = parents_with(&items, |a| (lo..=hi).contains(&a.votes), min, max);
        assert!(!expected.is_empty(), "{lo}..{hi} {min}..{max}");
        assert_eq!(ids(&snapshot, &query), expected, "{lo}..{hi} {min}..{max}");
    }

    for topic in TOPICS {
        let inner = Query::Term {
            field: "topic",
            value: Value::String(topic),
        };
        for score in [false, true] {
            let query = Query::HasParent {
                relation: "question",
                query: &inner,
                score,
            };
            let expected: BTreeSet<String> = items
                .iter()
                .filter(|a| {
                    a.parent.as_ref().is_some_and(|q| {
                        items
                            .iter()
                            .any(|p| &p.id == q && p.parent.is_none() && p.topic == topic)
                    })
                })
                .map(|a| a.id.clone())
                .collect();
            assert!(!expected.is_empty(), "{topic}");
            assert_eq!(ids(&snapshot, &query), expected, "{topic} {score}");
        }
    }

    // A join composes with root conditions, and the relation is a term on the join field.
    let inner = votes_in(5, 5);
    let child = Query::HasChild {
        relation: "answer",
        query: &inner,
        score_mode: JoinScore::None,
        min_children: 1,
        max_children: 0,
    };
    let rust = Query::Term {
        field: "topic",
        value: Value::String("rust"),
    };
    let both = Query::Bool {
        must: &[],
        filter: &[child, rust],
        should: &[],
        must_not: &[],
        minimum_should_match: 0,
    };
    let expected: BTreeSet<String> = parents_with(&items, |a| a.votes == 5, 1, 0)
        .into_iter()
        .filter(|id| items.iter().any(|p| &p.id == id && p.topic == "rust"))
        .collect();
    assert_eq!(ids(&snapshot, &both), expected);
    let questions = Query::Term {
        field: "qa",
        value: Value::String("question"),
    };
    assert_eq!(ids(&snapshot, &questions).len(), 60);
    drop(snapshot);

    // Replace children: the join follows the new documents.
    let mut moved = posts(60, 240, 5);
    for a in moved.iter_mut().filter(|p| p.parent.is_some()) {
        a.votes = 0;
    }
    put_posts(&engine, &schema, &moved[60..], 2, "reset");
    let snapshot = engine.refresh().unwrap();
    let inner = votes_in(1, 5);
    let query = Query::HasChild {
        relation: "answer",
        query: &inner,
        score_mode: JoinScore::None,
        min_children: 1,
        max_children: 0,
    };
    assert!(ids(&snapshot, &query).is_empty());
    drop(snapshot);
    engine.merge().unwrap();
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();
    // Reopening verifies the join values of every document.
    let engine = Engine::open(&observed, schema.clone(), Some(checkpoint)).unwrap();
    let snapshot = engine.refresh().unwrap();
    let inner = votes_in(0, 0);
    let query = Query::HasChild {
        relation: "answer",
        query: &inner,
        score_mode: JoinScore::None,
        min_children: 1,
        max_children: 0,
    };
    assert_eq!(
        ids(&snapshot, &query),
        parents_with(&moved, |a| a.votes == 0, 1, 0)
    );
    drop(snapshot);
    engine.close().unwrap();
}

#[test]
fn a_join_on_a_single_segment_matches_too() {
    let schema = post_schema();
    let items = posts(20, 60, 9);
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    put_posts(&engine, &schema, &items, 1, "all");
    engine.merge().unwrap();
    let snapshot = engine.refresh().unwrap();
    let inner = votes_in(3, 5);
    let query = Query::HasChild {
        relation: "answer",
        query: &inner,
        score_mode: JoinScore::Sum,
        min_children: 1,
        max_children: 0,
    };
    assert_eq!(
        ids(&snapshot, &query),
        parents_with(&items, |a| (3..=5).contains(&a.votes), 1, 0)
    );
    // Nothing in the index is a child of this relation.
    let none = Query::HasChild {
        relation: "answer",
        query: &votes_in(9, 9),
        score_mode: JoinScore::None,
        min_children: 1,
        max_children: 0,
    };
    assert!(ids(&snapshot, &none).is_empty());
    drop(snapshot);
    engine.close().unwrap();
}

#[test]
fn a_child_without_a_routable_parent_is_rejected() {
    let schema = post_schema();
    let put = |relation: &str, parent: Option<&str>| {
        schema.prepare(
            "r",
            &[Mutation::Put {
                id: "x",
                version: 1,
                fields: &[
                    ("topic", Value::String("t")),
                    ("votes", Value::Integer(1)),
                    ("qa", Value::Join { relation, parent }),
                ],
            }],
        )
    };
    assert!(matches!(
        put("answer", None),
        Err(Error::Refused(Refusal::ChildWithoutParent))
    ));
    assert!(matches!(
        put("answer", Some("")),
        Err(Error::Refused(Refusal::ChildWithoutParent))
    ));
    assert!(matches!(
        put("question", Some("q1")),
        Err(Error::Refused(Refusal::ParentNamesParent))
    ));
    assert!(matches!(
        put("comment", Some("q1")),
        Err(Error::Refused(Refusal::UnknownRelation))
    ));
    assert!(put("answer", Some("q1")).is_ok());
    assert!(put("question", None).is_ok());
    // A child whose parent id is too long to be an identity.
    let long = "p".repeat(300);
    assert!(matches!(
        put("answer", Some(&long)),
        Err(Error::Refused(Refusal::ChildWithoutParent))
    ));
    for refusal in [
        Refusal::ChildWithoutParent,
        Refusal::ParentNamesParent,
        Refusal::UnknownRelation,
        Refusal::CrossShardJoin,
        Refusal::NotNested,
        Refusal::NotJoin,
        Refusal::WrongScope,
        Refusal::NestedTooDeep,
    ] {
        assert!(!refusal.reason().is_empty());
    }
}

#[test]
fn parent_and_children_route_together_and_a_join_across_shards_is_refused() {
    let schema = post_schema();
    let question = [
        ("topic", Value::String("t")),
        ("votes", Value::Integer(0)),
        (
            "qa",
            Value::Join {
                relation: "question",
                parent: None,
            },
        ),
    ];
    let answer = [
        ("topic", Value::String("t")),
        ("votes", Value::Integer(0)),
        (
            "qa",
            Value::Join {
                relation: "answer",
                parent: Some("q1"),
            },
        ),
    ];
    // The routing key of a parent is its own id; of a child, its parent's.
    assert_eq!(schema.route_key("q1", &question).unwrap(), "q1");
    assert_eq!(schema.route_key("a9", &answer).unwrap(), "q1");
    assert!(schema.check_route("a9", &answer, "q1").is_ok());
    assert!(schema.check_route("q1", &question, "q1").is_ok());
    // A child placed by its own id would sit on another shard than its parent.
    assert_eq!(
        schema.check_route("a9", &answer, "a9"),
        Err(Error::Refused(Refusal::CrossShardJoin))
    );
    let orphan = [
        ("topic", Value::String("t")),
        ("votes", Value::Integer(0)),
        (
            "qa",
            Value::Join {
                relation: "answer",
                parent: None,
            },
        ),
    ];
    assert_eq!(
        schema.route_key("a9", &orphan),
        Err(Error::Refused(Refusal::ChildWithoutParent))
    );
    // Without a join field a document routes by its own id.
    let plain = Schema::new(1, &[("a", Kind::Keyword)]).unwrap();
    assert_eq!(
        plain.route_key("d1", &[("a", Value::String("x"))]).unwrap(),
        "d1"
    );
    // The inventory names the refusal.
    let factory = EngineFactory::new();
    match factory.support(Capability::CrossShardJoin) {
        Support::Unsupported { reason } => assert!(reason.contains("across shards"), "{reason}"),
        Support::Supported => panic!("a join across shards is not offered"),
    }
    assert_eq!(factory.support(Capability::QueryNested), Support::Supported);
    assert_eq!(
        factory.support(Capability::QueryHasChild),
        Support::Supported
    );
    assert_eq!(
        factory.support(Capability::QueryHasParent),
        Support::Supported
    );
}

#[test]
fn join_queries_that_the_schema_cannot_answer_are_refused() {
    let schema = post_schema();
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    put_posts(&engine, &schema, &posts(4, 8, 2), 1, "p");
    let snapshot = engine.refresh().unwrap();
    let run = |q: &Query<'_>| snapshot.search(q, &[], 10, &[]).unwrap_err();
    let inner = votes_in(0, 5);
    // A relation the join field does not declare, either way round.
    let bad_child = Query::HasChild {
        relation: "question",
        query: &inner,
        score_mode: JoinScore::None,
        min_children: 1,
        max_children: 0,
    };
    assert_eq!(run(&bad_child), Error::Refused(Refusal::NotJoin));
    let bad_parent = Query::HasParent {
        relation: "answer",
        query: &inner,
        score: false,
    };
    assert_eq!(run(&bad_parent), Error::Refused(Refusal::NotJoin));
    // Inconsistent bounds.
    let bounds = Query::HasChild {
        relation: "answer",
        query: &inner,
        score_mode: JoinScore::None,
        min_children: 3,
        max_children: 2,
    };
    assert_eq!(run(&bounds), Error::Limit);
    // More joins than one query may run.
    let join = || Query::HasChild {
        relation: "answer",
        query: &inner,
        score_mode: JoinScore::None,
        min_children: 1,
        max_children: 0,
    };
    let three = [join(), join(), join()];
    let many = Query::Bool {
        must: &three,
        filter: &[],
        should: &[],
        must_not: &[],
        minimum_should_match: 0,
    };
    assert_eq!(run(&many), Error::Limit);
    // The join field is not projectable, and a term on it must be text.
    assert_eq!(
        snapshot.search(&Query::All, &[], 1, &["qa"]).unwrap_err(),
        Error::Unsupported
    );
    let numeric = Query::Term {
        field: "qa",
        value: Value::Integer(1),
    };
    assert_eq!(run(&numeric), Error::Document);
    drop(snapshot);
    engine.close().unwrap();

    // A schema with no join field cannot run one.
    let plain = Schema::new(1, &[("votes", Kind::Integer)]).unwrap();
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &plain);
    engine
        .apply(
            plain
                .prepare(
                    "p",
                    &[Mutation::Put {
                        id: "d",
                        version: 1,
                        fields: &[("votes", Value::Integer(1))],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    let snapshot = engine.refresh().unwrap();
    let query = Query::HasParent {
        relation: "question",
        query: &Query::All,
        score: false,
    };
    assert_eq!(
        snapshot.search(&query, &[], 1, &[]).unwrap_err(),
        Error::Refused(Refusal::NotJoin)
    );
    let nested = Query::Nested {
        path: "reviews",
        query: &Query::All,
        score_mode: JoinScore::None,
    };
    assert_eq!(
        snapshot.search(&nested, &[], 1, &[]).unwrap_err(),
        Error::Refused(Refusal::NotNested)
    );
    drop(snapshot);
    engine.close().unwrap();
}

#[test]
fn nested_and_join_live_in_one_schema() {
    let schema = Schema::new(1, &[("topic", Kind::Keyword)])
        .unwrap()
        .with_nested("tags", &[("name", Kind::Keyword)])
        .unwrap()
        .with_join("qa", "question", &["answer"])
        .unwrap()
        .with_limits(Limits::scale())
        .unwrap();
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    let tag_a = [("name", Value::String("hot"))];
    let tag_b = [("name", Value::String("new"))];
    let tags: [&[(&str, Value)]; 2] = [&tag_a, &tag_b];
    let q_fields = [
        ("topic", Value::String("rust")),
        ("tags", Value::Nested(&tags)),
        (
            "qa",
            Value::Join {
                relation: "question",
                parent: None,
            },
        ),
    ];
    let a_fields = [
        ("topic", Value::String("rust")),
        ("tags", Value::Nested(&[])),
        (
            "qa",
            Value::Join {
                relation: "answer",
                parent: Some("q1"),
            },
        ),
    ];
    engine
        .apply(
            schema
                .prepare(
                    "r",
                    &[
                        Mutation::Put {
                            id: "q1",
                            version: 1,
                            fields: &q_fields,
                        },
                        Mutation::Put {
                            id: "a1",
                            version: 1,
                            fields: &a_fields,
                        },
                    ],
                )
                .unwrap(),
        )
        .unwrap();
    let snapshot = engine.refresh().unwrap();
    assert_eq!(snapshot.search(&Query::All, &[], 0, &[]).unwrap().total, 2);
    let hot = Query::Term {
        field: "tags.name",
        value: Value::String("hot"),
    };
    let nested = Query::Nested {
        path: "tags",
        query: &hot,
        score_mode: JoinScore::None,
    };
    assert_eq!(ids(&snapshot, &nested), BTreeSet::from(["q1".to_string()]));
    // has_parent over a parent chosen by its nested object.
    let child = Query::HasParent {
        relation: "question",
        query: &nested,
        score: false,
    };
    assert_eq!(ids(&snapshot, &child), BTreeSet::from(["a1".to_string()]));
    drop(snapshot);
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();
    let engine = Engine::open(&observed, schema, Some(checkpoint)).unwrap();
    engine.close().unwrap();
}
