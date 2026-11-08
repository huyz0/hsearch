//! Exact finite admission boundaries and source-backed analyzer controls.
use hsearch_engine::{Engine, Error, Kind, Mutation, Query, Schema, Value, analyze_text};
use lucene_search::directory_reader::DirectoryReader;
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
    let prepare = |id: &str| {
        schema
            .prepare(
                id,
                &[Mutation::Create {
                    id,
                    version: 1,
                    fields: &[("key", Value::String("value"))],
                }],
            )
            .unwrap()
    };
    // Each refresh flushes one distinct live document. Replacing a single ID
    // instead lets native delete application drop its fully deleted segments
    // (pinned index_writer.rs:11110-11128), even without an auto-merge policy.
    // The unchanged adapter admission guard reserves two slots below its
    // 32-segment ceiling, so the 32nd one-document batch must be refused.
    for number in 1..=31 {
        engine.apply(prepare(&format!("id-{number}"))).unwrap();
        drop(engine.refresh().unwrap());
    }
    assert_eq!(engine.apply(prepare("id-32")), Err(Error::Limit));
    engine.commit().unwrap();
    assert_eq!(
        DirectoryReader::open(&directory)
            .unwrap()
            .segment_readers()
            .len(),
        31
    );
    assert_eq!(engine.apply(prepare("id-32")), Err(Error::Limit));
    let old = engine.refresh().unwrap();
    assert_eq!(old.search(&Query::All, &[], 128, &[]).unwrap().total, 31);
    assert!(old.get("id-32", &[]).unwrap().is_none());

    engine.merge().unwrap();
    engine.commit().unwrap();
    assert_eq!(
        DirectoryReader::open(&directory)
            .unwrap()
            .segment_readers()
            .len(),
        1
    );
    // Retry the exact refused logical mutation after explicit maintenance.
    // A rejection must not consume its identity or poison continuation.
    engine.apply(prepare("id-32")).unwrap();
    let current = engine.refresh().unwrap();
    assert_eq!(current.search(&Query::All, &[], 128, &[]).unwrap().total, 32);
    assert_eq!(current.get("id-32", &[]).unwrap().unwrap().version, 1);
    assert_eq!(old.search(&Query::All, &[], 128, &[]).unwrap().total, 31);
    assert_eq!(old.get("id-1", &[]).unwrap().unwrap().version, 1);
    assert!(old.get("id-32", &[]).unwrap().is_none());
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
    // Pinned Java scanner blob 82203ff65324263dc40ae4c4f2214b7af7cea202:
    // the fixed 255-unit buffer splits long runs and holds a final high
    // surrogate for the next token. It does not skip that boundary token.
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
fn standard_analyzer_supplementary_buffer_boundaries_follow_pinned_java_source() {
    // Lucene 10.5.1 commit 64ce863a2bea79c69c19c4d56268c26710ff0ff9,
    // StandardTokenizerImpl.java:761-813: a held high surrogate occupies
    // buffer capacity, so 254 ASCII letters finish before its pair is read.
    // 253 letters plus the pair fit; 255 letters fill the buffer alone.
    // U+10400 -> U+10428 is also recorded in OpenJDK UnicodeData.txt at
    // 6c48f4ed707bf0b15f9b6098de30db8aae6fa40f.
    // These are source-derived oracles; this case has no Java runtime receipt.
    for (letters, expected) in [
        (
            253,
            vec![
                ("brown".to_string(), 0, 0, 5),
                (format!("{}\u{10428}", "a".repeat(253)), 1, 6, 261),
                ("fox".to_string(), 2, 262, 265),
            ],
        ),
        (
            254,
            vec![
                ("brown".to_string(), 0, 0, 5),
                ("a".repeat(254), 1, 6, 260),
                ("\u{10428}".to_string(), 2, 260, 262),
                ("fox".to_string(), 3, 263, 266),
            ],
        ),
        (
            255,
            vec![
                ("brown".to_string(), 0, 0, 5),
                ("a".repeat(255), 1, 6, 261),
                ("\u{10428}".to_string(), 2, 261, 263),
                ("fox".to_string(), 3, 264, 267),
            ],
        ),
    ] {
        let text = format!("brown {}\u{10400} fox", "a".repeat(letters));
        let tokens = analyze_text(&text).unwrap();
        let actual: Vec<_> = tokens
            .into_iter()
            .map(|t| (t.term, t.position, t.start_offset, t.end_offset))
            .collect();
        assert_eq!(actual, expected, "ASCII prefix length {letters}");
    }
}

#[test]
fn phrase_match_roundtrips_supplementary_buffer_boundary() {
    let text = format!("brown {}\u{10400} fox", "a".repeat(254));
    let directory = ByteBuffersDirectory::new();
    let schema = Schema::new(1, &[("title", Kind::Text)]).unwrap();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    for (id, title) in [
        ("exact", text.as_str()),
        ("adjacent", "brown fox"),
        ("gap", "brown red fox"),
        ("reversed", "fox brown"),
    ] {
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
        vec!["exact"]
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
