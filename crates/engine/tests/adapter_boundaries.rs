//! Exact finite admission boundaries and source-backed analyzer controls.
use hsearch_engine::{Engine, Error, Kind, Mutation, Query, Schema, Value, analyze_text};
use lucene_store::ByteBuffersDirectory;

#[test]
fn empty_terms_validate_unknown_fields_and_charge_empty_field_bytes() {
    let name = "x".repeat(64);
    let schema = Schema::new(1, &[(&name, Kind::Keyword)]).unwrap();
    let directory = ByteBuffersDirectory::new();
    let engine = Engine::open(&directory, schema, None).unwrap();
    let snapshot = engine.refresh().unwrap();
    assert_eq!(
        snapshot.search(
            &Query::Terms {
                field: "unknown",
                values: &[]
            },
            &[],
            1,
            &[]
        ),
        Err(Error::Schema)
    );
    assert_eq!(
        snapshot.metrics(
            &Query::Terms {
                field: "unknown",
                values: &[]
            },
            &[&name]
        ),
        Err(Error::Schema)
    );
    assert_eq!(
        snapshot.groups(
            &Query::Terms {
                field: "unknown",
                values: &[]
            },
            &name,
            1
        ),
        Err(Error::Schema)
    );
    assert_eq!(
        snapshot
            .search(
                &Query::Terms {
                    field: &name,
                    values: &[]
                },
                &[],
                1,
                &[]
            )
            .unwrap()
            .total,
        0
    );
    let value = "a".repeat(7200);
    let mut children = vec![Query::Term {
        field: &name,
        value: Value::String(&value),
    }];
    children.extend((0..16).map(|_| Query::Terms {
        field: &name,
        values: &[],
    }));
    let query = Query::Bool {
        must: &children,
        filter: &[],
        should: &[],
        must_not: &[],
        minimum_should_match: 0,
    };
    assert_eq!(snapshot.search(&query, &[], 1, &[]), Err(Error::Limit));
    engine.close().unwrap();
}
#[test]
fn schema_identity_document_and_aggregate_token_limits_have_exact_edges() {
    let names: Vec<_> = (0..15).map(|i| format!("field_{i}")).collect();
    let fields: Vec<_> = names
        .iter()
        .map(|name| (name.as_str(), Kind::Keyword))
        .collect();
    assert!(Schema::new(1, &fields[..14]).is_ok());
    assert_eq!(Schema::new(1, &fields), Err(Error::Schema));
    let name64 = "x".repeat(64);
    let name65 = "x".repeat(65);
    assert!(Schema::new(1, &[(&name64, Kind::Keyword)]).is_ok());
    assert_eq!(
        Schema::new(1, &[(&name65, Kind::Keyword)]),
        Err(Error::Schema)
    );
    let schema = Schema::new(1, &[("key", Kind::Keyword)]).unwrap();
    let id256 = "i".repeat(256);
    let id257 = "i".repeat(257);
    let request256 = "r".repeat(256);
    let request257 = "r".repeat(257);
    assert!(
        schema
            .prepare(
                &request256,
                &[Mutation::Delete {
                    id: &id256,
                    version: 1
                }]
            )
            .is_ok()
    );
    assert!(matches!(
        schema.prepare(
            &request257,
            &[Mutation::Delete {
                id: "id",
                version: 1
            }]
        ),
        Err(Error::Limit)
    ));
    assert!(matches!(
        schema.prepare(
            "r",
            &[Mutation::Delete {
                id: &id257,
                version: 1
            }]
        ),
        Err(Error::Document)
    ));
    let exact = "x".repeat(8188);
    let overflow = "x".repeat(8189);
    assert!(
        schema
            .prepare(
                "r",
                &[Mutation::Put {
                    id: "i",
                    version: 1,
                    fields: &[("key", Value::String(&exact))]
                }]
            )
            .is_ok()
    );
    assert!(matches!(
        schema.prepare(
            "r",
            &[Mutation::Put {
                id: "i",
                version: 1,
                fields: &[("key", Value::String(&overflow))]
            }]
        ),
        Err(Error::Limit)
    ));
    let schema = Schema::new(1, &[("first", Kind::Text), ("second", Kind::Text)]).unwrap();
    let a = "word ".repeat(64);
    let b = "word ".repeat(65);
    assert!(
        schema
            .prepare(
                "r",
                &[Mutation::Put {
                    id: "i",
                    version: 1,
                    fields: &[("first", Value::String(&a)), ("second", Value::String(&a))]
                }]
            )
            .is_ok()
    );
    assert!(matches!(
        schema.prepare(
            "r",
            &[Mutation::Put {
                id: "i",
                version: 1,
                fields: &[("first", Value::String(&a)), ("second", Value::String(&b))]
            }]
        ),
        Err(Error::Limit)
    ));
}
#[test]
fn history_byte_exhaustion_refuses_before_overwriting_the_last_admitted_version() {
    let directory = ByteBuffersDirectory::new();
    let schema = Schema::new(1, &[("key", Kind::Keyword)]).unwrap();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    let payload = "p".repeat(8000);
    for version in 1..=32 {
        engine
            .apply(
                schema
                    .prepare(
                        &format!("request-{version}"),
                        &[Mutation::Put {
                            id: "stable",
                            version,
                            fields: &[("key", Value::String(&payload))],
                        }],
                    )
                    .unwrap(),
            )
            .unwrap();
    }
    let last = engine.refresh().unwrap();
    assert_eq!(
        engine.apply(
            schema
                .prepare(
                    "request-33",
                    &[Mutation::Put {
                        id: "stable",
                        version: 33,
                        fields: &[("key", Value::String(&payload))]
                    }]
                )
                .unwrap()
        ),
        Err(Error::Limit)
    );
    assert_eq!(
        engine
            .refresh()
            .unwrap()
            .get("stable", &[])
            .unwrap()
            .unwrap()
            .version,
        32
    );
    assert_eq!(last.get("stable", &[]).unwrap().unwrap().version, 32);
    engine.close().unwrap();
}
#[test]
fn repeated_refresh_segment_limit_refuses_until_explicit_merge() {
    let directory = ByteBuffersDirectory::new();
    let schema = Schema::new(1, &[("key", Kind::Keyword)]).unwrap();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    let mut accepted = 0;
    let mut refused = false;
    for version in 1..=64 {
        let batch = schema
            .prepare(
                &format!("request-{version}"),
                &[Mutation::Put {
                    id: "stable",
                    version,
                    fields: &[("key", Value::String("value"))],
                }],
            )
            .unwrap();
        match engine.apply(batch) {
            Ok(_) => {
                accepted = version;
                drop(engine.refresh().unwrap());
            }
            Err(Error::Limit) => {
                refused = true;
                break;
            }
            Err(error) => panic!("unexpected admission failure {error}"),
        }
    }
    assert!(refused);
    assert!((1..=32).contains(&accepted));
    let old = engine.refresh().unwrap();
    engine.merge().unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "after-merge",
                    &[Mutation::Put {
                        id: "stable",
                        version: accepted + 1,
                        fields: &[("key", Value::String("new"))],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    assert_eq!(old.get("stable", &[]).unwrap().unwrap().version, accepted);
    assert_eq!(
        engine
            .refresh()
            .unwrap()
            .get("stable", &[])
            .unwrap()
            .unwrap()
            .version,
        accepted + 1
    );
    engine.close().unwrap();
}
#[test]
fn reader_quota_is_shared_across_checkpoint_reopen() {
    let directory = ByteBuffersDirectory::new();
    let schema = Schema::new(1, &[("key", Kind::Keyword)]).unwrap();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    let checkpoint = engine.commit().unwrap();
    let a = engine.refresh().unwrap();
    let b = engine.refresh().unwrap();
    let c = engine.refresh().unwrap();
    let d = engine.refresh().unwrap();
    engine.close().unwrap();
    let engine = Engine::open(&directory, schema, Some(checkpoint)).unwrap();
    assert!(matches!(engine.refresh(), Err(Error::Limit)));
    drop(a);
    let replacement = engine.refresh().unwrap();
    for snapshot in [b, c, d, replacement] {
        assert_eq!(snapshot.search(&Query::All, &[], 1, &[]).unwrap().total, 0);
    }
    engine.close().unwrap();
}
#[test]
fn standard_analyzer_255_boundary_and_supplementary_offsets_are_explicit() {
    // Pinned standard/mod.rs:55-65: length255 is the scanner buffer; ASCII
    // runs split, while a supplementary character straddling its end can skip.
    let a = "A".repeat(255);
    let b = "A".repeat(256);
    let exact = analyze_text(&a).unwrap();
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].term, "a".repeat(255));
    assert_eq!(
        (
            exact[0].position,
            exact[0].start_offset,
            exact[0].end_offset
        ),
        (0, 0, 255)
    );
    let split = analyze_text(&b).unwrap();
    assert_eq!(
        split
            .iter()
            .map(|t| (t.term.len(), t.position, t.start_offset, t.end_offset))
            .collect::<Vec<_>>(),
        vec![(255, 0, 0, 255), (1, 1, 255, 256)]
    );
    let unicode = analyze_text("A 😀 Z").unwrap();
    assert_eq!(
        unicode
            .iter()
            .map(|t| (t.term.as_str(), t.position, t.start_offset, t.end_offset))
            .collect::<Vec<_>>(),
        vec![("a", 0, 0, 1), ("😀", 1, 2, 4), ("z", 2, 5, 6)]
    );
}
#[test]
fn phrase_lowering_preserves_analyzer_position_holes() {
    let skipped = format!("{}𐐀", "a".repeat(254));
    let text = format!("brown {skipped} fox");
    let tokens = analyze_text(&text).unwrap();
    assert_eq!(
        tokens
            .iter()
            .map(|t| (t.term.as_str(), t.position))
            .collect::<Vec<_>>(),
        vec![("brown", 0), ("fox", 2)]
    );
    let directory = ByteBuffersDirectory::new();
    let schema = Schema::new(1, &[("title", Kind::Text)]).unwrap();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    for (id, title) in [("adjacent", "brown fox"), ("gap", "brown red fox")] {
        engine
            .apply(
                schema
                    .prepare(
                        id,
                        &[Mutation::Put {
                            id,
                            version: 1,
                            fields: &[("title", Value::String(title))],
                        }],
                    )
                    .unwrap(),
            )
            .unwrap();
    }
    let snapshot = engine.refresh().unwrap();
    let result = snapshot
        .search(
            &Query::Match {
                field: "title",
                text: &text,
                phrase: true,
            },
            &[],
            128,
            &[],
        )
        .unwrap();
    assert_eq!(
        result
            .hits
            .iter()
            .map(|h| h.id.as_str())
            .collect::<Vec<_>>(),
        vec!["gap"]
    );
    let adjacent = snapshot
        .search(
            &Query::Match {
                field: "title",
                text: "brown fox",
                phrase: true,
            },
            &[],
            128,
            &[],
        )
        .unwrap();
    assert_eq!(
        adjacent
            .hits
            .iter()
            .map(|h| h.id.as_str())
            .collect::<Vec<_>>(),
        vec!["adjacent"]
    );
    engine.close().unwrap();
}
