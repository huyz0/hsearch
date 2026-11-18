//! The commit-log batch body: a replayed batch is the batch that was written.
use hsearch_engine::{
    Error, Kind, Mutation, Query, Schema, Similarity, Value, VectorSpec, apply_batch, encode_batch,
    with_local_engine,
};

fn schema() -> Schema {
    Schema::new(
        1,
        &[
            ("tag", Kind::Keyword),
            ("body", Kind::Text),
            ("n", Kind::Integer),
            ("ok", Kind::Boolean),
            ("at", Kind::Date),
        ],
    )
    .unwrap()
    .with_nested(
        "reviews",
        &[("author", Kind::Keyword), ("stars", Kind::Integer)],
    )
    .unwrap()
}

fn root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hsearch-wal-{label}-{}", std::process::id()))
}

#[test]
fn a_replayed_batch_gives_the_receipts_and_documents_of_the_live_one() {
    let schema = schema();
    let r1 = [
        ("author", Value::String("amy")),
        ("stars", Value::Integer(5)),
    ];
    let r2 = [
        ("author", Value::String("bo")),
        ("stars", Value::Integer(2)),
    ];
    let objects: [&[(&str, Value<'_>)]; 2] = [&r1, &r2];
    let fields = [
        ("tag", Value::String("t")),
        ("body", Value::String("a quick fox")),
        ("n", Value::Integer(-7)),
        ("ok", Value::Boolean(true)),
        ("at", Value::Date(86_400_000)),
        ("reviews", Value::Nested(&objects)),
    ];
    let empty = [
        ("tag", Value::String("u")),
        ("body", Value::String("")),
        ("n", Value::Integer(0)),
        ("ok", Value::Boolean(false)),
        ("at", Value::Date(0)),
        ("reviews", Value::Nested(&[])),
    ];
    let batches: Vec<Vec<Mutation<'_>>> = vec![
        vec![
            Mutation::Put {
                id: "a",
                version: 1,
                fields: &fields,
            },
            Mutation::Create {
                id: "b",
                version: 1,
                fields: &empty,
            },
            Mutation::Create {
                id: "c",
                version: 1,
                fields: &empty,
            },
        ],
        vec![
            Mutation::Delete {
                id: "a",
                version: 2,
            },
            Mutation::Delete {
                id: "missing",
                version: 3,
            },
            Mutation::Put {
                id: "b",
                version: 4,
                fields: &fields,
            },
        ],
    ];
    let live = with_local_engine(&root("live"), schema.clone(), |engine, _| {
        let receipts: Vec<_> = batches
            .iter()
            .enumerate()
            .map(|(i, b)| {
                engine
                    .apply(schema.prepare(&format!("r{i}"), b).unwrap())
                    .unwrap()
            })
            .collect();
        let snapshot = engine.refresh().unwrap();
        let docs: Vec<_> = ["a", "b", "c", "missing"]
            .iter()
            .map(|id| snapshot.get(id, &["tag", "body", "n", "ok", "at"]).unwrap())
            .collect();
        (receipts, docs)
    })
    .unwrap();
    let replayed = with_local_engine(&root("replay"), schema.clone(), |engine, _| {
        let receipts: Vec<_> = batches
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let body = encode_batch(&format!("r{i}"), b).unwrap();
                apply_batch(&schema, engine, &body).unwrap()
            })
            .collect();
        let snapshot = engine.refresh().unwrap();
        let docs: Vec<_> = ["a", "b", "c", "missing"]
            .iter()
            .map(|id| snapshot.get(id, &["tag", "body", "n", "ok", "at"]).unwrap())
            .collect();
        (receipts, docs)
    })
    .unwrap();
    assert_eq!(live.0, replayed.0);
    assert_eq!(live.1, replayed.1);
    assert!(
        live.1[0].is_none() && live.1[1].is_some() && live.1[2].is_some() && live.1[3].is_none()
    );
}

#[test]
fn a_body_that_does_not_decode_is_refused_whole() {
    let schema = schema();
    let fields = [
        ("tag", Value::String("t")),
        ("body", Value::String("x")),
        ("n", Value::Integer(1)),
        ("ok", Value::Boolean(true)),
        ("at", Value::Date(1)),
        ("reviews", Value::Nested(&[])),
    ];
    let body = encode_batch(
        "r",
        &[Mutation::Put {
            id: "a",
            version: 1,
            fields: &fields,
        }],
    )
    .unwrap();
    with_local_engine(&root("bad"), schema.clone(), |engine, _| {
        // Every strict prefix is a truncated record.
        for cut in 0..body.len() {
            assert_eq!(
                apply_batch(&schema, engine, &body[..cut]).unwrap_err(),
                Error::Document,
                "prefix {cut}"
            );
        }
        let mut trailing = body.clone();
        trailing.push(0);
        assert_eq!(
            apply_batch(&schema, engine, &trailing).unwrap_err(),
            Error::Document
        );
        let mut version = body.clone();
        version[0] = 9;
        assert_eq!(
            apply_batch(&schema, engine, &version).unwrap_err(),
            Error::Document
        );
        // A count larger than the bytes that remain is refused before anything is allocated.
        let mut huge = vec![1u8];
        huge.extend_from_slice(&1u32.to_le_bytes());
        huge.push(b'r');
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            apply_batch(&schema, engine, &huge).unwrap_err(),
            Error::Document
        );
        // Nothing was applied by any of them.
        assert!(engine.refresh().unwrap().get("a", &[]).unwrap().is_none());
        // The intact body still applies.
        assert_eq!(apply_batch(&schema, engine, &body).unwrap().len(), 1);
    })
    .unwrap();
}

#[test]
fn vector_join_and_shared_values_replay_and_a_nested_in_nested_is_not_encoded() {
    let schema = Schema::new(
        1,
        &[
            ("tag", Kind::Keyword),
            ("body", Kind::Text),
            ("v", Kind::Vector(VectorSpec::hnsw(3, Similarity::L2))),
        ],
    )
    .unwrap()
    .with_join("qa", "question", &["answer"])
    .unwrap();
    let shared = std::sync::Arc::new("a shared text".to_string());
    let q = [
        ("tag", Value::String("q")),
        ("body", Value::Shared(&shared)),
        ("v", Value::Vector(&[1.0, 2.0, 3.0])),
        (
            "qa",
            Value::Join {
                relation: "question",
                parent: None,
            },
        ),
    ];
    let a = [
        ("tag", Value::String("a")),
        ("body", Value::String("an answer")),
        ("v", Value::Vector(&[0.5, 0.5, 0.5])),
        (
            "qa",
            Value::Join {
                relation: "answer",
                parent: Some("q1"),
            },
        ),
    ];
    let batch = [
        Mutation::Put {
            id: "q1",
            version: 1,
            fields: &q,
        },
        Mutation::Put {
            id: "a1",
            version: 1,
            fields: &a,
        },
    ];
    let run = |label: &str, replay: bool| {
        with_local_engine(&root(label), schema.clone(), |engine, _| {
            let receipts = if replay {
                apply_batch(&schema, engine, &encode_batch("r", &batch).unwrap()).unwrap()
            } else {
                engine.apply(schema.prepare("r", &batch).unwrap()).unwrap()
            };
            let snapshot = engine.refresh().unwrap();
            let total = snapshot.search(&Query::All, &[], 0, &[]).unwrap().total;
            let docs: Vec<_> = ["q1", "a1"]
                .iter()
                .map(|id| snapshot.get(id, &["tag", "body"]).unwrap())
                .collect();
            (receipts, total, docs)
        })
        .unwrap()
    };
    assert_eq!(run("vj-live", false), run("vj-replay", true));

    // An object nested in an object is refused at encode, as decode and prepare refuse it.
    let inner = [("x", Value::Nested(&[]))];
    let objects: [&[(&str, Value<'_>)]; 1] = [&inner];
    let deep = [("o", Value::Nested(&objects))];
    assert_eq!(
        encode_batch(
            "r",
            &[Mutation::Put {
                id: "d",
                version: 1,
                fields: &deep
            }]
        )
        .unwrap_err(),
        Error::Document
    );

    // A body holding a vector of the wrong dimension is the engine's refusal, not a decode.
    let bad = [
        ("tag", Value::String("q")),
        ("body", Value::String("x")),
        ("v", Value::Vector(&[1.0, 2.0])),
        (
            "qa",
            Value::Join {
                relation: "question",
                parent: None,
            },
        ),
    ];
    with_local_engine(&root("vj-bad"), schema.clone(), |engine, _| {
        let body = encode_batch(
            "r",
            &[Mutation::Put {
                id: "q",
                version: 1,
                fields: &bad,
            }],
        )
        .unwrap();
        assert_eq!(
            apply_batch(&schema, engine, &body).unwrap_err(),
            Error::Document
        );
    })
    .unwrap();
}
