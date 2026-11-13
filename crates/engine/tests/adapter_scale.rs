//! The scale profile admits 100,000 documents; the default profile still refuses at the old caps.
use hsearch_engine::{
    Engine, EngineDirectory, Error, Kind, Limits, MAX_DOCUMENTS, Mutation, Query, Schema, Sort,
    StoredValue, Value,
};
use lucene_store::ByteBuffersDirectory;

const DOCUMENTS: usize = 100_000;
const CATEGORIES: usize = 50;

fn fields() -> [(&'static str, Kind); 3] {
    [
        ("category", Kind::Keyword),
        ("body", Kind::Text),
        ("n", Kind::Integer),
    ]
}

fn load(engine: &Engine<'_>, schema: &Schema, from: usize, to: usize) {
    for start in (from..to).step_by(16) {
        let end = (start + 16).min(to);
        let ids: Vec<String> = (start..end).map(|i| format!("doc-{i:06}")).collect();
        let cats: Vec<String> = (start..end)
            .map(|i| format!("c{}", i % CATEGORIES))
            .collect();
        let bodies: Vec<String> = (start..end)
            .map(|i| format!("alpha beta gamma item{i}"))
            .collect();
        let values: Vec<[(&str, Value<'_>); 3]> = (start..end)
            .map(|i| {
                let k = i - start;
                [
                    ("category", Value::String(&cats[k])),
                    ("body", Value::String(&bodies[k])),
                    ("n", Value::Integer(i as i64)),
                ]
            })
            .collect();
        let mutations: Vec<Mutation<'_>> = (start..end)
            .map(|i| Mutation::Put {
                id: &ids[i - start],
                version: 1,
                fields: &values[i - start],
            })
            .collect();
        engine
            .apply(schema.prepare(&format!("r-{start}"), &mutations).unwrap())
            .unwrap();
    }
}

fn check(snapshot: &hsearch_engine::Snapshot) {
    let all = snapshot.search(&Query::All, &[], 0, &[]).unwrap();
    assert_eq!(all.total, DOCUMENTS as u64);
    let term = snapshot
        .search(
            &Query::Term {
                field: "category",
                value: Value::String("c7"),
            },
            &[],
            0,
            &[],
        )
        .unwrap();
    assert_eq!(term.total, (DOCUMENTS / CATEGORIES) as u64);
    let text = snapshot
        .search(
            &Query::Match {
                field: "body",
                text: "item99999",
                phrase: false,
            },
            &[],
            5,
            &["n"],
        )
        .unwrap();
    assert_eq!(text.total, 1);
    assert_eq!(text.hits[0].id, "doc-099999");
    let page = snapshot
        .search(
            &Query::All,
            &[Sort::Field {
                name: "n",
                descending: true,
            }],
            3,
            &["n"],
        )
        .unwrap();
    let top: Vec<_> = page.hits.iter().map(|h| h.fields[0].1.clone()).collect();
    assert_eq!(
        top,
        vec![
            StoredValue::Integer(99_999),
            StoredValue::Integer(99_998),
            StoredValue::Integer(99_997)
        ]
    );
    let got = snapshot.get("doc-054321", &["n"]).unwrap().unwrap();
    assert_eq!(got.fields[0].1, StoredValue::Integer(54_321));
}

#[test]
fn scale_profile_holds_searches_and_reopens_a_hundred_thousand_documents() {
    let started = std::time::Instant::now();
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let schema = Schema::new(1, &fields())
        .unwrap()
        .with_limits(Limits::scale())
        .unwrap();
    let engine = Engine::open(&observed, schema.clone(), None).unwrap();
    load(&engine, &schema, 0, DOCUMENTS);
    check(&engine.refresh().unwrap());
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();
    let engine = Engine::open(&observed, schema, Some(checkpoint)).unwrap();
    check(&engine.refresh().unwrap());
    engine.close().unwrap();
    eprintln!("100k scale test took {:?}", started.elapsed());
}

#[test]
fn default_profile_refuses_at_the_old_numbers() {
    assert_eq!(Limits::BOUNDED.documents, MAX_DOCUMENTS);
    assert_eq!(Limits::BOUNDED.documents, 128);
    assert_eq!(Limits::BOUNDED.operations, 256);
    assert_eq!(Limits::BOUNDED.history_bytes, 262_144);
    assert_eq!(Limits::BOUNDED.segments, 32);
    assert_eq!(Limits::BOUNDED.buffered_documents, 8);
    let schema = Schema::new(1, &fields()).unwrap();
    assert_eq!(schema.limits(), Limits::BOUNDED);
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = Engine::open(&observed, schema.clone(), None).unwrap();
    load(&engine, &schema, 0, 128);
    let ids = ["doc-extra".to_string()];
    let values = [
        ("category", Value::String("c")),
        ("body", Value::String("x")),
        ("n", Value::Integer(1)),
    ];
    let refused = engine.apply(
        schema
            .prepare(
                "extra",
                &[Mutation::Put {
                    id: &ids[0],
                    version: 1,
                    fields: &values,
                }],
            )
            .unwrap(),
    );
    assert!(matches!(refused, Err(Error::Limit)));
}

#[test]
fn invalid_limits_are_refused() {
    let schema = Schema::new(1, &fields()).unwrap();
    for bad in [
        Limits {
            documents: 0,
            ..Limits::BOUNDED
        },
        Limits {
            documents: usize::MAX,
            ..Limits::BOUNDED
        },
        Limits {
            operations: 0,
            ..Limits::BOUNDED
        },
        Limits {
            history_bytes: 0,
            ..Limits::BOUNDED
        },
        Limits {
            segments: 0,
            ..Limits::BOUNDED
        },
        Limits {
            buffered_documents: 0,
            ..Limits::BOUNDED
        },
        Limits {
            buffered_documents: usize::MAX,
            ..Limits::BOUNDED
        },
    ] {
        assert!(matches!(
            schema.clone().with_limits(bad),
            Err(Error::Schema)
        ));
    }
}
