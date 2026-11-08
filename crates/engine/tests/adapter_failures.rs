//! Native write/commit/refresh/merge faults and supported writer ownership.
use hsearch_engine::{Engine, Error, Kind, Mutation, Query, Schema, Value};
use lucene_store::{ByteBuffersDirectory, Directory, FsIndexOutput, Input, Lock};
use std::sync::atomic::{AtomicU8, Ordering};

struct FaultDirectory {
    inner: ByteBuffersDirectory,
    mode: AtomicU8,
}
impl FaultDirectory {
    fn new() -> Self {
        Self {
            inner: ByteBuffersDirectory::new(),
            mode: AtomicU8::new(0),
        }
    }
    fn fail(&self, mode: u8) -> lucene_store::Result<()> {
        if self.mode.load(Ordering::SeqCst) == mode {
            Err(std::io::Error::other("injected native failure").into())
        } else {
            Ok(())
        }
    }
}
impl Directory for FaultDirectory {
    fn list_all(&self) -> lucene_store::Result<Vec<String>> {
        self.inner.list_all()
    }
    fn open(&self, name: &str) -> lucene_store::Result<Input> {
        self.inner.open(name)
    }
    fn file_length(&self, name: &str) -> lucene_store::Result<u64> {
        self.inner.file_length(name)
    }
    fn create_output(&self, name: &str) -> lucene_store::Result<FsIndexOutput> {
        self.fail(1)?;
        self.inner.create_output(name)
    }
    fn create_temp_output(
        &self,
        prefix: &str,
        suffix: &str,
    ) -> lucene_store::Result<FsIndexOutput> {
        self.fail(1)?;
        self.inner.create_temp_output(prefix, suffix)
    }
    fn sync(&self, names: &[String]) -> lucene_store::Result<()> {
        self.fail(2)?;
        self.inner.sync(names)
    }
    fn rename(&self, source: &str, dest: &str) -> lucene_store::Result<()> {
        self.fail(3)?;
        self.inner.rename(source, dest)
    }
    fn delete_file(&self, name: &str) -> lucene_store::Result<()> {
        self.inner.delete_file(name)
    }
    fn sync_meta_data(&self) -> lucene_store::Result<()> {
        self.fail(2)?;
        self.inner.sync_meta_data()
    }
    fn obtain_lock(&self, name: &str) -> lucene_store::Result<Box<dyn Lock>> {
        self.inner.obtain_lock(name)
    }
}

#[test]
fn failed_native_mutation_commit_refresh_and_merge_poison_continuation() {
    for action in ["apply", "commit-sync", "commit-rename", "refresh", "merge"] {
        let directory = FaultDirectory::new();
        let schema = Schema::new(1, &[("key", Kind::Keyword)]).unwrap();
        let engine = Engine::open(&directory, schema.clone(), None).unwrap();
        engine
            .apply(
                schema
                    .prepare(
                        "initial",
                        &[Mutation::Put {
                            id: "stable",
                            version: 1,
                            fields: &[("key", Value::String("old"))],
                        }],
                    )
                    .unwrap(),
            )
            .unwrap();
        engine.commit().unwrap();
        let old = engine.refresh().unwrap();
        if action == "merge" {
            for i in 0..8 {
                let id = format!("extra-{i}");
                engine
                    .apply(
                        schema
                            .prepare(
                                &id,
                                &[Mutation::Put {
                                    id: &id,
                                    version: 1,
                                    fields: &[("key", Value::String("new"))],
                                }],
                            )
                            .unwrap(),
                    )
                    .unwrap();
            }
            engine.commit().unwrap();
        }
        directory.mode.store(
            match action {
                "commit-sync" => 2,
                "commit-rename" => 3,
                _ => 1,
            },
            Ordering::SeqCst,
        );
        let result = match action {
            "apply" => {
                let ids: Vec<_> = (0..8).map(|i| format!("new-{i}")).collect();
                let fields = [("key", Value::String("new"))];
                let operations: Vec<_> = ids
                    .iter()
                    .map(|id| Mutation::Put {
                        id,
                        version: 1,
                        fields: &fields,
                    })
                    .collect();
                engine
                    .apply(schema.prepare("failed-batch", &operations).unwrap())
                    .map(|_| ())
            }
            "commit-sync" | "commit-rename" => {
                engine
                    .apply(
                        schema
                            .prepare(
                                "pending",
                                &[Mutation::Put {
                                    id: "stable",
                                    version: 2,
                                    fields: &[("key", Value::String("new"))],
                                }],
                            )
                            .unwrap(),
                    )
                    .unwrap();
                engine.commit().map(|_| ())
            }
            "refresh" => {
                engine
                    .apply(
                        schema
                            .prepare(
                                "pending",
                                &[Mutation::Put {
                                    id: "stable",
                                    version: 2,
                                    fields: &[("key", Value::String("new"))],
                                }],
                            )
                            .unwrap(),
                    )
                    .unwrap();
                engine.refresh().map(|_| ())
            }
            "merge" => engine.merge(),
            _ => unreachable!(),
        };
        assert_eq!(result, Err(Error::Native), "{action}");
        assert!(matches!(engine.refresh(), Err(Error::Poisoned)), "{action}");
        assert!(matches!(engine.commit(), Err(Error::Poisoned)), "{action}");
        assert_eq!(
            engine.apply(
                schema
                    .prepare(
                        "later",
                        &[Mutation::Delete {
                            id: "stable",
                            version: 3
                        }]
                    )
                    .unwrap()
            ),
            Err(Error::Poisoned),
            "{action}"
        );
        assert_eq!(
            old.search(&Query::All, &[], 128, &[]).unwrap().total,
            1,
            "{action}"
        );
        directory.mode.store(0, Ordering::SeqCst);
        engine.close().unwrap();
    }
}

#[test]
fn a_second_writer_is_refused_and_checkpoint_schema_is_not_interchangeable() {
    let directory = ByteBuffersDirectory::new();
    let schema = Schema::new(1, &[("key", Kind::Keyword)]).unwrap();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    // Even an empty index has one native write-lock owner.
    assert!(Engine::open(&directory, schema.clone(), None).is_err());
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();
    let changed = Schema::new(2, &[("key", Kind::Keyword)]).unwrap();
    assert!(matches!(
        Engine::open(&directory, changed, Some(checkpoint.clone())),
        Err(Error::Checkpoint)
    ));
    let empty = ByteBuffersDirectory::new();
    assert!(matches!(
        Engine::open(&empty, schema, Some(checkpoint)),
        Err(Error::Checkpoint)
    ));
}
