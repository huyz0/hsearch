//! A writer recovered from a published base plus the committed tail, with nothing local, matches an
//! independent ledger of the whole history.
#[path = "support/history.rs"]
mod history;
mod support;

use std::path::PathBuf;

use history::*;
use hsearch_engine::{
    Engine, Error as EngineError, Mutation, StoredValue, TOMBSTONES_FILE, Value, apply_batch,
    with_local_engine,
};
use hsearch_lane::{block_on, read_head};
use hsearch_object::{MetadataStore, ObjectBackend, ObjectKey};
use hsearch_store::publish::AnyDependencies;
use hsearch_store::rebuild::{RebuildError, Rebuilt, recover_writer};
use hsearch_store::recover::{Expect, NotReady};
use support::*;

/// Batches `0..BASE` are in the published base; the rest are the committed tail.
const BASE: usize = 3;

fn scratch(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hsearch-rebase-{label}-{}", std::process::id()))
}

/// A log of the whole history in a lane, and a base published and activated at `BASE`: the writer
/// engine applies the batches below it, commits, and publishes its files and tombstone list.
/// `with_tombstones` leaves the list out of the base, as a base from before it existed.
fn world(
    label: &str,
    with_tombstones: bool,
) -> (std::sync::Arc<Crash>, hsearch_lane::LaneId, Vec<u64>) {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let h = history();
    let bodies: Vec<Vec<u8>> = h.iter().enumerate().map(|(i, s)| body(i, s)).collect();
    let ordinals = write(&mut c, &bodies);
    drop(c);
    let schema = schema();
    with_local_engine(&scratch(label), schema.clone(), |engine, local| {
        for b in &bodies[..BASE] {
            apply_batch(&schema, engine, b).unwrap();
        }
        let checkpoint = engine.commit().unwrap();
        let mut names = local.names().unwrap();
        if with_tombstones {
            names.push(local.write_tombstones(&checkpoint).unwrap());
        }
        let p = publisher(&store);
        let s = spec(cut(BASE as u64), h.len() as u64, 1);
        let published = p.publish(local, &names, &s, &AnyDependencies).unwrap();
        let (head, version) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
        p.activate(
            &at(&lane, &head, &version),
            &published,
            &s,
            ordinals[BASE],
            &AnyDependencies,
        )
        .unwrap();
    })
    .unwrap();
    (store, lane, ordinals)
}

fn recover(
    store: &std::sync::Arc<Crash>,
    lane: &hsearch_lane::LaneId,
    expect: Expect,
    dir: &std::path::Path,
    run: impl FnOnce(&Engine<'_>, &Rebuilt),
) -> Result<(), RebuildError> {
    recover_writer(
        &publisher(store),
        &target(lane, big(), expect),
        &AnyDependencies,
        &schema(),
        dir,
        run,
    )
}

#[test]
fn a_writer_recovered_from_a_base_and_tail_matches_the_ledger() {
    let (store, lane, ordinals) = world("a", true);
    let h = history();
    let expect = Expect {
        ordinal: ordinals.last().unwrap() + 1,
        seq: h.len() as u64,
    };
    let schema = schema();
    recover(
        &store,
        &lane,
        expect,
        &scratch("a-open"),
        |engine, rebuilt| {
            // Only the tail was replayed onto the base's engine.
            assert_eq!(rebuilt.recovered.from_seq, BASE as u64);
            assert_eq!(rebuilt.recovered.applied, h.len() as u64);
            assert_eq!(rebuilt.recovered.replayed, (h.len() - BASE) as u64);
            let model = outcomes(&h);
            for (i, (seq, receipts)) in rebuilt.receipts.iter().enumerate() {
                let at = BASE + i;
                assert_eq!(*seq, at as u64);
                let got: Vec<_> = receipts
                    .iter()
                    .map(|r| (r.id.as_str(), r.version, r.outcome))
                    .collect();
                let want: Vec<_> = h[at]
                    .iter()
                    .zip(&model[at])
                    .map(|(s, o)| (s.id, s.version, *o))
                    .collect();
                assert_eq!(got, want, "tail batch {at}");
            }
            // Documents, versions and stored values are the whole ledger's, base and tail alike.
            let snapshot = engine.refresh().unwrap();
            for (id, (version, tag)) in ledger(&h) {
                let got = snapshot.get(id, &["tag"]).unwrap();
                match tag {
                    None => assert!(got.is_none(), "{id}"),
                    Some(tag) => {
                        let hit = got.unwrap();
                        assert_eq!(hit.version, version, "{id}");
                        assert_eq!(
                            hit.fields,
                            vec![("tag".to_string(), StoredValue::String(tag.to_string()))],
                            "{id}"
                        );
                    }
                }
            }
            // "b" was deleted at version 3 inside the base: only the tombstone list keeps that, and a
            // put under it is still refused after recovery.
            let stale = [Mutation::Put {
                id: "b",
                version: 2,
                fields: &[("tag", Value::String("q"))],
            }];
            assert_eq!(
                engine
                    .apply(schema.prepare("later", &stale).unwrap())
                    .unwrap_err(),
                EngineError::Conflict
            );
            let newer = [Mutation::Put {
                id: "b",
                version: 9,
                fields: &[("tag", Value::String("q"))],
            }];
            assert_eq!(
                engine
                    .apply(schema.prepare("later2", &newer).unwrap())
                    .unwrap()
                    .len(),
                1
            );
        },
    )
    .unwrap();
}

#[test]
fn recovery_uses_nothing_local() {
    let (store, lane, _) = world("b", true);
    let dir = scratch("b-open");
    let run = |engine: &Engine<'_>, rebuilt: &Rebuilt| {
        let snapshot = engine.refresh().unwrap();
        let docs: Vec<_> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| {
                snapshot
                    .get(id, &["tag"])
                    .unwrap()
                    .map(|h| (h.version, h.fields))
            })
            .collect();
        (rebuilt.recovered.applied, docs)
    };
    let mut first = None;
    recover(&store, &lane, Expect::default(), &dir, |e, r| {
        first = Some(run(e, r))
    })
    .unwrap();
    // The writer's own directory is removed and the recovery's scratch is left holding the first
    // recovery's files: a second recovery empties it and rebuilds the same state from the store
    // alone.
    let _ = std::fs::remove_dir_all(scratch("b"));
    assert!(dir.exists());
    let mut second = None;
    recover(&store, &lane, Expect::default(), &dir, |e, r| {
        second = Some(run(e, r))
    })
    .unwrap();
    assert_eq!(first, second);
    assert!(first.is_some());
}

#[test]
fn a_base_without_a_tombstone_list_or_with_a_lost_file_is_refused() {
    // No list: the writer cannot know what was deleted, so it is not reopened on this base.
    let (store, lane, _) = world("c", false);
    assert_eq!(
        recover(
            &store,
            &lane,
            Expect::default(),
            &scratch("c-open"),
            |_, _| ()
        )
        .unwrap_err(),
        RebuildError::MissingTombstones
    );
    // A base file removed from the store is committed history missing, not something to skip.
    let (store, lane, _) = world("d", true);
    let (head, _) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
    let manifest = head.checkpoint.unwrap();
    let p = publisher(&store);
    let root = p
        .verify_root(&INDEX, &SHARD, 1, &manifest, &AnyDependencies)
        .unwrap();
    let entries = p.entries(&INDEX, &SHARD, 1, &root).unwrap();
    assert!(entries.iter().any(|e| e.name == TOMBSTONES_FILE));
    let entry = entries.iter().find(|e| e.name != TOMBSTONES_FILE).unwrap();
    let key = ObjectKey::new(layout().payload(&INDEX, &entry.object).as_bytes()).unwrap();
    let info = block_on(store.inner_ref().head(&ctx(), &key)).unwrap();
    block_on(
        store
            .inner_ref()
            .remove_version(&ctx(), &key, &info.version),
    )
    .unwrap();
    assert!(matches!(
        recover(
            &store,
            &lane,
            Expect::default(),
            &scratch("d-open"),
            |_, _| ()
        )
        .unwrap_err(),
        RebuildError::NotReady(NotReady::Base(_))
    ));
}

#[test]
fn a_fetched_file_that_fails_its_digest_is_removed_and_refused() {
    let (store, lane, _) = world("e", true);
    let (head, _) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
    let p = publisher(&store);
    let root = p
        .verify_root(
            &INDEX,
            &SHARD,
            1,
            &head.checkpoint.unwrap(),
            &AnyDependencies,
        )
        .unwrap();
    let entries = p.entries(&INDEX, &SHARD, 1, &root).unwrap();
    let mut entry = entries
        .iter()
        .find(|e| e.name != TOMBSTONES_FILE)
        .unwrap()
        .clone();
    let dir = scratch("e-fetch");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The intact entry fetches; the same entry claiming another digest does not, and leaves no file.
    p.fetch(&INDEX, &entry, &dir).unwrap();
    std::fs::remove_file(dir.join(&entry.name)).unwrap();
    entry.digest[0] ^= 1;
    assert!(matches!(
        p.fetch(&INDEX, &entry, &dir),
        Err(hsearch_store::publish::Error::Broken("file digest"))
    ));
    assert!(!dir.join(&entry.name).exists());
    // A length that differs from the object's is refused before any byte is read.
    entry.digest[0] ^= 1;
    entry.length += 1;
    assert!(matches!(
        p.fetch(&INDEX, &entry, &dir),
        Err(hsearch_store::publish::Error::Broken("file length"))
    ));
    // A name that would leave the directory is refused.
    entry.length -= 1;
    entry.name = "../escape".into();
    assert!(matches!(
        p.fetch(&INDEX, &entry, &dir),
        Err(hsearch_store::publish::Error::Broken("file name"))
    ));
    // An existing file is never overwritten.
    entry.name = entries
        .iter()
        .find(|e| e.name != TOMBSTONES_FILE)
        .unwrap()
        .name
        .clone();
    p.fetch(&INDEX, &entry, &dir).unwrap();
    assert!(matches!(
        p.fetch(&INDEX, &entry, &dir),
        Err(hsearch_store::publish::Error::Broken("scratch file"))
    ));
}

#[test]
fn a_directory_that_is_not_recovery_scratch_is_never_emptied() {
    let (store, lane, _) = world("f", true);
    let dir = scratch("f-open");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let keep = dir.join("precious.dat");
    std::fs::write(&keep, b"data").unwrap();
    // A non-empty directory no recovery created is refused, with its contents untouched, on the
    // path that has a base and on the one that has none.
    assert!(matches!(
        recover(&store, &lane, Expect::default(), &dir, |_, _| ()).unwrap_err(),
        RebuildError::Scratch(_)
    ));
    assert!(keep.exists());
    let (store, lane, _) = {
        let store = Crash::new();
        let (lane, mut c) = open_lane(&store);
        let bodies: Vec<Vec<u8>> = history()
            .iter()
            .enumerate()
            .map(|(i, s)| body(i, s))
            .collect();
        write(&mut c, &bodies);
        drop(c);
        (store, lane, ())
    };
    assert!(matches!(
        recover(&store, &lane, Expect::default(), &dir, |_, _| ()).unwrap_err(),
        RebuildError::Scratch(_)
    ));
    assert!(keep.exists());
    // An empty directory, or one a recovery marked, is fine.
    std::fs::remove_file(&keep).unwrap();
    recover(&store, &lane, Expect::default(), &dir, |_, _| ()).unwrap();
    recover(&store, &lane, Expect::default(), &dir, |_, _| ()).unwrap();
}
