//! `Engine::admit_chain` decides a run of batches as `Engine::apply` would take them one after
//! another, and changes nothing.
use hsearch_engine::{Checked, Error, Kind, Limits, Mutation, Schema, Value, with_local_engine};

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

fn dir(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hsearch-chain-{label}-{}", std::process::id()))
}

#[test]
fn a_chain_is_decided_as_if_its_fresh_batches_were_applied_in_order() {
    let schema = schema();
    with_local_engine(&dir("order"), schema.clone(), |engine, _| {
        let batches = [
            schema.prepare("r1", &put("a", 1)).unwrap(),
            // The same identity again at a higher version: fresh only after r1.
            schema.prepare("r2", &put("a", 2)).unwrap(),
            // Not above r2's version: refused, and it leaves no trace.
            schema.prepare("r3", &put("a", 2)).unwrap(),
            schema.prepare("r4", &put("b", 1)).unwrap(),
            // The same request as r1: a conflict inside the chain.
            schema.prepare("r1", &put("c", 1)).unwrap(),
        ];
        let refs: Vec<_> = batches.iter().collect();
        let decisions = engine.admit_chain(&refs).unwrap();
        assert_eq!(decisions[0], Ok(Checked::Fresh));
        assert_eq!(decisions[1], Ok(Checked::Fresh));
        assert_eq!(decisions[2], Err(Error::Conflict));
        assert_eq!(decisions[3], Ok(Checked::Fresh));
        assert_eq!(decisions[4], Err(Error::Conflict));
        // Asking changed nothing: the first is still fresh on its own.
        assert_eq!(engine.admit(&batches[0]).unwrap(), Checked::Fresh);
        // Applying in order agrees with every decision.
        for (batch, decision) in batches.into_iter().zip(decisions) {
            match decision {
                Ok(_) => assert!(engine.apply(batch).is_ok()),
                Err(error) => assert_eq!(engine.apply(batch).unwrap_err(), error),
            }
        }
    })
    .unwrap();
}

#[test]
fn a_bound_crossed_inside_the_chain_is_found_before_anything_is_applied() {
    let limits = Limits {
        operations: 3,
        ..Limits::BOUNDED
    };
    let schema = schema().with_limits(limits).unwrap();
    with_local_engine(&dir("limit"), schema.clone(), |engine, _| {
        let batches: Vec<_> = ["a", "b", "c", "d"]
            .iter()
            .enumerate()
            .map(|(i, id)| schema.prepare(&format!("r{i}"), &put(id, 1)).unwrap())
            .collect();
        let refs: Vec<_> = batches.iter().collect();
        let decisions = engine.admit_chain(&refs).unwrap();
        assert_eq!(
            decisions,
            vec![
                Ok(Checked::Fresh),
                Ok(Checked::Fresh),
                Ok(Checked::Fresh),
                Err(Error::Limit)
            ]
        );
        for (batch, decision) in batches.into_iter().zip(decisions) {
            assert_eq!(engine.apply(batch).map(|_| ()), decision.map(|_| ()));
        }
    })
    .unwrap();
}

#[test]
fn a_recognised_batch_adds_nothing_to_the_chain() {
    let schema = schema();
    with_local_engine(&dir("again"), schema.clone(), |engine, _| {
        let first = schema.prepare("r1", &put("a", 1)).unwrap();
        let receipts = engine.apply(first).unwrap();
        let again = schema.prepare("r1", &put("a", 1)).unwrap();
        let next = schema.prepare("r2", &put("a", 2)).unwrap();
        let decisions = engine.admit_chain(&[&again, &next]).unwrap();
        assert_eq!(decisions[0], Ok(Checked::Recognised(receipts)));
        assert_eq!(decisions[1], Ok(Checked::Fresh));
    })
    .unwrap();
}
