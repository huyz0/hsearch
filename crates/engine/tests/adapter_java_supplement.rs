//! Generated hash-frozen Java/native differential observations; no execution claim.
use hsearch_engine::{
    Engine, Kind, Mutation, Query, Schema, Snapshot, Sort, StoredValue, Value, analyze_text,
};
use lucene_store::ByteBuffersDirectory;
fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch < ' ' => {
                use std::fmt::Write;
                write!(out, "\\u{:04x}", u32::from(ch)).unwrap();
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}
fn emit(id: &str, value: String) {
    assert!(value.len() <= 65536);
    println!(
        "{{\"engine_differential_native_case\":{{\"id\":{},\"outcome\":\"pass\",\"fixture_sha256\":\"e56f8bf8ee611a69998bdd1961081a52e8830fd71a9346aeea53d9980c719b2e\",\"value\":{}}}}}",
        quote(id),
        value
    );
}
fn hit_value(
    snapshot: &Snapshot,
    query: &Query,
    sort: &[Sort],
    size: usize,
    expected: &[&str],
    total: u64,
) -> String {
    let result = snapshot.search(query, sort, size, &["uid"]).unwrap();
    assert_eq!(result.total, total);
    assert_eq!(
        result
            .hits
            .iter()
            .map(|h| h.id.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    for hit in &result.hits {
        assert_eq!(
            hit.fields,
            vec![("uid".to_string(), StoredValue::String(hit.id.clone()))]
        );
    }
    format!(
        "{{\"kind\":\"hits\",\"ids\":[{}],\"total\":{},\"projection\":[{}]}}",
        result
            .hits
            .iter()
            .map(|h| quote(&h.id))
            .collect::<Vec<_>>()
            .join(","),
        result.total,
        result
            .hits
            .iter()
            .map(|h| format!("{{\"uid\":{}}}", quote(&h.id)))
            .collect::<Vec<_>>()
            .join(",")
    )
}
fn core_schema() -> Schema {
    Schema::new(
        1,
        &[
            ("uid", Kind::Keyword),
            ("tenant", Kind::Keyword),
            ("category", Kind::Keyword),
            ("title", Kind::Text),
            ("price", Kind::Integer),
            ("active", Kind::Boolean),
            ("created_at", Kind::Date),
        ],
    )
    .unwrap()
}
fn seed_core(engine: &Engine, schema: &Schema) {
    engine
        .apply(
            schema
                .prepare(
                    "001",
                    &[Mutation::Put {
                        id: "001",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("001")),
                            ("tenant", Value::String("acct-a")),
                            ("category", Value::String("book")),
                            ("title", Value::String("Quick brown fox")),
                            ("price", Value::Integer(10)),
                            ("active", Value::Boolean(true)),
                            ("created_at", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "002",
                    &[Mutation::Put {
                        id: "002",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("002")),
                            ("tenant", Value::String("acct-a")),
                            ("category", Value::String("book")),
                            ("title", Value::String("The quick blue hare")),
                            ("price", Value::Integer(20)),
                            ("active", Value::Boolean(false)),
                            ("created_at", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "003",
                    &[Mutation::Put {
                        id: "003",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("003")),
                            ("tenant", Value::String("acct-a")),
                            ("category", Value::String("music")),
                            ("title", Value::String("Slow brown bear")),
                            ("price", Value::Integer(30)),
                            ("active", Value::Boolean(true)),
                            ("created_at", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "004",
                    &[Mutation::Put {
                        id: "004",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("004")),
                            ("tenant", Value::String("acct-b")),
                            ("category", Value::String("book")),
                            ("title", Value::String("Quick brown fox jumps")),
                            ("price", Value::Integer(15)),
                            ("active", Value::Boolean(true)),
                            ("created_at", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "005",
                    &[Mutation::Put {
                        id: "005",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("005")),
                            ("tenant", Value::String("acct-b")),
                            ("category", Value::String("tool")),
                            ("title", Value::String("Blue steel hammer")),
                            ("price", Value::Integer(25)),
                            ("active", Value::Boolean(false)),
                            ("created_at", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "006",
                    &[Mutation::Put {
                        id: "006",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("006")),
                            ("tenant", Value::String("acct-c")),
                            ("category", Value::String("tool")),
                            ("title", Value::String("Red steel saw")),
                            ("price", Value::Integer(35)),
                            ("active", Value::Boolean(true)),
                            ("created_at", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "007",
                    &[Mutation::Put {
                        id: "007",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("007")),
                            ("tenant", Value::String("acct-c")),
                            ("category", Value::String("book")),
                            ("title", Value::String("Quiet northern lake")),
                            ("price", Value::Integer(40)),
                            ("active", Value::Boolean(true)),
                            ("created_at", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "008",
                    &[Mutation::Put {
                        id: "008",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("008")),
                            ("tenant", Value::String("acct-c")),
                            ("category", Value::String("music")),
                            ("title", Value::String("Blue moon rises")),
                            ("price", Value::Integer(50)),
                            ("active", Value::Boolean(false)),
                            ("created_at", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "009",
                    &[Mutation::Put {
                        id: "009",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("009")),
                            ("tenant", Value::String("acct-d")),
                            ("category", Value::String("book")),
                            ("title", Value::String("Brown fox brown fox")),
                            ("price", Value::Integer(5)),
                            ("active", Value::Boolean(true)),
                            ("created_at", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "010",
                    &[Mutation::Put {
                        id: "010",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("010")),
                            ("tenant", Value::String("acct-d")),
                            ("category", Value::String("book")),
                            ("title", Value::String("")),
                            ("price", Value::Integer(0)),
                            ("active", Value::Boolean(false)),
                            ("created_at", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "011",
                    &[Mutation::Put {
                        id: "011",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("011")),
                            ("tenant", Value::String("acct-d")),
                            ("category", Value::String("book")),
                            ("title", Value::String("QUICK café fox")),
                            ("price", Value::Integer(10)),
                            ("active", Value::Boolean(true)),
                            ("created_at", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
}
#[test]
fn differential_ascii_lowercase() {
    let actual = analyze_text("Quick brown fox").unwrap();
    let expected: Vec<(&str, i32, i32, i32)> =
        vec![("quick", 0, 0, 5), ("brown", 1, 6, 11), ("fox", 2, 12, 15)];
    assert_eq!(
        actual
            .iter()
            .map(|t| (t.term.as_str(), t.position, t.start_offset, t.end_offset))
            .collect::<Vec<_>>(),
        expected
    );
    let value = format!(
        "{{\"kind\":\"tokens\",\"tokens\":[{}]}}",
        actual
            .iter()
            .map(|t| format!(
                "{{\"term\":{},\"position\":{},\"start_offset\":{},\"end_offset\":{}}}",
                quote(&t.term),
                t.position,
                t.start_offset,
                t.end_offset
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    emit("ascii-lowercase", value);
}
#[test]
fn differential_accent_preserved() {
    let actual = analyze_text("QUICK café fox").unwrap();
    let expected: Vec<(&str, i32, i32, i32)> =
        vec![("quick", 0, 0, 5), ("café", 1, 6, 10), ("fox", 2, 11, 14)];
    assert_eq!(
        actual
            .iter()
            .map(|t| (t.term.as_str(), t.position, t.start_offset, t.end_offset))
            .collect::<Vec<_>>(),
        expected
    );
    let value = format!(
        "{{\"kind\":\"tokens\",\"tokens\":[{}]}}",
        actual
            .iter()
            .map(|t| format!(
                "{{\"term\":{},\"position\":{},\"start_offset\":{},\"end_offset\":{}}}",
                quote(&t.term),
                t.position,
                t.start_offset,
                t.end_offset
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    emit("accent-preserved", value);
}
#[test]
fn differential_no_default_stopword_removal() {
    let actual = analyze_text("The quick blue hare").unwrap();
    let expected: Vec<(&str, i32, i32, i32)> = vec![
        ("the", 0, 0, 3),
        ("quick", 1, 4, 9),
        ("blue", 2, 10, 14),
        ("hare", 3, 15, 19),
    ];
    assert_eq!(
        actual
            .iter()
            .map(|t| (t.term.as_str(), t.position, t.start_offset, t.end_offset))
            .collect::<Vec<_>>(),
        expected
    );
    let value = format!(
        "{{\"kind\":\"tokens\",\"tokens\":[{}]}}",
        actual
            .iter()
            .map(|t| format!(
                "{{\"term\":{},\"position\":{},\"start_offset\":{},\"end_offset\":{}}}",
                quote(&t.term),
                t.position,
                t.start_offset,
                t.end_offset
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    emit("no-default-stopword-removal", value);
}
#[test]
fn differential_empty_text() {
    let actual = analyze_text("").unwrap();
    let expected: Vec<(&str, i32, i32, i32)> = vec![];
    assert_eq!(
        actual
            .iter()
            .map(|t| (t.term.as_str(), t.position, t.start_offset, t.end_offset))
            .collect::<Vec<_>>(),
        expected
    );
    let value = format!(
        "{{\"kind\":\"tokens\",\"tokens\":[{}]}}",
        actual
            .iter()
            .map(|t| format!(
                "{{\"term\":{},\"position\":{},\"start_offset\":{},\"end_offset\":{}}}",
                quote(&t.term),
                t.position,
                t.start_offset,
                t.end_offset
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    emit("empty-text", value);
}
#[test]
fn differential_jdiff_v1_ascii_255() {
    let actual=analyze_text("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
    let expected: Vec<(&str, i32, i32, i32)> = vec![(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        0,
        0,
        255,
    )];
    assert_eq!(
        actual
            .iter()
            .map(|t| (t.term.as_str(), t.position, t.start_offset, t.end_offset))
            .collect::<Vec<_>>(),
        expected
    );
    let value = format!(
        "{{\"kind\":\"tokens\",\"tokens\":[{}]}}",
        actual
            .iter()
            .map(|t| format!(
                "{{\"term\":{},\"position\":{},\"start_offset\":{},\"end_offset\":{}}}",
                quote(&t.term),
                t.position,
                t.start_offset,
                t.end_offset
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    emit("jdiff-v1-ascii-255", value);
}
#[test]
fn differential_jdiff_v1_ascii_256() {
    let actual=analyze_text("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
    let expected: Vec<(&str, i32, i32, i32)> = vec![
        (
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            0,
            0,
            255,
        ),
        ("a", 1, 255, 256),
    ];
    assert_eq!(
        actual
            .iter()
            .map(|t| (t.term.as_str(), t.position, t.start_offset, t.end_offset))
            .collect::<Vec<_>>(),
        expected
    );
    let value = format!(
        "{{\"kind\":\"tokens\",\"tokens\":[{}]}}",
        actual
            .iter()
            .map(|t| format!(
                "{{\"term\":{},\"position\":{},\"start_offset\":{},\"end_offset\":{}}}",
                quote(&t.term),
                t.position,
                t.start_offset,
                t.end_offset
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    emit("jdiff-v1-ascii-256", value);
}
#[test]
fn differential_jdiff_v1_supplementary_offsets() {
    let actual = analyze_text("A 😀 Z").unwrap();
    let expected: Vec<(&str, i32, i32, i32)> =
        vec![("a", 0, 0, 1), ("😀", 1, 2, 4), ("z", 2, 5, 6)];
    assert_eq!(
        actual
            .iter()
            .map(|t| (t.term.as_str(), t.position, t.start_offset, t.end_offset))
            .collect::<Vec<_>>(),
        expected
    );
    let value = format!(
        "{{\"kind\":\"tokens\",\"tokens\":[{}]}}",
        actual
            .iter()
            .map(|t| format!(
                "{{\"term\":{},\"position\":{},\"start_offset\":{},\"end_offset\":{}}}",
                quote(&t.term),
                t.position,
                t.start_offset,
                t.end_offset
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    emit("jdiff-v1-supplementary-offsets", value);
}
#[test]
fn differential_jdiff_v1_supplementary_prefix_253() {
    let actual=analyze_text("brown aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa𐐀 fox").unwrap();
    let expected: Vec<(&str, i32, i32, i32)> = vec![
        ("brown", 0, 0, 5),
        (
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa𐐨",
            1,
            6,
            261,
        ),
        ("fox", 2, 262, 265),
    ];
    assert_eq!(
        actual
            .iter()
            .map(|t| (t.term.as_str(), t.position, t.start_offset, t.end_offset))
            .collect::<Vec<_>>(),
        expected
    );
    let value = format!(
        "{{\"kind\":\"tokens\",\"tokens\":[{}]}}",
        actual
            .iter()
            .map(|t| format!(
                "{{\"term\":{},\"position\":{},\"start_offset\":{},\"end_offset\":{}}}",
                quote(&t.term),
                t.position,
                t.start_offset,
                t.end_offset
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    emit("jdiff-v1-supplementary-prefix-253", value);
}
#[test]
fn differential_jdiff_v1_supplementary_prefix_254() {
    let actual=analyze_text("brown aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa𐐀 fox").unwrap();
    let expected: Vec<(&str, i32, i32, i32)> = vec![
        ("brown", 0, 0, 5),
        (
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            1,
            6,
            260,
        ),
        ("𐐨", 2, 260, 262),
        ("fox", 3, 263, 266),
    ];
    assert_eq!(
        actual
            .iter()
            .map(|t| (t.term.as_str(), t.position, t.start_offset, t.end_offset))
            .collect::<Vec<_>>(),
        expected
    );
    let value = format!(
        "{{\"kind\":\"tokens\",\"tokens\":[{}]}}",
        actual
            .iter()
            .map(|t| format!(
                "{{\"term\":{},\"position\":{},\"start_offset\":{},\"end_offset\":{}}}",
                quote(&t.term),
                t.position,
                t.start_offset,
                t.end_offset
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    emit("jdiff-v1-supplementary-prefix-254", value);
}
#[test]
fn differential_jdiff_v1_supplementary_prefix_255() {
    let actual=analyze_text("brown aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa𐐀 fox").unwrap();
    let expected: Vec<(&str, i32, i32, i32)> = vec![
        ("brown", 0, 0, 5),
        (
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            1,
            6,
            261,
        ),
        ("𐐨", 2, 261, 263),
        ("fox", 3, 264, 267),
    ];
    assert_eq!(
        actual
            .iter()
            .map(|t| (t.term.as_str(), t.position, t.start_offset, t.end_offset))
            .collect::<Vec<_>>(),
        expected
    );
    let value = format!(
        "{{\"kind\":\"tokens\",\"tokens\":[{}]}}",
        actual
            .iter()
            .map(|t| format!(
                "{{\"term\":{},\"position\":{},\"start_offset\":{},\"end_offset\":{}}}",
                quote(&t.term),
                t.position,
                t.start_offset,
                t.end_offset
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    emit("jdiff-v1-supplementary-prefix-255", value);
}
#[test]
fn differential_match_all() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::All;
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        100,
        &[
            "001", "002", "003", "004", "005", "006", "007", "008", "009", "010", "011",
        ],
        11,
    );
    emit("match-all", value);
    engine.close().unwrap();
}
#[test]
fn differential_keyword_term() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::Term {
        field: "category",
        value: Value::String("book"),
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        100,
        &["001", "002", "004", "007", "009", "010", "011"],
        7,
    );
    emit("keyword-term", value);
    engine.close().unwrap();
}
#[test]
fn differential_keyword_terms() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::Terms {
        field: "category",
        values: &[Value::String("music"), Value::String("tool")],
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        100,
        &["003", "005", "006", "008"],
        4,
    );
    emit("keyword-terms", value);
    engine.close().unwrap();
}
#[test]
fn differential_integer_range() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::Range {
        field: "price",
        lower: Value::Integer(10),
        upper: Value::Integer(29),
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        100,
        &["001", "002", "004", "005", "011"],
        5,
    );
    emit("integer-range", value);
    engine.close().unwrap();
}
#[test]
fn differential_integer_threshold_bool() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::Bool {
        must: &[],
        filter: &[
            Query::Term {
                field: "category",
                value: Value::String("book"),
            },
            Query::Term {
                field: "active",
                value: Value::Boolean(true),
            },
        ],
        should: &[
            Query::Match {
                field: "title",
                text: "quick",
                phrase: false,
            },
            Query::Term {
                field: "tenant",
                value: Value::String("acct-d"),
            },
        ],
        must_not: &[Query::Range {
            field: "price",
            lower: Value::Integer(30),
            upper: Value::Integer(2147483647),
        }],
        minimum_should_match: 1,
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        100,
        &["001", "004", "009", "011"],
        4,
    );
    emit("integer-threshold-bool", value);
    engine.close().unwrap();
}
#[test]
fn differential_standard_match() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::Match {
        field: "title",
        text: "quick",
        phrase: false,
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        100,
        &["001", "002", "004", "011"],
        4,
    );
    emit("standard-match", value);
    engine.close().unwrap();
}
#[test]
fn differential_standard_phrase() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::Match {
        field: "title",
        text: "brown fox",
        phrase: true,
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        100,
        &["001", "004", "009"],
        3,
    );
    emit("standard-phrase", value);
    engine.close().unwrap();
}
#[test]
fn differential_standard_accent_preserved() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::Match {
        field: "title",
        text: "café",
        phrase: false,
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        100,
        &["011"],
        1,
    );
    emit("standard-accent-preserved", value);
    engine.close().unwrap();
}
#[test]
fn differential_analyzed_text_term_is_case_sensitive() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::Term {
        field: "title",
        value: Value::String("Quick"),
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        100,
        &[],
        0,
    );
    emit("analyzed-text-term-is-case-sensitive", value);
    engine.close().unwrap();
}
#[test]
fn differential_bounded_field_sort() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::All;
    let value = hit_value(
        &snapshot,
        &query,
        &[
            Sort::Field {
                name: "price",
                descending: false,
            },
            Sort::Field {
                name: "uid",
                descending: false,
            },
        ],
        3,
        &["010", "009", "001"],
        11,
    );
    emit("bounded-field-sort", value);
    engine.close().unwrap();
}
#[test]
fn differential_date_range() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::Range {
        field: "created_at",
        lower: Value::Date(1767225600000),
        upper: Value::Date(1767311999999),
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        100,
        &[
            "001", "002", "003", "004", "005", "006", "007", "008", "009", "010", "011",
        ],
        11,
    );
    emit("date-range", value);
    engine.close().unwrap();
}
#[test]
fn differential_finite_metrics() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::All;
    let m = snapshot.metrics(&query, &["price"]).unwrap().remove(0);
    assert_eq!(
        (m.sum, m.min, m.max, m.count),
        (240.0, Some(0.0), Some(50.0), 11)
    );
    let value = format!(
        "{{\"kind\":\"aggregations\",\"values\":{{\"sum_price\":{:?},\"min_price\":{:?},\"max_price\":{:?},\"count_price\":{}.0}}}}",
        m.sum,
        m.min.unwrap(),
        m.max.unwrap(),
        m.count
    );
    emit("finite-metrics", value);
    engine.close().unwrap();
}
#[test]
fn differential_finite_key_ordered_terms() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::All;
    let groups = snapshot.groups(&query, "category", 128).unwrap();
    assert_eq!(
        groups
            .iter()
            .map(|b| (b.key.as_str(), b.count))
            .collect::<Vec<_>>(),
        vec![("book", 7), ("music", 2), ("tool", 2)]
    );
    let value = format!(
        "{{\"kind\":\"aggregations\",\"values\":{{\"categories\":[{}]}}}}",
        groups
            .iter()
            .map(|b| format!("{{\"key\":{},\"doc_count\":{}}}", quote(&b.key), b.count))
            .collect::<Vec<_>>()
            .join(",")
    );
    emit("finite-key-ordered-terms", value);
    engine.close().unwrap();
}
#[test]
fn differential_local_bm25_ranking_order() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = core_schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    seed_core(&engine, &schema);
    let snapshot = engine.refresh().unwrap();
    let query = Query::Match {
        field: "title",
        text: "brown fox",
        phrase: false,
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[
            Sort::Score,
            Sort::Field {
                name: "uid",
                descending: false,
            },
        ],
        100,
        &["009", "001", "004", "003", "011"],
        5,
    );
    emit("local-bm25-ranking-order", value);
    engine.close().unwrap();
}
#[test]
fn differential_jdiff_v1_supplementary_phrase() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = Schema::new(1, &[("uid", Kind::Keyword), ("title", Kind::Text)]).unwrap();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    engine.apply(schema.prepare("exact",&[Mutation::Put{id:"exact",version:1,fields:&[("uid",Value::String("exact")),("title",Value::String("brown aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa𐐀 fox"))]}]).unwrap()).unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "adjacent",
                    &[Mutation::Put {
                        id: "adjacent",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("adjacent")),
                            ("title", Value::String("brown fox")),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "gap",
                    &[Mutation::Put {
                        id: "gap",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("gap")),
                            ("title", Value::String("brown red fox")),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "reversed",
                    &[Mutation::Put {
                        id: "reversed",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("reversed")),
                            ("title", Value::String("fox brown")),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    let snapshot = engine.refresh().unwrap();
    let query = Query::Match {
        field: "title",
        text: "brown aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa𐐀 fox",
        phrase: true,
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        128,
        &["exact"],
        1,
    );
    emit("jdiff-v1-supplementary-phrase", value);
    engine.close().unwrap();
}
#[test]
fn differential_jdiff_v1_held_out_phrase() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = Schema::new(1, &[("uid", Kind::Keyword), ("title", Kind::Text)]).unwrap();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "adjacent",
                    &[Mutation::Put {
                        id: "adjacent",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("adjacent")),
                            ("title", Value::String("brown fox")),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "gap",
                    &[Mutation::Put {
                        id: "gap",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("gap")),
                            ("title", Value::String("brown red fox")),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "reversed",
                    &[Mutation::Put {
                        id: "reversed",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("reversed")),
                            ("title", Value::String("fox brown")),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    let snapshot = engine.refresh().unwrap();
    let query = Query::Match {
        field: "title",
        text: "brown fox",
        phrase: true,
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        128,
        &["adjacent"],
        1,
    );
    emit("jdiff-v1-held-out-phrase", value);
    engine.close().unwrap();
}
#[test]
fn differential_jdiff_v1_held_out_date() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = Schema::new(1, &[("uid", Kind::Keyword), ("date", Kind::Date)]).unwrap();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "below",
                    &[Mutation::Put {
                        id: "below",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("below")),
                            ("date", Value::Date(1767225599999)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "lower",
                    &[Mutation::Put {
                        id: "lower",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("lower")),
                            ("date", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "middle",
                    &[Mutation::Put {
                        id: "middle",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("middle")),
                            ("date", Value::Date(1767225600001)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "upper",
                    &[Mutation::Put {
                        id: "upper",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("upper")),
                            ("date", Value::Date(1767225600002)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "above",
                    &[Mutation::Put {
                        id: "above",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("above")),
                            ("date", Value::Date(1767225600003)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    let snapshot = engine.refresh().unwrap();
    let query = Query::Range {
        field: "date",
        lower: Value::Date(1767225600000),
        upper: Value::Date(1767225600002),
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        128,
        &["lower", "middle", "upper"],
        3,
    );
    emit("jdiff-v1-held-out-date", value);
    engine.close().unwrap();
}
#[test]
fn differential_jdiff_v1_held_out_phrase_unordered_control() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = Schema::new(1, &[("uid", Kind::Keyword), ("title", Kind::Text)]).unwrap();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "adjacent",
                    &[Mutation::Put {
                        id: "adjacent",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("adjacent")),
                            ("title", Value::String("brown fox")),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "gap",
                    &[Mutation::Put {
                        id: "gap",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("gap")),
                            ("title", Value::String("brown red fox")),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "reversed",
                    &[Mutation::Put {
                        id: "reversed",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("reversed")),
                            ("title", Value::String("fox brown")),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    let snapshot = engine.refresh().unwrap();
    let query = Query::Bool {
        must: &[
            Query::Term {
                field: "title",
                value: Value::String("brown"),
            },
            Query::Term {
                field: "title",
                value: Value::String("fox"),
            },
        ],
        filter: &[],
        should: &[],
        must_not: &[],
        minimum_should_match: 0,
    };
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        128,
        &["adjacent", "gap", "reversed"],
        3,
    );
    emit("jdiff-v1-held-out-phrase-unordered-control", value);
    engine.close().unwrap();
}
#[test]
fn differential_jdiff_v1_held_out_date_all_control() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = Schema::new(1, &[("uid", Kind::Keyword), ("date", Kind::Date)]).unwrap();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "below",
                    &[Mutation::Put {
                        id: "below",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("below")),
                            ("date", Value::Date(1767225599999)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "lower",
                    &[Mutation::Put {
                        id: "lower",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("lower")),
                            ("date", Value::Date(1767225600000)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "middle",
                    &[Mutation::Put {
                        id: "middle",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("middle")),
                            ("date", Value::Date(1767225600001)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "upper",
                    &[Mutation::Put {
                        id: "upper",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("upper")),
                            ("date", Value::Date(1767225600002)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    engine
        .apply(
            schema
                .prepare(
                    "above",
                    &[Mutation::Put {
                        id: "above",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("above")),
                            ("date", Value::Date(1767225600003)),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    let snapshot = engine.refresh().unwrap();
    let query = Query::All;
    let value = hit_value(
        &snapshot,
        &query,
        &[Sort::Field {
            name: "uid",
            descending: false,
        }],
        128,
        &["above", "below", "lower", "middle", "upper"],
        5,
    );
    emit("jdiff-v1-held-out-date-all-control", value);
    engine.close().unwrap();
}
