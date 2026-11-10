//! Typed conversion boundaries, exact payload projection, and query controls.
use hsearch_engine::{Engine, Error, Kind, Mutation, Query, Schema, Sort, StoredValue, Value};
use lucene_store::ByteBuffersDirectory;

#[test]
fn all_types_sort_projection_empty_analysis_and_boolean_roles() {
    let schema = Schema::new(
        1,
        &[
            ("tag", Kind::Keyword),
            ("title", Kind::Text),
            ("number", Kind::Integer),
            ("active", Kind::Boolean),
            ("date", Kind::Date),
        ],
    )
    .unwrap();
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    for (id, n, active, date) in [
        ("a", i64::from(i32::MIN), false, -62_167_219_200_000),
        ("b", i64::from(i32::MAX), true, 253_402_300_799_999),
    ] {
        engine
            .apply(
                schema
                    .prepare(
                        id,
                        &[Mutation::Put {
                            id,
                            version: 1,
                            fields: &[
                                ("tag", Value::String(id)),
                                ("title", Value::String("Brown fox")),
                                ("number", Value::Integer(n)),
                                ("active", Value::Boolean(active)),
                                ("date", Value::Date(date)),
                            ],
                        }],
                    )
                    .unwrap(),
            )
            .unwrap();
    }
    let snapshot = engine.refresh().unwrap();
    let all = snapshot
        .search(
            &Query::All,
            &[Sort::Field {
                name: "active",
                descending: true,
            }],
            128,
            &["tag", "title", "number", "active", "date"],
        )
        .unwrap();
    assert_eq!(all.total, 2);
    assert_eq!(all.hits[0].id, "b");
    assert_eq!(
        all.hits[0].fields,
        vec![
            ("tag".into(), StoredValue::String("b".into())),
            ("title".into(), StoredValue::String("Brown fox".into())),
            ("number".into(), StoredValue::Integer(i64::from(i32::MAX))),
            ("active".into(), StoredValue::Boolean(true)),
            ("date".into(), StoredValue::Date(253_402_300_799_999))
        ]
    );
    for phrase in [false, true] {
        assert_eq!(
            snapshot
                .search(
                    &Query::Match {
                        field: "title",
                        text: "",
                        phrase
                    },
                    &[],
                    128,
                    &[]
                )
                .unwrap()
                .total,
            0
        );
    }
    let only_a = Query::Bool {
        must: &[],
        filter: &[],
        should: &[],
        must_not: &[Query::Term {
            field: "tag",
            value: Value::String("b"),
        }],
        minimum_should_match: 0,
    };
    assert_eq!(
        snapshot.search(&only_a, &[], 128, &[]).unwrap().hits[0].id,
        "a"
    );
    let filter_and_optional = Query::Bool {
        must: &[],
        filter: &[Query::All],
        should: &[Query::Term {
            field: "tag",
            value: Value::String("b"),
        }],
        must_not: &[],
        minimum_should_match: 0,
    };
    assert_eq!(
        snapshot
            .search(&filter_and_optional, &[], 128, &[])
            .unwrap()
            .total,
        2
    );
    assert_eq!(
        snapshot
            .search(
                &Query::Range {
                    field: "date",
                    lower: Value::Date(-62_167_219_200_000),
                    upper: Value::Date(-62_167_219_200_000)
                },
                &[],
                128,
                &[]
            )
            .unwrap()
            .hits[0]
            .id,
        "a"
    );
    assert_eq!(
        snapshot
            .search(
                &Query::Term {
                    field: "number",
                    value: Value::Integer(i64::from(i32::MAX))
                },
                &[],
                128,
                &[]
            )
            .unwrap()
            .hits[0]
            .id,
        "b"
    );
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();
    let reopened = Engine::open(&observed_directory, schema.clone(), Some(checkpoint)).unwrap();
    assert_eq!(
        reopened
            .refresh()
            .unwrap()
            .search(
                &Query::All,
                &[Sort::Field {
                    name: "active",
                    descending: true
                }],
                128,
                &["tag", "title", "number", "active", "date"]
            )
            .unwrap(),
        all
    );
    for value in [
        Value::Integer(i64::from(i32::MAX) + 1),
        Value::Date(253_402_300_800_000),
        Value::Date(-62_167_219_200_001),
    ] {
        let field = if matches!(value, Value::Integer(_)) {
            "number"
        } else {
            "date"
        };
        assert_eq!(
            snapshot.search(&Query::Term { field, value }, &[], 1, &[]),
            Err(Error::Document)
        );
    }
    reopened.close().unwrap();
}

#[test]
fn query_nodes_terms_depth_bytes_and_numeric_bounds_refuse_explicitly() {
    let schema = Schema::new(1, &[("tag", Kind::Keyword), ("number", Kind::Integer)]).unwrap();
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let engine = Engine::open(&observed_directory, schema, None).unwrap();
    let snapshot = engine.refresh().unwrap();
    let values = vec![Value::String("x"); 129];
    let clauses: Vec<_> = (0..129).map(|_| Query::All).collect();
    let deep1 = [Query::All];
    let deep2 = [boolean(&deep1)];
    let deep3 = [boolean(&deep2)];
    let deep4 = [boolean(&deep3)];
    let deep5 = [boolean(&deep4)];
    let deep6 = [boolean(&deep5)];
    let deep7 = [boolean(&deep6)];
    let deep8 = [boolean(&deep7)];
    let deep9 = [boolean(&deep8)];
    for query in [
        Query::Terms {
            field: "tag",
            values: &values,
        },
        boolean(&clauses),
        boolean(&deep9),
    ] {
        assert_eq!(snapshot.search(&query, &[], 1, &[]), Err(Error::Limit));
    }
    let huge = "x".repeat(8193);
    assert_eq!(
        snapshot.search(
            &Query::Term {
                field: "tag",
                value: Value::String(&huge)
            },
            &[],
            1,
            &[]
        ),
        Err(Error::Limit)
    );
    assert_eq!(
        snapshot
            .search(
                &Query::Range {
                    field: "number",
                    lower: Value::Integer(2),
                    upper: Value::Integer(1)
                },
                &[],
                1,
                &[]
            )
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        snapshot.search(
            &Query::Range {
                field: "tag",
                lower: Value::String("a"),
                upper: Value::String("z")
            },
            &[],
            1,
            &[]
        ),
        Err(Error::Unsupported)
    );
    assert_eq!(
        snapshot.search(
            &Query::Match {
                field: "tag",
                text: "x",
                phrase: false
            },
            &[],
            1,
            &[]
        ),
        Err(Error::Unsupported)
    );
    engine.close().unwrap();
}
fn boolean<'a>(must: &'a [Query<'a>]) -> Query<'a> {
    Query::Bool {
        must,
        filter: &[],
        should: &[],
        must_not: &[],
        minimum_should_match: 0,
    }
}

#[test]
fn retained_reader_budget_is_released_on_drop() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = Schema::new(1, &[("key", Kind::Keyword)]).unwrap();
    let engine = Engine::open(&observed_directory, schema, None).unwrap();
    let a = engine.refresh().unwrap();
    let b = engine.refresh().unwrap();
    let c = engine.refresh().unwrap();
    let d = engine.refresh().unwrap();
    assert!(matches!(engine.refresh(), Err(Error::Limit)));
    drop(a);
    let replacement = engine.refresh().unwrap();
    engine.close().unwrap();
    for snapshot in [b, c, d, replacement] {
        assert_eq!(snapshot.search(&Query::All, &[], 1, &[]).unwrap().total, 0);
    }
}

#[test]
fn empty_terms_and_impossible_boolean_threshold_match_nothing() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = Schema::new(1, &[("key", Kind::Keyword)]).unwrap();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "one",
                    &[Mutation::Put {
                        id: "one",
                        version: 1,
                        fields: &[("key", Value::String("x"))],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    let snapshot = engine.refresh().unwrap();
    for query in [
        Query::Terms {
            field: "key",
            values: &[],
        },
        Query::Bool {
            must: &[],
            filter: &[],
            should: &[Query::All],
            must_not: &[],
            minimum_should_match: 2,
        },
    ] {
        assert_eq!(snapshot.search(&query, &[], 1, &[]).unwrap().total, 0);
    }
    engine.close().unwrap();
}
