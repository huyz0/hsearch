//! Dense-vector search through the adapter: HNSW plain and quantized, filtered
//! kNN, composition in a bool, deletes, merges and reopening from a checkpoint.
//!
//! Recall is measured against an exact scan of the same corpus. The corpus is
//! synthetic and clustered (see `corpus`); the floors are the ones M02.12
//! froze, and a real-dataset number is reported by the `vector_recall` example.
use hsearch_engine::{
    Engine, EngineDirectory, Error, Kind, Limits, Mutation, Quantizer, Query, ScalarBits, Schema,
    Similarity, Snapshot, Sort, StoredValue, Value, VectorSpec,
};
use lucene_store::ByteBuffersDirectory;

const DIM: usize = 32;
const CATEGORIES: usize = 20;

/// Deterministic xorshift, so every run builds the same corpus.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// Approximately standard normal (sum of twelve uniforms).
    fn normal(&mut self) -> f32 {
        (0..12).map(|_| self.unit()).sum::<f32>() - 6.0
    }
}

/// `n` vectors around 16 centres, so neighbours are not all equally near.
fn corpus(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    let centres: Vec<Vec<f32>> = (0..16)
        .map(|_| (0..dim).map(|_| rng.normal() * 3.0).collect())
        .collect();
    (0..n)
        .map(|i| centres[i % 16].iter().map(|c| c + rng.normal()).collect())
        .collect()
}

fn schema(spec: VectorSpec) -> Schema {
    Schema::new(
        1,
        &[
            ("category", Kind::Keyword),
            ("embedding", Kind::Vector(spec)),
        ],
    )
    .unwrap()
    .with_limits(Limits::scale().with_document_bytes(16_384))
    .unwrap()
}

fn category(i: usize) -> String {
    format!("c{}", i % CATEGORIES)
}

fn load(engine: &Engine<'_>, schema: &Schema, vectors: &[Vec<f32>], first: usize, version: i64) {
    for (b, chunk) in vectors.chunks(16).enumerate() {
        let ids: Vec<String> = (0..chunk.len())
            .map(|j| format!("d{}", first + b * 16 + j))
            .collect();
        let categories: Vec<String> = (0..chunk.len())
            .map(|j| category(first + b * 16 + j))
            .collect();
        let fields: Vec<[(&str, Value); 2]> = chunk
            .iter()
            .zip(&categories)
            .map(|(v, c)| {
                [
                    ("category", Value::String(c)),
                    ("embedding", Value::Vector(v)),
                ]
            })
            .collect();
        let mutations: Vec<Mutation> = ids
            .iter()
            .zip(&fields)
            .map(|(id, f)| Mutation::Put {
                id,
                version,
                fields: f,
            })
            .collect();
        engine
            .apply(
                schema
                    .prepare(&format!("load-{first}-{b}-{version}"), &mutations)
                    .unwrap(),
            )
            .unwrap();
    }
}

fn delete(engine: &Engine<'_>, schema: &Schema, ids: &[String], version: i64) {
    for (b, chunk) in ids.chunks(16).enumerate() {
        let mutations: Vec<Mutation> = chunk
            .iter()
            .map(|id| Mutation::Delete { id, version })
            .collect();
        engine
            .apply(
                schema
                    .prepare(&format!("delete-{b}-{version}"), &mutations)
                    .unwrap(),
            )
            .unwrap();
    }
}

fn search(snapshot: &Snapshot, query: &Query<'_>, size: usize) -> Vec<String> {
    snapshot
        .search(query, &[Sort::Score], size, &[])
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.id)
        .collect()
}

fn knn<'a>(vector: &'a [f32], k: usize, filter: Option<&'a Query<'a>>) -> Query<'a> {
    knn_beam(vector, k, 100, filter)
}

fn knn_beam<'a>(
    vector: &'a [f32],
    k: usize,
    candidates: usize,
    filter: Option<&'a Query<'a>>,
) -> Query<'a> {
    Query::Knn {
        field: "embedding",
        vector,
        k,
        candidates,
        nprobe: 0,
        filter,
    }
}

/// The ids of the `k` nearest of `live` (by Euclidean distance) to `query`.
fn exact(
    corpus: &[Vec<f32>],
    live: impl Fn(usize) -> bool,
    query: &[f32],
    k: usize,
) -> Vec<String> {
    let mut scored: Vec<(f32, usize)> = corpus
        .iter()
        .enumerate()
        .filter(|(i, _)| live(*i))
        .map(|(i, v)| {
            let d: f32 = v.iter().zip(query).map(|(a, b)| (a - b) * (a - b)).sum();
            (d, i)
        })
        .collect();
    scored.sort_by(|a, b| a.partial_cmp(b).unwrap());
    scored
        .iter()
        .take(k)
        .map(|(_, i)| format!("d{i}"))
        .collect()
}

fn recall(
    snapshot: &Snapshot,
    corpus: &[Vec<f32>],
    queries: &[Vec<f32>],
    live: impl Fn(usize) -> bool + Copy,
) -> f64 {
    recall_at(snapshot, corpus, queries, 100, live)
}

fn recall_at(
    snapshot: &Snapshot,
    corpus: &[Vec<f32>],
    queries: &[Vec<f32>],
    candidates: usize,
    live: impl Fn(usize) -> bool + Copy,
) -> f64 {
    let mut found = 0;
    for q in queries {
        let want = exact(corpus, live, q, 10);
        let got = search(snapshot, &knn_beam(q, 10, candidates, None), 10);
        found += want.iter().filter(|id| got.contains(id)).count();
    }
    found as f64 / (10 * queries.len()) as f64
}

fn open<'d>(directory: &'d EngineDirectory<&ByteBuffersDirectory>, schema: &Schema) -> Engine<'d> {
    Engine::open(directory, schema.clone(), None).unwrap()
}

#[test]
fn every_variant_meets_its_recall_floor() {
    let vectors = corpus(3000, DIM, 7);
    let queries = corpus(40, DIM, 99);
    let base = VectorSpec::hnsw(DIM as u16, Similarity::L2);
    // (name, codes kept, candidates searched, recall@10 floor). M02.12 froze
    // 0.95 for float HNSW and 0.90 for quantized variants. The 1-bit codes
    // keep little of a vector's distance, so they are held to the same floor
    // at a wider beam, which is the per-query setting that buys it: this corpus
    // is tight clusters, the hard case for 1-bit codes (at beam 100 they reach
    // 0.65 to 0.85, measured).
    let variants = [
        ("plain", Quantizer::None, 100, 0.95),
        ("scalar8", Quantizer::Scalar(ScalarBits::Eight), 100, 0.90),
        ("scalar7", Quantizer::Scalar(ScalarBits::Seven), 100, 0.90),
        ("scalar4", Quantizer::Scalar(ScalarBits::Four), 100, 0.90),
        ("scalar2", Quantizer::Scalar(ScalarBits::Two), 100, 0.90),
        ("erq4", Quantizer::Erq(4), 100, 0.90),
        ("rabitq", Quantizer::RaBitQ, 200, 0.90),
        ("scalar1", Quantizer::Scalar(ScalarBits::One), 400, 0.90),
        ("bbq", Quantizer::Bbq, 400, 0.90),
    ];
    for (name, quantizer, candidates, floor) in variants {
        let schema = schema(base.with_quantizer(quantizer));
        let directory = ByteBuffersDirectory::new();
        let observed = EngineDirectory::new(&directory);
        let engine = open(&observed, &schema);
        load(&engine, &schema, &vectors, 0, 1);
        let snapshot = engine.refresh().unwrap();
        let measured = recall_at(&snapshot, &vectors, &queries, candidates, |_| true);
        eprintln!("recall@10 {name} at beam {candidates}: {measured:.4}");
        assert!(measured >= floor, "{name}: {measured} below {floor}");
        drop(snapshot);
        engine.close().unwrap();
    }
}

#[test]
fn similarity_functions_rank_by_their_own_measure() {
    // Three points on a line from the query: nearest by every measure is `near`.
    for similarity in [Similarity::L2, Similarity::Cosine, Similarity::DotProduct] {
        let spec = VectorSpec::hnsw(3, similarity);
        let schema = Schema::new(1, &[("embedding", Kind::Vector(spec))]).unwrap();
        let directory = ByteBuffersDirectory::new();
        let observed = EngineDirectory::new(&directory);
        let engine = open(&observed, &schema);
        let points: [(&str, [f32; 3]); 3] = [
            ("far", [0.0, 0.0, 1.0]),
            ("mid", [0.6, 0.0, 0.8]),
            ("near", [1.0, 0.0, 0.0]),
        ];
        let fields: Vec<[(&str, Value); 1]> = points
            .iter()
            .map(|(_, v)| [("embedding", Value::Vector(v))])
            .collect();
        let mutations: Vec<Mutation> = points
            .iter()
            .zip(&fields)
            .map(|((id, _), f)| Mutation::Put {
                id,
                version: 1,
                fields: f,
            })
            .collect();
        engine
            .apply(schema.prepare("points", &mutations).unwrap())
            .unwrap();
        let snapshot = engine.refresh().unwrap();
        let query = [1.0f32, 0.0, 0.0];
        let ids = search(&snapshot, &knn(&query, 3, None), 3);
        assert_eq!(ids, vec!["near", "mid", "far"], "{similarity:?}");
        drop(snapshot);
        engine.close().unwrap();
    }
}

#[test]
fn a_filter_restricts_neighbours_and_still_returns_k() {
    let vectors = corpus(3000, DIM, 11);
    let queries = corpus(20, DIM, 5);
    let schema = schema(VectorSpec::hnsw(DIM as u16, Similarity::L2));
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    load(&engine, &schema, &vectors, 0, 1);
    let snapshot = engine.refresh().unwrap();
    // One category in twenty: a 5% filter, so a post-filter would return ~0.5.
    let in_c3 = |i: usize| i % CATEGORIES == 3;
    let filter = Query::Term {
        field: "category",
        value: Value::String("c3"),
    };
    let mut found = 0;
    for q in &queries {
        let got = search(&snapshot, &knn(q, 10, Some(&filter)), 10);
        assert_eq!(got.len(), 10, "a filtered search must still return k");
        assert!(got.iter().all(|id| in_c3(id[1..].parse().unwrap())));
        let want = exact(&vectors, in_c3, q, 10);
        found += want.iter().filter(|id| got.contains(id)).count();
    }
    let recall = found as f64 / (10 * queries.len()) as f64;
    eprintln!("filtered recall@10 at 5% selectivity: {recall:.4}");
    assert!(recall >= 0.95, "filtered recall {recall}");
    drop(snapshot);
    engine.close().unwrap();
}

#[test]
fn a_filter_matching_fewer_than_k_returns_only_those() {
    let vectors = corpus(500, DIM, 3);
    let schema = schema(VectorSpec::hnsw(DIM as u16, Similarity::L2));
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    load(&engine, &schema, &vectors, 0, 1);
    let snapshot = engine.refresh().unwrap();
    let nothing = Query::Term {
        field: "category",
        value: Value::String("absent"),
    };
    assert!(search(&snapshot, &knn(&vectors[0], 10, Some(&nothing)), 10).is_empty());
    let id = Query::Id("d7");
    assert_eq!(
        search(&snapshot, &knn(&vectors[0], 10, Some(&id)), 10),
        vec!["d7"]
    );
    drop(snapshot);
    engine.close().unwrap();
}

#[test]
fn knn_composes_in_a_bool_with_a_filter_and_a_must_not() {
    let vectors = corpus(1000, DIM, 21);
    let schema = schema(VectorSpec::hnsw(DIM as u16, Similarity::L2));
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    load(&engine, &schema, &vectors, 0, 1);
    let snapshot = engine.refresh().unwrap();
    let q = &vectors[40];
    let want = |keep: &dyn Fn(usize) -> bool| exact(&vectors, keep, q, 10);

    // Neighbours of d40, restricted by a bool filter after the search: the
    // top-k is fixed first, so a bool filter can only remove from it.
    let in_c0 = Query::Term {
        field: "category",
        value: Value::String("c0"),
    };
    let query = Query::Bool {
        must: &[knn(q, 50, None)],
        filter: std::slice::from_ref(&in_c0),
        should: &[],
        must_not: &[],
        minimum_should_match: 0,
    };
    let got = search(&snapshot, &query, 10);
    let top50 = exact(&vectors, |_| true, q, 50);
    let expected: Vec<_> = top50
        .iter()
        .filter(|id| id[1..].parse::<usize>().unwrap() % CATEGORIES == 0)
        .take(10)
        .cloned()
        .collect();
    assert!(!got.is_empty());
    assert!(
        got.iter().all(|id| expected.contains(id)),
        "{got:?} not within {expected:?}"
    );

    // The same neighbours minus one: must_not removes exactly that document.
    let gone = Query::Id("d40");
    let query = Query::Bool {
        must: &[knn(q, 10, None)],
        filter: &[],
        should: &[],
        must_not: std::slice::from_ref(&gone),
        minimum_should_match: 0,
    };
    let got = search(&snapshot, &query, 10);
    assert_eq!(got.len(), 9);
    assert!(!got.contains(&"d40".to_string()));
    let top = want(&|_| true);
    assert!(got.iter().all(|id| top.contains(id)));
    drop(snapshot);
    engine.close().unwrap();
}

#[test]
fn deleted_and_replaced_vectors_stop_matching_before_and_after_a_merge() {
    let vectors = corpus(2000, DIM, 17);
    let queries = corpus(20, DIM, 2);
    let schema = schema(VectorSpec::hnsw(DIM as u16, Similarity::L2));
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    // Three flushes, so the merge below has segments to combine.
    for (n, part) in vectors.chunks(700).enumerate() {
        load(&engine, &schema, part, n * 700, 1);
        drop(engine.refresh().unwrap());
    }
    // Delete a third of the corpus; replace d1 with a vector nowhere near it.
    let dead = |i: usize| i.is_multiple_of(3);
    let ids: Vec<String> = (0..vectors.len())
        .filter(|i| dead(*i))
        .map(|i| format!("d{i}"))
        .collect();
    delete(&engine, &schema, &ids, 2);
    let moved: Vec<f32> = vec![1000.0; DIM];
    let fields = [
        ("category", Value::String("c1")),
        ("embedding", Value::Vector(&moved)),
    ];
    engine
        .apply(
            schema
                .prepare(
                    "move",
                    &[Mutation::Put {
                        id: "d1",
                        version: 2,
                        fields: &fields,
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    let mut live_vectors = vectors.clone();
    live_vectors[1] = moved.clone();
    let live = |i: usize| !dead(i);

    let before = engine.refresh().unwrap();
    for q in &queries {
        let got = search(&before, &knn(q, 10, None), 10);
        assert!(
            got.iter().all(|id| !dead(id[1..].parse().unwrap())),
            "a deleted document was returned"
        );
    }
    let r = recall(&before, &live_vectors, &queries, live);
    eprintln!("recall@10 with a third deleted: {r:.4}");
    assert!(r >= 0.95, "{r}");
    drop(before);

    engine.merge().unwrap();
    let after = engine.refresh().unwrap();
    let r = recall(&after, &live_vectors, &queries, live);
    eprintln!("recall@10 after merge: {r:.4}");
    assert!(r >= 0.95, "{r}");
    for q in &queries {
        let got = search(&after, &knn(q, 10, None), 10);
        assert!(got.iter().all(|id| !dead(id[1..].parse().unwrap())));
    }
    // d1 now sits at 1000.0: it is the nearest document to a query there.
    assert_eq!(search(&after, &knn(&moved, 1, None), 1), vec!["d1"]);
    drop(after);
    engine.close().unwrap();
}

#[test]
fn a_checkpoint_reopens_with_the_same_top_k() {
    let vectors = corpus(1500, DIM, 29);
    let queries = corpus(10, DIM, 31);
    for quantizer in [
        Quantizer::None,
        Quantizer::Scalar(ScalarBits::Eight),
        Quantizer::Scalar(ScalarBits::Seven),
        Quantizer::Scalar(ScalarBits::Four),
        Quantizer::Scalar(ScalarBits::Two),
        Quantizer::Scalar(ScalarBits::One),
        Quantizer::Bbq,
        Quantizer::RaBitQ,
        Quantizer::Erq(4),
    ] {
        let schema = schema(VectorSpec::hnsw(DIM as u16, Similarity::L2).with_quantizer(quantizer));
        let directory = ByteBuffersDirectory::new();
        let observed = EngineDirectory::new(&directory);
        let engine = open(&observed, &schema);
        load(&engine, &schema, &vectors, 0, 1);
        let snapshot = engine.refresh().unwrap();
        let first: Vec<_> = queries
            .iter()
            .map(|q| search(&snapshot, &knn(q, 10, None), 10))
            .collect();
        drop(snapshot);
        let checkpoint = engine.commit().unwrap();
        engine.close().unwrap();

        // Reopening verifies every stored vector against the retry digests.
        let engine = Engine::open(&observed, schema.clone(), Some(checkpoint)).unwrap();
        let snapshot = engine.refresh().unwrap();
        let second: Vec<_> = queries
            .iter()
            .map(|q| search(&snapshot, &knn(q, 10, None), 10))
            .collect();
        assert_eq!(first, second, "{quantizer:?}");
        drop(snapshot);
        engine.close().unwrap();

        // A search role opens the committed files with no writer at all.
        let committed = Snapshot::open_committed(&observed, schema.clone()).unwrap();
        let third: Vec<_> = queries
            .iter()
            .map(|q| search(&committed, &knn(q, 10, None), 10))
            .collect();
        assert_eq!(first, third, "{quantizer:?}");
    }
}

#[test]
fn bad_vectors_and_unsupported_uses_are_refused() {
    let schema = schema(VectorSpec::hnsw(4, Similarity::L2));
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    let put = |vector: &[f32]| {
        let fields = [
            ("category", Value::String("c")),
            ("embedding", Value::Vector(vector)),
        ];
        schema
            .prepare(
                "bad",
                &[Mutation::Put {
                    id: "x",
                    version: 1,
                    fields: &fields,
                }],
            )
            .map(|_| ())
    };
    assert_eq!(put(&[1.0, 2.0, 3.0]), Err(Error::Document));
    assert_eq!(put(&[1.0, 2.0, 3.0, 4.0, 5.0]), Err(Error::Document));
    assert_eq!(put(&[1.0, f32::NAN, 3.0, 4.0]), Err(Error::Document));
    assert_eq!(put(&[1.0, f32::INFINITY, 3.0, 4.0]), Err(Error::Document));
    assert_eq!(put(&[1.0, 2.0, 3.0, 4.0]), Ok(()));

    let fields = [
        ("category", Value::String("c")),
        ("embedding", Value::Vector(&[1.0, 2.0, 3.0, 4.0])),
    ];
    engine
        .apply(
            schema
                .prepare(
                    "one",
                    &[Mutation::Put {
                        id: "x",
                        version: 1,
                        fields: &fields,
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    let snapshot = engine.refresh().unwrap();
    let ok = [0.0f32; 4];
    // Wrong dimension, non-finite, unknown field, a non-vector field, k bounds.
    for (query, expected) in [
        (knn(&[0.0; 3], 1, None), Error::Document),
        (knn(&[0.0, f32::NAN, 0.0, 0.0], 1, None), Error::Document),
        (knn(&ok, 0, None), Error::Limit),
        (knn(&ok, hsearch_engine::MAX_KNN_K + 1, None), Error::Limit),
        (
            Query::Knn {
                field: "embedding",
                vector: &ok,
                k: 1,
                candidates: hsearch_engine::MAX_KNN_CANDIDATES + 1,
                nprobe: 0,
                filter: None,
            },
            Error::Limit,
        ),
        (
            Query::Knn {
                field: "category",
                vector: &ok,
                k: 1,
                candidates: 1,
                nprobe: 0,
                filter: None,
            },
            Error::Unsupported,
        ),
        (
            Query::Knn {
                field: "nope",
                vector: &ok,
                k: 1,
                candidates: 1,
                nprobe: 0,
                filter: None,
            },
            Error::Schema,
        ),
        (
            Query::Term {
                field: "embedding",
                value: Value::Vector(&ok),
            },
            Error::Unsupported,
        ),
    ] {
        assert_eq!(
            snapshot.search(&query, &[], 1, &[]).map(|_| ()),
            Err(expected),
            "case for {expected:?}"
        );
    }
    // The vector is indexed, not stored: it cannot be projected or sorted on.
    assert_eq!(
        snapshot
            .search(&Query::All, &[], 1, &["embedding"])
            .map(|_| ()),
        Err(Error::Unsupported)
    );
    assert_eq!(
        snapshot
            .search(
                &Query::All,
                &[Sort::Field {
                    name: "embedding",
                    descending: false
                }],
                1,
                &[]
            )
            .map(|_| ()),
        Err(Error::Unsupported)
    );
    // The other fields of a document that has a vector read back as usual.
    let hit = snapshot.get("x", &["category"]).unwrap().unwrap();
    assert_eq!(
        hit.fields,
        vec![("category".to_string(), StoredValue::String("c".into()))]
    );
    drop(snapshot);
    engine.close().unwrap();
}

#[test]
fn schema_rejects_malformed_vector_declarations() {
    let ok = VectorSpec::hnsw(8, Similarity::Cosine);
    assert!(Schema::new(1, &[("v", Kind::Vector(ok))]).is_ok());
    for bad in [
        VectorSpec::hnsw(0, Similarity::L2),
        VectorSpec::hnsw(hsearch_engine::MAX_DIMENSION as u16 + 1, Similarity::L2),
        ok.with_graph(0, 100),
        ok.with_graph(16, 0),
        ok.with_quantizer(Quantizer::Erq(9)),
    ] {
        assert_eq!(
            Schema::new(1, &[("v", Kind::Vector(bad))]),
            Err(Error::Schema)
        );
    }
    let many: Vec<(String, Kind)> = (0..=hsearch_engine::MAX_VECTOR_FIELDS)
        .map(|i| (format!("v{i}"), Kind::Vector(ok)))
        .collect();
    let refs: Vec<(&str, Kind)> = many.iter().map(|(n, k)| (n.as_str(), *k)).collect();
    assert_eq!(Schema::new(1, &refs), Err(Error::Schema));
}

#[test]
fn a_batch_of_large_vectors_is_charged_at_their_real_size() {
    // 16 documents of 2,000 components are 128 KB: over the batch bound, though
    // each document alone is within its own.
    let schema = schema(VectorSpec::hnsw(2000, Similarity::L2));
    let vector = vec![0.5f32; 2000];
    let ids: Vec<String> = (0..16).map(|i| format!("d{i}")).collect();
    let fields = [
        ("category", Value::String("c")),
        ("embedding", Value::Vector(&vector)),
    ];
    let mutations: Vec<Mutation> = ids
        .iter()
        .map(|id| Mutation::Put {
            id,
            version: 1,
            fields: &fields,
        })
        .collect();
    assert!(matches!(
        schema.prepare("big", &mutations),
        Err(Error::Limit)
    ));
    assert!(schema.prepare("small", &mutations[..4]).is_ok());
}

#[test]
fn a_query_may_run_only_a_few_knn_searches() {
    let schema = schema(VectorSpec::hnsw(4, Similarity::L2));
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = open(&observed, &schema);
    let snapshot = engine.refresh().unwrap();
    let v = [0.0f32; 4];
    let searches: Vec<Query> = (0..5).map(|_| knn(&v, 1, None)).collect();
    let bool_of = |n: usize| Query::Bool {
        must: &[],
        filter: &[],
        should: &searches[..n],
        must_not: &[],
        minimum_should_match: 1,
    };
    assert!(snapshot.search(&bool_of(4), &[], 1, &[]).is_ok());
    assert!(matches!(
        snapshot.search(&bool_of(5), &[], 1, &[]),
        Err(Error::Limit)
    ));
    drop(snapshot);
    engine.close().unwrap();
}
