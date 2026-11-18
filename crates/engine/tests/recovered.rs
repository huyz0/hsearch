//! An engine opened on the files of a committed base and its tombstone list, in a process that
//! holds no checkpoint, continues exactly as the engine that wrote them would.
use hsearch_engine::{
    Checkpoint, Engine, EngineDirectory, Error, Kind, MergeMode, Mutation, Outcome, Query, Schema,
    Value, with_local_engine,
};
use lucene_store::FsDirectory;

fn schema() -> Schema {
    Schema::new(1, &[("tag", Kind::Keyword)]).unwrap()
}

fn root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hsearch-recovered-{label}-{}", std::process::id()))
}

fn put<'a>(id: &'a str, version: i64, fields: &'a [(&'a str, Value<'a>)]) -> Mutation<'a> {
    Mutation::Put {
        id,
        version,
        fields,
    }
}

/// A base's native files by name.
type Files = Vec<(String, Vec<u8>)>;

/// Write a history on a scratch engine, commit, and return its files and tombstone list.
fn base(label: &str) -> (Files, Vec<u8>, Checkpoint) {
    let schema = schema();
    with_local_engine(&root(label), schema.clone(), |engine, local| {
        let x = [("tag", Value::String("x"))];
        let y = [("tag", Value::String("y"))];
        for (r, batch) in [
            (
                "r0",
                vec![put("a", 1, &x), put("b", 1, &x), put("c", 1, &x)],
            ),
            ("r1", vec![put("a", 5, &y)]),
            (
                "r2",
                vec![
                    Mutation::Delete {
                        id: "b",
                        version: 7,
                    },
                    Mutation::Delete {
                        id: "ghost",
                        version: 9,
                    },
                ],
            ),
        ] {
            engine.apply(schema.prepare(r, &batch).unwrap()).unwrap();
        }
        let checkpoint = engine.commit().unwrap();
        let files = local
            .names()
            .unwrap()
            .into_iter()
            .map(|n| {
                let bytes = local.read(&n).unwrap();
                (n, bytes)
            })
            .collect();
        let tombstones = checkpoint.tombstones().unwrap();
        (files, tombstones, checkpoint)
    })
    .unwrap()
}

fn place(label: &str, files: &[(String, Vec<u8>)]) -> std::path::PathBuf {
    let dir = root(label);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, bytes) in files {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    dir
}

#[test]
fn a_recovered_engine_keeps_live_versions_and_tombstones() {
    let (files, tombstones, _) = base("base");
    let dir = place("open", &files);
    let directory = EngineDirectory::new(FsDirectory::open(&dir));
    let schema = schema();
    let engine =
        Engine::open_recovered(&directory, schema.clone(), &tombstones, MergeMode::Inline).unwrap();
    let z = [("tag", Value::String("z"))];
    let apply =
        |request: &str, m: Mutation<'_>| engine.apply(schema.prepare(request, &[m]).unwrap());
    // A live document: an older or equal version is stale, a newer one replaces it.
    assert_eq!(apply("s1", put("a", 5, &z)).unwrap_err(), Error::Conflict);
    assert_eq!(apply("s2", put("a", 4, &z)).unwrap_err(), Error::Conflict);
    assert_eq!(
        apply("s3", put("a", 6, &z)).unwrap()[0].outcome,
        Outcome::Updated
    );
    // A deleted one keeps its tombstone's version: a put under it is refused.
    assert_eq!(apply("s4", put("b", 7, &z)).unwrap_err(), Error::Conflict);
    assert_eq!(apply("s5", put("b", 3, &z)).unwrap_err(), Error::Conflict);
    assert_eq!(
        apply("s6", put("b", 8, &z)).unwrap()[0].outcome,
        Outcome::Created
    );
    // A delete of an identity that never existed was remembered too.
    assert_eq!(
        apply("s7", put("ghost", 9, &z)).unwrap_err(),
        Error::Conflict
    );
    // An identity the history never saw is simply new.
    assert_eq!(
        apply("s8", put("new", 1, &z)).unwrap()[0].outcome,
        Outcome::Created
    );
    let snapshot = engine.refresh().unwrap();
    assert_eq!(snapshot.search(&Query::All, &[], 0, &[]).unwrap().total, 4);
    // Unchanged documents are as written; the engine commits again.
    assert_eq!(snapshot.get("c", &["tag"]).unwrap().unwrap().version, 1);
    engine.commit().unwrap();
    engine.close().unwrap();
}

#[test]
fn a_recovered_engine_continues_like_the_one_that_wrote_the_base() {
    let (files, tombstones, checkpoint) = base("same");
    let schema = schema();
    let z = [("tag", Value::String("z"))];
    let later = [
        put("a", 6, &z),
        put("b", 7, &z),
        put("c", 2, &z),
        put("d", 1, &z),
    ];
    let live: Vec<_> = with_local_engine(&root("same-live"), schema.clone(), |engine, _| {
        // The same history, then the continuation.
        let x = [("tag", Value::String("x"))];
        let y = [("tag", Value::String("y"))];
        engine
            .apply(
                schema
                    .prepare("r0", &[put("a", 1, &x), put("b", 1, &x), put("c", 1, &x)])
                    .unwrap(),
            )
            .unwrap();
        engine
            .apply(schema.prepare("r1", &[put("a", 5, &y)]).unwrap())
            .unwrap();
        engine
            .apply(
                schema
                    .prepare(
                        "r2",
                        &[
                            Mutation::Delete {
                                id: "b",
                                version: 7,
                            },
                            Mutation::Delete {
                                id: "ghost",
                                version: 9,
                            },
                        ],
                    )
                    .unwrap(),
            )
            .unwrap();
        later
            .iter()
            .enumerate()
            .map(|(i, m)| {
                engine
                    .apply(
                        schema
                            .prepare(&format!("n{i}"), std::slice::from_ref(m))
                            .unwrap(),
                    )
                    .map(|v| v[0].outcome)
            })
            .collect()
    })
    .unwrap();
    drop(checkpoint);
    let dir = place("same-open", &files);
    let directory = EngineDirectory::new(FsDirectory::open(&dir));
    let engine =
        Engine::open_recovered(&directory, schema.clone(), &tombstones, MergeMode::Inline).unwrap();
    let recovered: Vec<_> = later
        .iter()
        .enumerate()
        .map(|(i, m)| {
            engine
                .apply(
                    schema
                        .prepare(&format!("n{i}"), std::slice::from_ref(m))
                        .unwrap(),
                )
                .map(|v| v[0].outcome)
        })
        .collect();
    assert_eq!(live, recovered);
    // b at 7 is refused (its tombstone is 7), the others are accepted.
    assert_eq!(recovered[1], Err(Error::Conflict));
    assert_eq!(recovered[0], Ok(Outcome::Updated));
    engine.close().unwrap();
}

#[test]
fn what_does_not_match_the_files_is_refused() {
    let (files, tombstones, _) = base("bad");
    let schema = schema();
    let open = |label: &str, files: &[(String, Vec<u8>)], bytes: &[u8]| {
        let dir = place(label, files);
        let directory = EngineDirectory::new(FsDirectory::open(&dir));
        Engine::open_recovered(&directory, schema.clone(), bytes, MergeMode::Inline)
            .map(|e| e.close())
            .err()
    };
    assert_eq!(open("ok", &files, &tombstones), None);
    // Every strict prefix of the list, a trailing byte and a bad magic are refused whole.
    for cut in 0..tombstones.len() {
        assert_eq!(
            open("cut", &files, &tombstones[..cut]),
            Some(Error::Checkpoint),
            "prefix {cut}"
        );
    }
    let mut extra = tombstones.clone();
    extra.push(0);
    assert_eq!(open("extra", &files, &extra), Some(Error::Checkpoint));
    let mut magic = tombstones.clone();
    magic[0] ^= 1;
    assert_eq!(open("magic", &files, &magic), Some(Error::Checkpoint));
    // A count the bytes cannot hold.
    let mut huge = tombstones[..4].to_vec();
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(open("huge", &files, &huge), Some(Error::Checkpoint));
    // No files at all is not a base.
    assert_eq!(open("empty", &[], &tombstones), Some(Error::Checkpoint));
}

#[test]
fn a_deleted_and_live_identity_is_refused() {
    // The base's files hold "a" and "c" live; a list that also deletes "a" cannot be this base.
    let (files, real, _) = base("dl");
    let schema = schema();
    // Build the list by hand: one tombstone, for the live "a".
    let mut bytes = b"HST2".to_vec();
    bytes.extend_from_slice(&real[4..36]);
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.push(b'a');
    bytes.extend_from_slice(&9i64.to_le_bytes());
    let dir = place("dl-open", &files);
    let directory = EngineDirectory::new(FsDirectory::open(&dir));
    assert_eq!(
        Engine::open_recovered(&directory, schema, &bytes, MergeMode::Inline)
            .err()
            .unwrap(),
        Error::Checkpoint
    );
}

#[test]
fn a_tombstone_without_a_positive_version_is_refused() {
    let (files, real, _) = base("zero");
    let schema = schema();
    for version in [0i64, -3] {
        let mut bytes = b"HST2".to_vec();
        bytes.extend_from_slice(&real[4..36]);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(b'z');
        bytes.extend_from_slice(&version.to_le_bytes());
        let dir = place("zero-open", &files);
        let directory = EngineDirectory::new(FsDirectory::open(&dir));
        assert_eq!(
            Engine::open_recovered(&directory, schema.clone(), &bytes, MergeMode::Inline)
                .err()
                .unwrap(),
            Error::Checkpoint
        );
    }
}

/// A list with the base's own commit identity and the given tombstones.
fn list(real: &[u8], dead: &[(&str, i64)]) -> Vec<u8> {
    let mut bytes = b"HST2".to_vec();
    bytes.extend_from_slice(&real[4..36]);
    bytes.extend_from_slice(&(dead.len() as u32).to_le_bytes());
    for (id, version) in dead {
        bytes.extend_from_slice(&(id.len() as u32).to_le_bytes());
        bytes.extend_from_slice(id.as_bytes());
        bytes.extend_from_slice(&version.to_le_bytes());
    }
    bytes
}

#[test]
fn a_list_must_be_ordered_and_belong_to_these_files() {
    let (files, real, _) = base("order");
    let schema = schema();
    let open = |label: &str, files: &Files, bytes: &[u8]| {
        let dir = place(label, files);
        let directory = EngineDirectory::new(FsDirectory::open(&dir));
        Engine::open_recovered(&directory, schema.clone(), bytes, MergeMode::Inline)
            .map(|e| e.close())
            .err()
    };
    assert_eq!(
        open("o1", &files, &list(&real, &[("m", 2), ("n", 3)])),
        None
    );
    // Out of order, or repeated, is a list this engine never wrote.
    assert_eq!(
        open("o2", &files, &list(&real, &[("n", 3), ("m", 2)])),
        Some(Error::Checkpoint)
    );
    assert_eq!(
        open("o3", &files, &list(&real, &[("m", 2), ("m", 2)])),
        Some(Error::Checkpoint)
    );
    // The same history one commit later: its list is not this base's. Its files differ in commit
    // identity, so pairing the new list with the old files (or the reverse) is refused, where a
    // list lacking the delete would otherwise be accepted and forget it.
    let (later_files, first_list, second_list) =
        with_local_engine(&root("order-later"), schema.clone(), |engine, local| {
            let x = [("tag", Value::String("x"))];
            engine
                .apply(
                    schema
                        .prepare("r0", &[put("a", 1, &x), put("b", 1, &x), put("c", 1, &x)])
                        .unwrap(),
                )
                .unwrap();
            let first = engine.commit().unwrap().tombstones().unwrap();
            let delete = [Mutation::Delete {
                id: "b",
                version: 7,
            }];
            engine
                .apply(schema.prepare("r1", &delete).unwrap())
                .unwrap();
            let second = engine.commit().unwrap().tombstones().unwrap();
            let files: Files = local
                .names()
                .unwrap()
                .into_iter()
                .map(|n| {
                    let bytes = local.read(&n).unwrap();
                    (n, bytes)
                })
                .collect();
            (files, first, second)
        })
        .unwrap();
    assert_eq!(
        open("o4", &later_files, &first_list),
        Some(Error::Checkpoint)
    );
    assert_eq!(open("o5", &later_files, &second_list), None);
}

#[test]
fn a_recovered_engine_reopens_as_a_checkpoint_over_every_value_kind() {
    // The digests rebuilt from the index are the ones apply computes: the recovered engine's own
    // checkpoint reopens over the same directory, which verifies each stored value against them.
    let schema = Schema::new(
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
    .unwrap();
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
    let (files, tombstones) = with_local_engine(&root("kinds"), schema.clone(), |engine, local| {
        engine
            .apply(
                schema
                    .prepare("r", &[put("a", 1, &fields), put("b", 1, &fields)])
                    .unwrap(),
            )
            .unwrap();
        let tombstones = engine.commit().unwrap().tombstones().unwrap();
        let files: Files = local
            .names()
            .unwrap()
            .into_iter()
            .map(|n| {
                let bytes = local.read(&n).unwrap();
                (n, bytes)
            })
            .collect();
        (files, tombstones)
    })
    .unwrap();
    let dir = place("kinds-open", &files);
    let directory = EngineDirectory::new(FsDirectory::open(&dir));
    let engine =
        Engine::open_recovered(&directory, schema.clone(), &tombstones, MergeMode::Inline).unwrap();
    let again = [
        ("tag", Value::String("t")),
        ("body", Value::String("a quick fox")),
        ("n", Value::Integer(-7)),
        ("ok", Value::Boolean(true)),
        ("at", Value::Date(86_400_000)),
        ("reviews", Value::Nested(&[])),
    ];
    engine
        .apply(schema.prepare("s", &[put("c", 2, &again)]).unwrap())
        .unwrap();
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();
    let reopened = Engine::open(&directory, schema.clone(), Some(checkpoint)).unwrap();
    assert_eq!(
        reopened
            .refresh()
            .unwrap()
            .search(&Query::All, &[], 0, &[])
            .unwrap()
            .total,
        3
    );
    reopened.close().unwrap();
}
