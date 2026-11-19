//! `Engine::admit` says what `Engine::apply` would do, and changes nothing.
use hsearch_engine::{Checked, Error, Kind, Mutation, Schema, Value, with_local_engine};

fn schema() -> Schema {
    Schema::new(1, &[("tag", Kind::Keyword)]).unwrap()
}

fn put(id: &'static str, version: i64) -> [Mutation<'static>; 1] {
    [Mutation::Put {
        id,
        version,
        fields: &[("tag", Value::String("x"))],
    }]
}

#[test]
fn admit_agrees_with_apply_and_changes_nothing() {
    let schema = schema();
    let dir = std::env::temp_dir().join(format!("hsearch-admit-{}", std::process::id()));
    with_local_engine(&dir, schema.clone(), |engine, _| {
        let first = schema.prepare("r1", &put("a", 2)).unwrap();
        // Admitting twice changes nothing: it is still fresh, and nothing is searchable.
        assert_eq!(engine.admit(&first).unwrap(), Checked::Fresh);
        assert_eq!(engine.admit(&first).unwrap(), Checked::Fresh);
        assert!(
            engine
                .refresh()
                .unwrap()
                .get("a", &["tag"])
                .unwrap()
                .is_none()
        );
        let receipts = engine.apply(first).unwrap();
        // The same request and operations are recognised with the original receipts; a different
        // body under the same request is a conflict.
        let again = schema.prepare("r1", &put("a", 2)).unwrap();
        assert_eq!(
            engine.admit(&again).unwrap(),
            Checked::Recognised(receipts.clone())
        );
        let other = schema.prepare("r1", &put("a", 3)).unwrap();
        assert_eq!(engine.admit(&other).unwrap_err(), Error::Conflict);
        // A stale version is refused by both, and an apply after a fresh admit is accepted.
        let stale = schema.prepare("r2", &put("a", 2)).unwrap();
        assert_eq!(engine.admit(&stale).unwrap_err(), Error::Conflict);
        assert_eq!(engine.apply(stale).unwrap_err(), Error::Conflict);
        let next = schema.prepare("r3", &put("a", 3)).unwrap();
        assert_eq!(engine.admit(&next).unwrap(), Checked::Fresh);
        assert_eq!(engine.apply(next).unwrap().len(), 1);
    })
    .unwrap();
}
