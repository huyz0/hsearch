//! Online schema evolution (M21.04): fields added to a live engine are optional,
//! documents written before the addition stay readable and searchable, and a
//! change that cannot be made in place is refused.
use hsearch_engine::{
    Engine, EngineDirectory, Error, Kind, MAX_FIELDS, Mutation, Query, Schema, Snapshot, Sort,
    StoredValue, Value,
};
use lucene_store::ByteBuffersDirectory;

fn base() -> Schema {
    Schema::new(1, &[("k", Kind::Keyword), ("n", Kind::Integer)]).unwrap()
}

fn put(
    schema: &Schema,
    request: &str,
    id: &str,
    fields: &[(&str, Value<'_>)],
) -> Result<hsearch_engine::PreparedBatch, Error> {
    schema.prepare(
        request,
        &[Mutation::Put {
            id,
            version: 1,
            fields,
        }],
    )
}

fn ids(snapshot: &Snapshot, query: &Query<'_>) -> Vec<String> {
    let mut found: Vec<String> = snapshot
        .search(
            query,
            &[Sort::Field {
                name: "k",
                descending: false,
            }],
            10,
            &[],
        )
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.id)
        .collect();
    found.sort();
    found
}

#[test]
fn a_field_added_to_a_live_engine_is_searchable_and_old_documents_stay_readable() {
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let v1 = base();
    let engine = Engine::open(&observed, v1.clone(), None).unwrap();
    engine
        .apply(
            put(
                &v1,
                "a",
                "old",
                &[("k", Value::String("a")), ("n", Value::Integer(1))],
            )
            .unwrap(),
        )
        .unwrap();
    engine.refresh().unwrap();

    let v2 = v1
        .evolve(2, &[("tag", Kind::Keyword), ("note", Kind::Text)])
        .unwrap();
    engine.evolve_schema(v2.clone()).unwrap();

    // A batch prepared under the old schema is not silently applied under the new.
    let stale = put(
        &v1,
        "stale",
        "s",
        &[("k", Value::String("s")), ("n", Value::Integer(9))],
    )
    .unwrap();
    assert_eq!(engine.apply(stale).err(), Some(Error::Schema));

    // New documents may omit the added fields, in any combination.
    for (request, id, fields) in [
        (
            "b",
            "tagged",
            vec![
                ("k", Value::String("b")),
                ("n", Value::Integer(2)),
                ("tag", Value::String("hot")),
            ],
        ),
        (
            "c",
            "noted",
            vec![
                ("k", Value::String("c")),
                ("n", Value::Integer(3)),
                ("note", Value::String("quick brown fox")),
            ],
        ),
        (
            "d",
            "both",
            vec![
                ("k", Value::String("d")),
                ("n", Value::Integer(4)),
                ("tag", Value::String("hot")),
                ("note", Value::String("lazy dog")),
            ],
        ),
    ] {
        engine
            .apply(put(&v2, request, id, &fields).unwrap())
            .unwrap();
    }
    let snapshot = engine.refresh().unwrap();

    let hot = Query::Term {
        field: "tag",
        value: Value::String("hot"),
    };
    assert_eq!(ids(&snapshot, &hot), ["both", "tagged"]);
    let fox = Query::Match {
        field: "note",
        text: "fox",
        phrase: false,
    };
    assert_eq!(ids(&snapshot, &fox), ["noted"]);
    assert_eq!(ids(&snapshot, &Query::All).len(), 4);

    // The document written before the field existed has no value for it, and says so.
    let old = snapshot.get("old", &["k", "tag", "note"]).unwrap().unwrap();
    assert_eq!(
        old.fields,
        vec![("k".to_string(), StoredValue::String("a".to_string()))]
    );
    let both = snapshot.get("both", &["tag", "note"]).unwrap().unwrap();
    assert_eq!(both.fields.len(), 2);
}

#[test]
fn a_required_field_cannot_be_omitted_and_an_added_one_must_keep_schema_order() {
    let v2 = base().evolve(2, &[("tag", Kind::Keyword)]).unwrap();
    assert_eq!(
        put(&v2, "x", "d", &[("k", Value::String("a"))]).err(),
        Some(Error::Document),
        "n is declared at creation, so it stays required"
    );
    assert_eq!(
        put(&v2, "x", "d", &[("n", Value::Integer(1))]).err(),
        Some(Error::Document),
        "a required field cannot be skipped in front of one that is present"
    );
    assert_eq!(
        put(
            &v2,
            "x",
            "d",
            &[
                ("tag", Value::String("t")),
                ("k", Value::String("a")),
                ("n", Value::Integer(1)),
            ],
        )
        .err(),
        Some(Error::Document),
        "fields keep schema order"
    );
    assert!(
        put(
            &v2,
            "x",
            "d",
            &[("k", Value::String("a")), ("n", Value::Integer(1))],
        )
        .is_ok()
    );
}

#[test]
fn an_evolution_that_cannot_be_made_in_place_is_refused() {
    let v1 = base();
    // A different type for an existing name, a repeated name, a reserved name.
    for added in [
        vec![("k", Kind::Text)],
        vec![("k", Kind::Keyword)],
        vec![("a", Kind::Keyword), ("a", Kind::Keyword)],
        vec![("_id", Kind::Keyword)],
        vec![("bad name", Kind::Keyword)],
        vec![],
    ] {
        assert_eq!(v1.evolve(2, &added).err(), Some(Error::Schema), "{added:?}");
    }
    // Only scalar kinds are added online: a vector or nested field needs a rebuild.
    assert_eq!(
        v1.evolve(2, &[("nested", Kind::Nested)]).err(),
        Some(Error::Schema)
    );
    // The revision only moves forward.
    assert_eq!(
        v1.evolve(1, &[("a", Kind::Keyword)]).err(),
        Some(Error::Schema)
    );
    // The native field bound still holds: two names are reserved.
    let too_many: Vec<(String, Kind)> = (0..MAX_FIELDS)
        .map(|i| (format!("f{i}"), Kind::Keyword))
        .collect();
    let too_many: Vec<(&str, Kind)> = too_many.iter().map(|(n, k)| (n.as_str(), *k)).collect();
    assert_eq!(v1.evolve(2, &too_many).err(), Some(Error::Schema));
    // A schema that has evolved takes no nested path or join afterwards.
    let v2 = v1.evolve(2, &[("a", Kind::Keyword)]).unwrap();
    assert_eq!(
        v2.with_nested("o", &[("x", Kind::Keyword)]).err(),
        Some(Error::Schema)
    );
}

#[test]
fn a_checkpoint_reopens_under_a_schema_that_extends_it_and_not_under_one_that_does_not() {
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let v1 = base();
    let engine = Engine::open(&observed, v1.clone(), None).unwrap();
    engine
        .apply(
            put(
                &v1,
                "a",
                "old",
                &[("k", Value::String("a")), ("n", Value::Integer(1))],
            )
            .unwrap(),
        )
        .unwrap();
    let before = engine.commit().unwrap();
    let v2 = v1.evolve(2, &[("tag", Kind::Keyword)]).unwrap();
    engine.evolve_schema(v2.clone()).unwrap();
    engine
        .apply(
            put(
                &v2,
                "b",
                "new",
                &[
                    ("k", Value::String("b")),
                    ("n", Value::Integer(2)),
                    ("tag", Value::String("t")),
                ],
            )
            .unwrap(),
        )
        .unwrap();
    let after = engine.commit().unwrap();
    engine.close().unwrap();

    // A checkpoint taken before the field existed reopens under the evolved schema:
    // its documents are still valid, the new field is absent from them.
    let reopened = Engine::open(&observed, v2.clone(), Some(after.clone()));
    assert!(reopened.is_ok());
    reopened.unwrap().close().unwrap();
    // The reverse is refused: the checkpoint knows a field the schema does not.
    assert!(matches!(
        Engine::open(&observed, v1.clone(), Some(after)),
        Err(Error::Checkpoint)
    ));
    // `before` is stale against the files now (a later commit exists), as it always was.
    assert!(matches!(
        Engine::open(&observed, v2, Some(before)),
        Err(Error::Checkpoint)
    ));
}

#[test]
fn an_evolved_schema_must_extend_the_one_it_replaces() {
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let v1 = base();
    let engine = Engine::open(&observed, v1.clone(), None).unwrap();
    let other = Schema::new(1, &[("k", Kind::Keyword), ("z", Kind::Integer)])
        .unwrap()
        .evolve(2, &[("tag", Kind::Keyword)])
        .unwrap();
    assert_eq!(engine.evolve_schema(other), Err(Error::Schema));
    // The same schema again is a no-op, and a schema that drops a field is refused.
    assert_eq!(engine.evolve_schema(v1.clone()), Ok(()));
    let v2 = v1.evolve(2, &[("tag", Kind::Keyword)]).unwrap();
    engine.evolve_schema(v2.clone()).unwrap();
    assert_eq!(engine.evolve_schema(v1), Err(Error::Schema));
    engine.close().unwrap();
}
