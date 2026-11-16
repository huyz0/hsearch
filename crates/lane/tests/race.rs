//! Writers racing to open and append on one lane: the safety property of the fence, checked on the
//! final log rather than on any one writer's view.
mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use hsearch_format::SlotKind;
use hsearch_lane::{Error, LaneId, LaneWriter, block_on, create_lane, read_log, shared_group};
use support::*;

#[derive(Debug)]
struct Run {
    fence: u64,
    acked: Vec<(u64, u64)>, // (ordinal, marker)
}

fn writer_run(
    store: Arc<hsearch_lane::counting::CountingStore<hsearch_object::memory::MemoryStore>>,
    lane: LaneId,
    who: u64,
    batches: u64,
) -> Run {
    let mut tries = 0;
    let (mut writer, report) = loop {
        match block_on(LaneWriter::open(
            Arc::clone(&store),
            layout(),
            lane,
            0,
            |_| Ok(()),
        )) {
            Ok(opened) => break opened,
            Err(Error::Contended | Error::Unknown { .. } | Error::Store(_)) if tries < 200 => {
                tries += 1
            }
            Err(error) => panic!("open failed: {error:?}"),
        }
    };
    let mut run = Run {
        fence: report.fence_ordinal,
        acked: Vec::new(),
    };
    for n in 0..batches {
        let marker = who * 1_000_000 + n;
        match block_on(writer.append(&[put(id(1), marker, b"payload")])) {
            Ok(ack) => run.acked.push((ack.ordinal, marker)),
            Err(Error::Fenced | Error::Poisoned | Error::Unknown { .. }) => break,
            Err(error) => panic!("append failed: {error:?}"),
        }
    }
    run
}

#[test]
fn no_acknowledged_batch_lands_behind_a_later_fence_and_every_ack_is_in_the_log() {
    for round in 0..20u64 {
        let store = counted();
        let (domain, group_id) = shared_group(&policy(), round);
        let lane = LaneId::new(domain);
        block_on(create_lane(
            &*store,
            &layout(),
            &lane,
            &policy(),
            group_id,
            1000,
            &[id(1)],
        ))
        .unwrap();
        let handles: Vec<_> = (0..4u64)
            .map(|who| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || writer_run(store, lane, who + 1, 25))
            })
            .collect();
        let runs: Vec<Run> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let mut log = BTreeMap::new();
        block_on(read_log(&*store, &layout(), &lane, 0, None, |v| {
            log.insert(
                v.position.ordinal,
                (v.kind, v.records().map(|r| r.seq).collect::<Vec<_>>()),
            );
            Ok(())
        }))
        .unwrap();
        // The log is gap free: positions 0..n all exist.
        assert_eq!(
            log.keys().copied().collect::<Vec<_>>(),
            (0..log.len() as u64).collect::<Vec<_>>()
        );
        let mut seen = std::collections::BTreeSet::new();
        for run in &runs {
            for (ordinal, marker) in &run.acked {
                assert!(
                    seen.insert(*ordinal),
                    "round {round}: ordinal {ordinal} acknowledged twice"
                );
                let slot = log
                    .get(ordinal)
                    .unwrap_or_else(|| panic!("round {round}: acked slot {ordinal} missing"));
                assert_eq!(slot.0, SlotKind::Data);
                assert_eq!(
                    slot.1,
                    vec![*marker],
                    "round {round}: the acked slot holds the acked record"
                );
            }
        }
        // The fence property: a writer never acknowledges at or past a later opener's fence.
        for a in &runs {
            for b in &runs {
                if b.fence > a.fence
                    && let Some(max) = a.acked.iter().map(|x| x.0).max()
                {
                    assert!(
                        max < b.fence,
                        "round {round}: ack at {max} behind fence {}",
                        b.fence
                    );
                }
            }
        }
        // Every fence position holds a fence (a fence is never overwritten by data).
        for run in &runs {
            assert_eq!(log[&run.fence].0, SlotKind::Fence);
        }
    }
}
