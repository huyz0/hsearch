//! M13.01: cold activation is bounded and its work does not depend on how many indexes are idle.
mod support;

use hsearch_format::{Id, bucket_of};
use hsearch_lane::activation::{ActiveSet, Begin, Limits, Refusal};
use hsearch_lane::partition::{PAGE_TARGET, bucket_bits_for};
use hsearch_lane::{RepairClaim, Resolution, block_on, create_catalog};
use hsearch_object::{
    Metadata, MetadataStore as _, Mutation, ObjectBackend as _, ObjectKey, Payload, Precondition,
    RequestContext, RequestId,
};
use support::{counted, layout};

fn id(n: u64) -> Id {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&n.to_be_bytes());
    out[15] = 0x5A;
    out
}

fn limits(active: usize, bytes: u64, loading: usize) -> Limits {
    Limits {
        max_active: active,
        max_bytes: bytes,
        max_loading: loading,
    }
}

/// Activate `n` with a handle `n` and `bytes` of footprint; returns what was evicted.
fn activate(set: &mut ActiveSet<u64>, n: u64, bytes: u64) -> Vec<(Id, u64)> {
    match set.begin(id(n), bytes).unwrap() {
        Begin::Load { ticket, evicted } => {
            let mut all = evicted;
            all.extend(set.finish(ticket, n, bytes).unwrap());
            all
        }
        other => panic!("expected a load, got {other:?}"),
    }
}

#[test]
fn a_hit_touches_nothing_else_and_a_miss_loads_once() {
    let mut set = ActiveSet::new(limits(4, 1 << 20, 2)).unwrap();
    assert!(activate(&mut set, 1, 100).is_empty());
    assert!(matches!(set.begin(id(1), 100).unwrap(), Begin::Hit));
    assert_eq!(set.get(&id(1)), Some(&1));
    assert_eq!((set.stats().activations, set.stats().hits), (1, 1));
}

#[test]
fn concurrent_activations_of_one_index_share_one_load() {
    let mut set: ActiveSet<u64> = ActiveSet::new(limits(4, 1 << 20, 2)).unwrap();
    let Begin::Load { ticket, .. } = set.begin(id(7), 10).unwrap() else {
        panic!("first caller loads");
    };
    assert!(matches!(set.begin(id(7), 10).unwrap(), Begin::Pending));
    assert!(matches!(set.begin(id(7), 10).unwrap(), Begin::Pending));
    set.finish(ticket, 7, 10).unwrap();
    assert!(matches!(set.begin(id(7), 10).unwrap(), Begin::Hit));
    assert_eq!(set.stats().activations, 1, "one load for three callers");
}

#[test]
fn the_least_recently_used_unleased_index_goes_first() {
    let mut set = ActiveSet::new(limits(3, 1 << 20, 2)).unwrap();
    for n in 1..=3 {
        activate(&mut set, n, 10);
    }
    set.get(&id(1)); // 2 is now the oldest
    let evicted = activate(&mut set, 4, 10);
    assert_eq!(evicted, vec![(id(2), 2)]);
    assert!(set.get(&id(2)).is_none());
    assert_eq!(set.len(), 3);
}

#[test]
fn a_leased_index_is_never_evicted_and_a_full_set_refuses_instead_of_growing() {
    let mut set = ActiveSet::new(limits(2, 1 << 20, 2)).unwrap();
    activate(&mut set, 1, 10);
    activate(&mut set, 2, 10);
    set.lease(&id(1)).unwrap();
    set.lease(&id(2)).unwrap();
    assert_eq!(set.begin(id(3), 10).unwrap_err(), Refusal::Capacity);
    set.release(&id(2)).unwrap();
    let evicted = activate(&mut set, 3, 10);
    assert_eq!(evicted, vec![(id(2), 2)], "the leased one stayed");
    assert!(set.get(&id(1)).is_some());
}

#[test]
fn bytes_are_capped_as_well_as_counts() {
    let mut set = ActiveSet::new(limits(100, 100, 2)).unwrap();
    activate(&mut set, 1, 60);
    let evicted = activate(&mut set, 2, 60);
    assert_eq!(evicted.len(), 1, "60 + 60 exceeds 100 bytes");
    assert_eq!(set.bytes(), 60);
    assert_eq!(set.begin(id(3), 101).unwrap_err(), Refusal::TooLarge);
}

#[test]
fn a_burst_is_held_to_the_loading_cap_and_a_failed_load_gives_its_place_back() {
    let mut set: ActiveSet<u64> = ActiveSet::new(limits(100, 1 << 20, 2)).unwrap();
    let Begin::Load { ticket: a, .. } = set.begin(id(1), 1).unwrap() else {
        panic!()
    };
    let Begin::Load { ticket: b, .. } = set.begin(id(2), 1).unwrap() else {
        panic!()
    };
    assert_eq!(set.begin(id(3), 1).unwrap_err(), Refusal::Loading);
    set.fail(a);
    assert_eq!(set.len(), 0);
    assert!(matches!(set.begin(id(3), 1).unwrap(), Begin::Load { .. }));
    set.fail(b);
    assert_eq!(set.stats().failed, 2);
    // The failed index can be tried again.
    assert!(matches!(set.begin(id(1), 1).unwrap(), Begin::Load { .. }));
}

#[test]
fn work_per_operation_does_not_depend_on_how_many_indexes_are_idle() {
    // The set only ever holds what was activated: the idle population never enters it, so the
    // same sequence costs the same steps whether the ids come from a thousand indexes or ten million.
    let run = |population: u64| {
        let mut set = ActiveSet::new(limits(64, 1 << 30, 8)).unwrap();
        let mut worst = 0;
        for i in 0..5_000u64 {
            let n = i.wrapping_mul(2_654_435_761) % population;
            let before = set.stats().steps;
            match set.begin(id(n), 100).unwrap() {
                Begin::Load { ticket, .. } => {
                    set.finish(ticket, n, 100).unwrap();
                }
                Begin::Hit => {
                    set.get(&id(n));
                }
                Begin::Pending => unreachable!(),
            }
            worst = worst.max(set.stats().steps - before);
        }
        (worst, set.len(), set.footprint_bytes())
    };
    let small = run(1_000);
    let big = run(10_000_000);
    assert!(small.0 <= 8 && big.0 <= 8, "{small:?} {big:?}");
    assert_eq!(small.1, big.1, "both end full");
    assert_eq!(
        small.2, big.2,
        "the footprint depends on the cap, not on the population"
    );
}

#[test]
fn the_page_count_follows_the_names_not_the_tenants() {
    // One-name tenants cost two page objects at most, and only once a name is written.
    assert_eq!(bucket_bits_for(0), 1);
    assert_eq!(bucket_bits_for(50), 1);
    assert_eq!(bucket_bits_for(PAGE_TARGET as u64 * 2), 1);
    assert_eq!(bucket_bits_for(PAGE_TARGET as u64 * 2 + 1), 2);
    // 10 million names in one catalog stay inside the format's fan-out.
    let bits = bucket_bits_for(10_000_000);
    assert_eq!(bits, 14);
    assert!(10_000_000 / (1u64 << bits) <= PAGE_TARGET as u64);
    assert_eq!(
        bucket_bits_for(u64::MAX),
        16,
        "clamped to the format ceiling"
    );
}

#[test]
fn a_load_that_turns_out_larger_makes_room_or_is_given_up_whole() {
    let mut set = ActiveSet::new(limits(10, 100, 2)).unwrap();
    activate(&mut set, 1, 30);
    activate(&mut set, 2, 30);
    // Estimated 10 bytes, really 60: the least recently used goes to make room.
    let Begin::Load { ticket, evicted } = set.begin(id(3), 10).unwrap() else {
        panic!()
    };
    assert!(evicted.is_empty());
    let evicted = set.finish(ticket, 3, 60).unwrap();
    assert_eq!(evicted, vec![(id(1), 1)]);
    assert_eq!(set.bytes(), 90);
    // With the others leased, a load that grows past the cap is refused, not squeezed in.
    set.lease(&id(2)).unwrap();
    set.lease(&id(3)).unwrap();
    let Begin::Load { ticket, .. } = set.begin(id(4), 5).unwrap() else {
        panic!()
    };
    assert_eq!(set.finish(ticket, 4, 50).unwrap_err(), Refusal::Capacity);
    assert!(set.get(&id(4)).is_none());
    assert_eq!(set.bytes(), 90, "its reservation was returned");
    assert_eq!(set.stats().failed, 1);
    // A load that came in under its estimate gives the difference back.
    let Begin::Load { ticket, .. } = set.begin(id(5), 10).unwrap() else {
        panic!()
    };
    set.finish(ticket, 5, 4).unwrap();
    assert_eq!(set.bytes(), 94);
}

#[test]
fn a_zero_cap_a_stale_ticket_and_a_double_release_are_refused() {
    assert_eq!(
        ActiveSet::<u64>::new(limits(0, 1, 1)).err(),
        Some(Refusal::Invalid)
    );
    assert_eq!(
        ActiveSet::<u64>::new(limits(1, 0, 1)).err(),
        Some(Refusal::Invalid)
    );
    assert_eq!(
        ActiveSet::<u64>::new(limits(1, 1, 0)).err(),
        Some(Refusal::Invalid)
    );
    let mut set: ActiveSet<u64> = ActiveSet::new(limits(2, 100, 2)).unwrap();
    let Begin::Load { ticket, .. } = set.begin(id(1), 1).unwrap() else {
        panic!()
    };
    set.fail(ticket);
    assert!(
        set.lease(&id(1)).is_err(),
        "a failed load left nothing to lease"
    );
    activate(&mut set, 2, 1);
    set.lease(&id(2)).unwrap();
    set.release(&id(2)).unwrap();
    assert_eq!(set.release(&id(2)), Err(Refusal::Invalid));
}

fn ctx() -> RequestContext {
    RequestContext::new(
        std::time::Duration::from_secs(10),
        hsearch_object::Cancellation::default(),
        hsearch_object::Budget {
            request_bytes: 1 << 20,
            response_bytes: 1 << 20,
            attempts: 8,
        },
    )
    .unwrap()
}

#[test]
fn a_cold_index_behind_a_lost_page_fails_cleanly_and_activates_after_repair() {
    let store = counted();
    let tenant: Id = [6; 16];
    let bits = bucket_bits_for(20);
    let catalog = block_on(create_catalog(&*store, &layout(), tenant, bits)).unwrap();
    let names: Vec<String> = (0..20).map(|i| format!("index-{i:02}")).collect();
    for (i, name) in names.iter().enumerate() {
        block_on(catalog.create(&*store, &layout(), name, id(100 + i as u64))).unwrap();
    }
    let victim = &names[3];
    let bucket = bucket_of(&tenant, bits, victim);
    // Rot the page the victim lives on.
    let key = ObjectKey::new(layout().catalog_page(&tenant, bucket).as_bytes()).unwrap();
    let info = block_on(store.head(&ctx(), &key)).unwrap();
    block_on(store.compare_exchange(
        &ctx(),
        &Mutation {
            request_id: RequestId::new(b"rot").unwrap(),
            key,
            payload: Payload::new(b"not a page").unwrap(),
            metadata: Metadata::new(b"").unwrap(),
            precondition: Precondition::Matches(info.version),
        },
    ))
    .unwrap();

    let mut set: ActiveSet<Id> = ActiveSet::new(limits(4, 1 << 20, 2)).unwrap();
    let cold = |set: &mut ActiveSet<Id>, name: &str, want: u64| {
        let Begin::Load { ticket, .. } = set.begin(id(want), 10).unwrap() else {
            panic!("expected a load")
        };
        match block_on(catalog.resolve(&*store, &layout(), name)) {
            Ok(Resolution::Live { index, .. }) => {
                assert_eq!(index, id(want));
                set.finish(ticket, index, 10).unwrap();
                true
            }
            _ => {
                set.fail(ticket);
                false
            }
        }
    };
    assert!(
        !cold(&mut set, victim, 103),
        "the lost page cannot be resolved"
    );
    assert_eq!(set.len(), 0, "a failed activation holds no place");
    // Repair from the claims of every name that lives on that page, then activate.
    let claims: Vec<RepairClaim> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| bucket_of(&tenant, bits, n) == bucket)
        .map(|(i, n)| RepairClaim {
            name: n.clone(),
            index: id(100 + i as u64),
            live: true,
        })
        .collect();
    block_on(catalog.repair_page(&*store, &layout(), bucket, &claims)).unwrap();
    let before = store.requests();
    assert!(cold(&mut set, victim, 103));
    let after = store.requests();
    assert_eq!(
        (after.head - before.head, after.read - before.read),
        (1, 1),
        "a resolve is one HEAD and one read"
    );
    assert!(matches!(set.begin(id(103), 10).unwrap(), Begin::Hit));
}
