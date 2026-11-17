//! M04.01 bridge contract: the one public boundary over the pinned Lucene
//! revision. Run alone after a pin bump: `cargo test -p hsearch-engine --test bridge_contract`.
use hsearch_engine::{
    Capability, ENGINE_REVISION, EngineDirectory, EngineFactory, Error, Kind, Mutation, Outcome,
    Query, Schema, ShardEngine, Sort, Support, Value, WriterMode,
};
use lucene_store::ByteBuffersDirectory;

fn schema() -> Schema {
    Schema::new(1, &[("uid", Kind::Keyword), ("title", Kind::Text)]).unwrap()
}

#[test]
fn pinned_revision_matches_every_lucene_dependency_in_the_workspace_manifest() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml")).unwrap();
    let revs: Vec<&str> = manifest
        .lines()
        .filter(|l| l.starts_with("lucene-") && l.contains("huyz0/lucene-rust"))
        .map(|l| {
            l.split("rev = \"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
        })
        .collect();
    assert!(
        revs.len() >= 6,
        "expected every lucene crate pinned: {revs:?}"
    );
    assert!(revs.iter().all(|r| *r == ENGINE_REVISION), "{revs:?}");
    assert_eq!(EngineFactory::new().revision(), ENGINE_REVISION);
}

#[test]
fn no_hsearch_source_vendors_or_patches_lucene_rust() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for dir in ["crates", "xtask", "tools"] {
        let mut stack = vec![root.join(dir)];
        while let Some(path) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in read.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    assert_ne!(
                        p.file_name().unwrap(),
                        "lucene-rust",
                        "vendored copy: {p:?}"
                    );
                    stack.push(p);
                }
            }
        }
    }
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(
        !manifest.contains("[patch"),
        "private patch of an engine crate"
    );
}

#[test]
fn inventory_is_explicit_and_every_capability_has_exactly_one_entry() {
    let factory = EngineFactory::new();
    let inventory = factory.inventory();
    for capability in Capability::ALL {
        let n = inventory
            .iter()
            .filter(|f| f.capability == *capability)
            .count();
        assert_eq!(n, 1, "{capability:?}");
    }
    assert_eq!(inventory.len(), Capability::ALL.len());
    for (i, capability) in Capability::ALL.iter().enumerate() {
        assert_eq!(
            capability.index(),
            i,
            "ALL must list {capability:?} at its slot"
        );
        assert_eq!(inventory[i].capability, *capability);
    }
    for feature in inventory {
        if let Support::Unsupported { reason } = feature.support {
            assert!(
                !reason.is_empty(),
                "{:?} needs a stated reason",
                feature.capability
            );
        }
    }
    assert_eq!(
        factory.support(Capability::MaterializedInput),
        Support::Supported
    );
    assert_eq!(
        factory.support(Capability::RemoteRangeInput),
        Support::Supported
    );
    assert!(matches!(
        factory.support(Capability::ScriptedUpdate),
        Support::Unsupported { .. }
    ));
}

#[test]
fn an_unsupported_capability_is_refused_never_ignored() {
    let factory = EngineFactory::new();
    assert_eq!(
        factory.require(Capability::ScriptedUpdate),
        Err(Error::Unsupported)
    );
    assert_eq!(factory.require(Capability::QueryTerm), Ok(()));
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    // Both input modes are offered since M04.02. `open` refuses through
    // `require(capability_of_mode(mode))`, so the mapping is pinned here and
    // the refusal itself by `require` above.
    assert_eq!(
        hsearch_engine::capability_of_mode(WriterMode::Materialized),
        Capability::MaterializedInput
    );
    assert_eq!(
        hsearch_engine::capability_of_mode(WriterMode::RemoteRange),
        Capability::RemoteRangeInput
    );
    assert!(
        factory
            .open(&observed, schema(), None, WriterMode::RemoteRange)
            .is_ok()
    );
    // The directory stays openable.
    assert!(
        factory
            .open(&observed, schema(), None, WriterMode::Materialized)
            .is_ok()
    );
}

#[test]
fn every_query_and_sort_variant_maps_to_its_own_capability() {
    let factory = EngineFactory::new();
    let leaf = [Query::All];
    let values = [Value::Integer(1)];
    let cases: Vec<(Query<'_>, Capability)> = vec![
        (Query::Id("a"), Capability::QueryId),
        (Query::All, Capability::QueryAll),
        (
            Query::Term {
                field: "uid",
                value: Value::String("a"),
            },
            Capability::QueryTerm,
        ),
        (
            Query::Terms {
                field: "n",
                values: &values,
            },
            Capability::QueryTerms,
        ),
        (
            Query::Range {
                field: "n",
                lower: Value::Integer(1),
                upper: Value::Integer(2),
            },
            Capability::QueryRange,
        ),
        (
            Query::Match {
                field: "title",
                text: "x",
                phrase: false,
            },
            Capability::QueryMatch,
        ),
        (
            Query::Bool {
                must: &leaf,
                filter: &[],
                should: &[],
                must_not: &[],
                minimum_should_match: 0,
            },
            Capability::QueryBool,
        ),
        (
            Query::Knn {
                field: "embedding",
                vector: &[0.0],
                k: 1,
                candidates: 1,
                nprobe: 0,
                filter: None,
            },
            Capability::QueryKnn,
        ),
        (Query::Unsupported, Capability::UnsupportedQuery),
    ];
    for (query, expected) in &cases {
        assert_eq!(hsearch_engine::capability_of_query(query), *expected);
        assert_eq!(
            factory.support(*expected) == Support::Supported,
            *expected != Capability::UnsupportedQuery
        );
    }
    assert_eq!(
        hsearch_engine::capability_of_sort(&Sort::Score),
        Capability::SortScore
    );
    assert_eq!(
        hsearch_engine::capability_of_sort(&Sort::Field {
            name: "n",
            descending: true
        }),
        Capability::SortField
    );
    assert_eq!(
        factory.require(Capability::UnsupportedQuery),
        Err(Error::Unsupported)
    );
}

#[test]
fn a_search_refuses_an_unsupported_query_as_unsupported() {
    let factory = EngineFactory::new();
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = factory
        .open(&observed, schema(), None, WriterMode::Materialized)
        .unwrap();
    let snapshot = engine.refresh().unwrap();
    assert_eq!(
        snapshot.search(&Query::Unsupported, &[], 1, &[]).err(),
        Some(Error::Unsupported)
    );
    assert!(snapshot.search(&Query::All, &[], 1, &[]).is_ok());
}

#[test]
fn no_crate_outside_the_engine_opens_an_engine_except_through_the_factory() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for entry in std::fs::read_dir(&root).unwrap().flatten() {
        if entry.file_name() == "engine" {
            continue;
        }
        let mut stack = vec![entry.path()];
        while let Some(path) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&path) else {
                continue;
            };
            for e in read.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let text = std::fs::read_to_string(&p).unwrap();
                    assert!(
                        !text.contains("Engine::open"),
                        "bypasses the factory: {p:?}"
                    );
                    assert!(!text.contains("lucene_"), "names a Lucene crate: {p:?}");
                }
            }
        }
    }
}

#[test]
fn a_shard_engine_runs_the_lifecycle_through_the_trait_only() {
    let factory = EngineFactory::new();
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let schema = schema();
    let engine = factory
        .open(&observed, schema.clone(), None, WriterMode::Materialized)
        .unwrap();
    let boxed: Box<dyn ShardEngine + '_> = Box::new(engine);
    let shard = &*boxed;
    let receipts = shard
        .apply(
            schema
                .prepare(
                    "r1",
                    &[Mutation::Put {
                        id: "a",
                        version: 1,
                        fields: &[
                            ("uid", Value::String("a")),
                            ("title", Value::String("hello")),
                        ],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    assert_eq!(receipts[0].outcome, Outcome::Created);
    let snapshot = shard.refresh().unwrap();
    assert_eq!(
        snapshot
            .search(&Query::All, &[Sort::Score], 0, &[])
            .unwrap()
            .total,
        1
    );
    let checkpoint = shard.commit().unwrap();
    shard.merge().unwrap();
    boxed.close_boxed().unwrap();
    // Reopen through the factory with the held checkpoint, as a restart would.
    let reopened = factory
        .open(
            &observed,
            schema,
            Some(checkpoint),
            WriterMode::Materialized,
        )
        .unwrap();
    reopened.close().unwrap();
}

#[test]
fn a_checkpoint_that_native_state_cannot_match_is_a_typed_refusal() {
    let factory = EngineFactory::new();
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let schema = schema();
    let engine = factory
        .open(&observed, schema.clone(), None, WriterMode::Materialized)
        .unwrap();
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();
    // A checkpoint for an empty directory: native state cannot match it.
    let empty = ByteBuffersDirectory::new();
    let empty = EngineDirectory::new(&empty);
    let r = factory.open(&empty, schema, Some(checkpoint), WriterMode::Materialized);
    assert!(matches!(r, Err(Error::Checkpoint)));
}
