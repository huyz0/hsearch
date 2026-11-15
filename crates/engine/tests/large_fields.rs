//! Large text fields: the per-document byte and term bounds are `Limits`, not
//! constants, the term count streams instead of materializing tokens, and a
//! keyword stays under Lucene's 32766-byte term limit however large the document
//! limit is.
use hsearch_engine::{Engine, Error, Kind, Limits, Mutation, Query, Schema, StoredValue, Value};
use lucene_store::ByteBuffersDirectory;

const MIB: usize = 1 << 20;

fn schema(document_bytes: usize) -> Schema {
    Schema::new(1, &[("tag", Kind::Keyword), ("body", Kind::Text)])
        .unwrap()
        .with_limits(Limits::scale().with_document_bytes(document_bytes))
        .unwrap()
}

fn put(
    schema: &Schema,
    id: &str,
    tag: &str,
    body: &str,
) -> Result<hsearch_engine::PreparedBatch, Error> {
    schema.prepare(
        "request",
        &[Mutation::Put {
            id,
            version: 1,
            fields: &[("tag", Value::String(tag)), ("body", Value::String(body))],
        }],
    )
}

#[test]
fn a_multi_megabyte_text_field_is_indexed_searchable_and_stored_whole() {
    let schema = schema(4 * MIB);
    // About 600,000 tokens: far over the research profile's 128-term bound.
    let body = format!("{}needle", "word ".repeat(3 * MIB / 5));
    let directory = ByteBuffersDirectory::new();
    let observed = hsearch_engine::EngineDirectory::new(&directory);
    let engine = Engine::open(&observed, schema.clone(), None).unwrap();
    engine
        .apply(put(&schema, "big", "t", &body).unwrap())
        .unwrap();
    let snapshot = engine.refresh().unwrap();
    let found = snapshot
        .search(
            &Query::Term {
                field: "body",
                value: Value::String("needle"),
            },
            &[],
            1,
            &["body"],
        )
        .unwrap();
    assert_eq!(found.total, 1);
    assert_eq!(
        found.hits[0].fields[0].1,
        StoredValue::String(body.clone()),
        "the stored value round-trips every byte"
    );
}

#[test]
fn the_document_byte_limit_is_the_schema_limit_and_refuses_one_byte_over() {
    let schema = schema(MIB);
    // "id" (2) + "tag" (3) + "t" (1) + "body" (4) = 10 bytes of frame.
    let fits = "a".repeat(MIB - 10);
    assert!(put(&schema, "id", "t", &fits).is_ok());
    assert!(matches!(
        put(&schema, "id", "t", &format!("{fits}a")),
        Err(Error::Limit)
    ));
}

#[test]
fn the_research_profile_still_refuses_beyond_its_constants() {
    let bounded = Schema::new(1, &[("tag", Kind::Keyword), ("body", Kind::Text)]).unwrap();
    assert!(matches!(
        put(&bounded, "id", "t", &"a".repeat(9000)),
        Err(Error::Limit)
    ));
}

#[test]
fn a_keyword_keeps_lucenes_term_limit_whatever_the_document_limit() {
    let schema = schema(4 * MIB);
    assert!(put(&schema, "id", &"k".repeat(32766), "x").is_ok());
    assert!(matches!(
        put(&schema, "id", &"k".repeat(32767), "x"),
        Err(Error::Limit)
    ));
}

#[test]
fn the_term_bound_is_a_limit_and_counts_across_the_whole_document() {
    let schema = Schema::new(1, &[("tag", Kind::Keyword), ("body", Kind::Text)])
        .unwrap()
        .with_limits(Limits {
            terms: 1000,
            ..Limits::scale().with_document_bytes(4 * MIB)
        })
        .unwrap();
    assert!(put(&schema, "id", "t", &"w ".repeat(1000)).is_ok());
    assert!(matches!(
        put(&schema, "id", "t", &"w ".repeat(1001)),
        Err(Error::Limit)
    ));
}
