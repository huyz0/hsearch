//! M03.05 formats: the structural head, the catalog root and a catalog page.
mod common;
use common::*;
use hsearch_format::*;

fn tok(n: u8) -> CommitToken {
    CommitToken::new([n; 16]).unwrap()
}

fn head(token: u8) -> StructuralHead {
    StructuralHead {
        generation: 1,
        domain: [7; 16],
        incarnation: 1,
        activation: 1,
        mode: Durability::Object,
        writer_epoch: 0,
        membership_epoch: 0,
        schema_generation: 1,
        publication_generation: 1,
        cover: 0,
        checkpoint: None,
        tail: TailLocator {
            lane: 0,
            ordinal: 0,
        },
        inventory: vec![],
        token: tok(token),
    }
}

fn enc<T>(f: impl FnOnce(&mut Vec<u8>) -> Result<T>) -> Vec<u8> {
    let mut v = Vec::new();
    f(&mut v).unwrap();
    v
}

fn established(token: u8, epoch: u64) -> StructuralHead {
    let mut h = head(token);
    h.writer_epoch = epoch;
    h.inventory = (1..=epoch)
        .map(|e| EpochEntry {
            epoch: e,
            activation: 1,
            lane: 0,
            fence: 10 * e,
        })
        .collect();
    h
}

#[test]
fn a_head_round_trips_and_is_hardened() {
    let mut h = established(2, 3);
    h.cover = 5;
    h.checkpoint = Some([9; 16]);
    h.tail = TailLocator {
        lane: 0,
        ordinal: 8,
    };
    let b = enc(|o| h.encode(o));
    assert_eq!(StructuralHead::decode(&b).unwrap(), h);
    assert_hardened(&b, StructuralHead::decode);
    assert_eq!(
        StructuralHead::decode_at(&b, &[8; 16], 1).unwrap_err(),
        FormatError::Mismatch("head domain")
    );
    assert_eq!(
        StructuralHead::decode_at(&b, &[7; 16], 2).unwrap_err(),
        FormatError::Mismatch("head incarnation")
    );
}

#[test]
fn a_head_that_contradicts_itself_is_refused() {
    let bad = |f: &dyn Fn(&mut StructuralHead)| {
        let mut h = established(2, 2);
        f(&mut h);
        let mut v = Vec::new();
        h.encode(&mut v).unwrap_err()
    };
    // Writer epoch must be the newest inventory epoch.
    assert!(matches!(
        bad(&|h| h.writer_epoch = 3),
        FormatError::Malformed(_)
    ));
    // Inventory epochs strictly ascend.
    assert!(matches!(
        bad(&|h| h.inventory[1].epoch = 1),
        FormatError::Malformed(_)
    ));
    // A checkpoint locator without a cover, a cover without a checkpoint.
    assert!(matches!(
        bad(&|h| h.checkpoint = Some([1; 16])),
        FormatError::Malformed(_)
    ));
    assert!(matches!(bad(&|h| h.cover = 4), FormatError::Malformed(_)));
    // The tail cannot sit below the cover.
    assert!(matches!(
        bad(&|h| {
            h.cover = 5;
            h.checkpoint = Some([1; 16]);
            h.tail.ordinal = 4;
        }),
        FormatError::Malformed(_)
    ));
    assert!(matches!(
        bad(&|h| h.generation = 0),
        FormatError::Malformed(_)
    ));
    assert!(matches!(
        bad(&|h| h.incarnation = 0),
        FormatError::Malformed(_)
    ));
    assert!(matches!(
        bad(&|h| h.activation = 0),
        FormatError::Malformed(_)
    ));
    let mut h = head(1);
    h.inventory = (1..=(MAX_EPOCH_INVENTORY as u64 + 1))
        .map(|e| EpochEntry {
            epoch: e,
            activation: 1,
            lane: 0,
            fence: e,
        })
        .collect();
    h.writer_epoch = MAX_EPOCH_INVENTORY as u64 + 1;
    assert!(matches!(
        h.encode(&mut Vec::new()).unwrap_err(),
        FormatError::TooLarge(_)
    ));
}

#[test]
fn a_successor_never_regresses_anything() {
    let a = established(1, 1);
    let step = |f: &dyn Fn(&mut StructuralHead)| {
        let mut n = a.clone();
        n.generation = 2;
        n.token = tok(2);
        f(&mut n);
        a.validate_successor(&n)
    };
    assert_eq!(step(&|_| {}), Ok(()));
    // Raising the cover and moving the tail forward is the checkpoint case.
    assert_eq!(
        step(&|n| {
            n.cover = 4;
            n.checkpoint = Some([5; 16]);
            n.tail.ordinal = 6;
        }),
        Ok(())
    );
    // A new writer epoch appends to the inventory.
    assert_eq!(
        step(&|n| {
            n.writer_epoch = 2;
            n.inventory.push(EpochEntry {
                epoch: 2,
                activation: 1,
                lane: 0,
                fence: 20,
            });
        }),
        Ok(())
    );
    let refused = |r: Result<()>| assert!(r.is_err(), "regression accepted");
    refused(step(&|n| n.generation = 3));
    refused(step(&|n| n.generation = 1));
    refused(step(&|n| n.token = a.token));
    refused(step(&|n| n.domain = [1; 16]));
    refused(step(&|n| n.incarnation = 2));
    refused(step(&|n| n.mode = Durability::Quorum));
    refused(step(&|n| n.schema_generation = 0));
    refused(step(&|n| n.publication_generation = 0));
    // Membership epoch and activation only rise.
    let mut b = a.clone();
    b.membership_epoch = 2;
    b.generation = 2;
    b.token = tok(3);
    assert_eq!(a.validate_successor(&b), Ok(()));
    let mut c = b.clone();
    c.generation = 3;
    c.token = tok(4);
    c.membership_epoch = 1;
    refused(b.validate_successor(&c));
    // The cover never falls and the inventory is only ever extended.
    let mut d = a.clone();
    d.generation = 2;
    d.token = tok(5);
    d.cover = 4;
    d.checkpoint = Some([5; 16]);
    d.tail.ordinal = 6;
    let mut e = d.clone();
    e.generation = 3;
    e.token = tok(6);
    e.cover = 3;
    refused(d.validate_successor(&e));
    let mut f = a.clone();
    f.generation = 2;
    f.token = tok(7);
    f.inventory[0].fence = 99;
    refused(a.validate_successor(&f));
    // An epoch cannot be rewritten by dropping then re-adding it.
    let two = established(8, 2);
    let mut g = two.clone();
    g.generation = 2;
    g.token = tok(9);
    g.inventory.truncate(1);
    g.writer_epoch = 1;
    refused(two.validate_successor(&g));
}

#[test]
fn activation_may_rise_and_the_new_activation_is_in_the_inventory() {
    let a = established(1, 1);
    let mut n = a.clone();
    n.generation = 2;
    n.token = tok(2);
    n.activation = 2;
    n.writer_epoch = 2;
    n.inventory.push(EpochEntry {
        epoch: 2,
        activation: 2,
        lane: 0,
        fence: 3,
    });
    assert_eq!(a.validate_successor(&n), Ok(()));
    // The head's activation must be the newest inventory entry's.
    let mut bad = n.clone();
    bad.inventory[1].activation = 1;
    assert!(bad.encode(&mut Vec::new()).is_err());
    let mut back = n.clone();
    back.generation = 3;
    back.token = tok(3);
    back.activation = 1;
    assert!(n.validate_successor(&back).is_err());
}

fn root() -> CatalogRoot {
    CatalogRoot {
        catalog: [3; 16],
        bucket_bits: 4,
        token: tok(1),
    }
}

fn entry(name: &str, state: NameState, n: u8, generation: u64) -> CatalogEntry {
    CatalogEntry {
        name: name.to_owned(),
        state,
        index: [n; 16],
        name_generation: generation,
    }
}

/// The `i`th name that hashes to bucket 2 of catalog `[3; 16]` at 4 bits.
fn nm(i: usize) -> String {
    (0..)
        .map(|k| format!("n{k}"))
        .filter(|n| bucket_of(&[3; 16], 4, n) == 2)
        .nth(i)
        .unwrap()
}

fn page(entries: Vec<CatalogEntry>) -> CatalogPage {
    CatalogPage {
        catalog: [3; 16],
        bucket: 2,
        generation: 1,
        entries,
        token: tok(5),
    }
}

#[test]
fn a_catalog_root_round_trips_and_bounds_its_fanout() {
    let b = enc(|o| root().encode(o));
    assert_eq!(CatalogRoot::decode(&b).unwrap(), root());
    assert_hardened(&b, CatalogRoot::decode);
    for bits in [0u8, MAX_CATALOG_BUCKET_BITS + 1] {
        let mut r = root();
        r.bucket_bits = bits;
        assert!(r.encode(&mut Vec::new()).is_err());
    }
}

#[test]
fn a_catalog_page_round_trips_sorted_and_hardened() {
    let p = page(vec![
        entry(&nm(0), NameState::Live, 1, 1),
        entry(&nm(1), NameState::Deleted, 2, 3),
        entry(&nm(2), NameState::Live, 4, 1),
    ]);
    let b = enc(|o| p.encode(o, 4));
    assert_eq!(CatalogPage::decode(&b, 4).unwrap(), p);
    assert_hardened(&b, |x| CatalogPage::decode(x, 4));
    // Bucket must fit the root's fan-out.
    let mut far = p.clone();
    far.bucket = 16;
    assert!(far.encode(&mut Vec::new(), 4).is_err());
}

#[test]
fn a_catalog_page_refuses_unsorted_duplicate_and_bad_names() {
    let bad = |entries: Vec<CatalogEntry>| page(entries).encode(&mut Vec::new(), 4).is_err();
    assert!(bad(vec![
        entry(&nm(1), NameState::Live, 1, 1),
        entry(&nm(0), NameState::Live, 2, 1)
    ]));
    assert!(bad(vec![
        entry(&nm(0), NameState::Live, 1, 1),
        entry(&nm(0), NameState::Live, 2, 1)
    ]));
    assert!(bad(vec![entry("", NameState::Live, 1, 1)]));
    assert!(bad(vec![entry("a\0b", NameState::Live, 1, 1)]));
    assert!(bad(vec![entry(
        &"x".repeat(MAX_INDEX_NAME_BYTES + 1),
        NameState::Live,
        1,
        1
    )]));
    assert!(bad(vec![entry(&nm(0), NameState::Live, 1, 0)]));
    // A name that hashes elsewhere is refused.
    let stray = (0..)
        .map(|k| format!("s{k}"))
        .find(|n| bucket_of(&[3; 16], 4, n) != 2)
        .unwrap();
    assert!(bad(vec![entry(&stray, NameState::Live, 1, 1)]));
}

#[test]
fn a_page_decoded_for_the_wrong_catalog_bucket_or_name_is_refused() {
    let p = page(vec![entry(&nm(0), NameState::Live, 1, 1)]);
    let b = enc(|o| p.encode(o, 4));
    assert!(CatalogPage::decode_at(&b, 4, &[3; 16], 2).is_ok());
    assert!(CatalogPage::decode_at(&b, 4, &[4; 16], 2).is_err());
    assert!(CatalogPage::decode_at(&b, 4, &[3; 16], 3).is_err());
}

#[test]
fn bucket_of_is_stable_spreads_names_and_is_bounded() {
    let c = [3u8; 16];
    assert_eq!(bucket_of(&c, 4, &nm(0)), bucket_of(&c, 4, &nm(0)));
    let mut seen = std::collections::BTreeSet::new();
    for i in 0..400 {
        let b = bucket_of(&c, 4, &format!("name-{i}"));
        assert!(b < 16);
        seen.insert(b);
    }
    assert_eq!(seen.len(), 16, "400 names reach every one of 16 buckets");
    assert!(bucket_of(&c, 16, "z") < 65536);
    assert_ne!(bucket_of(&c, 16, &nm(0)), bucket_of(&[4; 16], 16, &nm(0)));
}

#[test]
fn successors_of_a_page_follow_the_rule() {
    let a = page(vec![entry(&nm(0), NameState::Live, 1, 1)]);
    let mut n = a.clone();
    n.generation = 2;
    n.token = tok(6);
    n.entries[0].state = NameState::Deleted;
    n.entries[0].name_generation = 2;
    assert_eq!(a.validate_successor(&n), Ok(()));
    // A name's generation never falls and a Live → Live replacement must raise it.
    let mut same = a.clone();
    same.generation = 2;
    same.token = tok(6);
    same.entries[0].index = [9; 16];
    assert!(a.validate_successor(&same).is_err());
    // A deleted name may not return under the UUID it had (no resurrection).
    let del = n.clone();
    let mut back = del.clone();
    back.generation = 3;
    back.token = tok(7);
    back.entries[0].state = NameState::Live;
    back.entries[0].name_generation = 3;
    assert!(
        del.validate_successor(&back).is_err(),
        "same uuid came back"
    );
    back.entries[0].index = [2; 16];
    assert_eq!(del.validate_successor(&back), Ok(()));
    // Entries are never dropped.
    let mut drop_it = a.clone();
    drop_it.generation = 2;
    drop_it.token = tok(8);
    drop_it.entries.clear();
    assert!(a.validate_successor(&drop_it).is_err());
    // Generation steps by one with a new token.
    let mut skip = a.clone();
    skip.generation = 5;
    skip.token = tok(9);
    assert!(a.validate_successor(&skip).is_err());
    let mut same_token = a.clone();
    same_token.generation = 2;
    assert!(a.validate_successor(&same_token).is_err());
}

#[test]
fn authority_keys_are_derived_and_ordered() {
    let l = Layout::new("d").unwrap();
    let h = l.structural_head(&[1; 16], 1);
    assert!(h.ends_with("/AUTHORITY") && h.contains("/lanes/"));
    assert_ne!(h, l.lane_head(&[1; 16], 1));
    assert!(l.catalog_root(&[3; 16]).ends_with("/ROOT"));
    assert!(l.catalog_page(&[3; 16], 2) < l.catalog_page(&[3; 16], 10));
    assert_ne!(l.catalog_page(&[3; 16], 2), l.catalog_page(&[3; 16], 3));
}

#[test]
fn a_page_holds_at_most_its_bound_and_the_bound_fits_the_body_limit() {
    let names: Vec<String> = (0..)
        .map(|k| format!("n{k:05}"))
        .filter(|n| bucket_of(&[3; 16], 4, n) == 2)
        .take(MAX_CATALOG_PAGE_ENTRIES + 1)
        .collect();
    let all: Vec<CatalogEntry> = names
        .iter()
        .map(|n| entry(n, NameState::Live, 1, 1))
        .collect();
    let mut sorted = all.clone();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let full = page(sorted[..MAX_CATALOG_PAGE_ENTRIES].to_vec());
    enc(|o| full.encode(o, 4));
    let over = page(sorted);
    assert_eq!(
        over.encode(&mut Vec::new(), 4).unwrap_err(),
        FormatError::TooLarge("catalog entries")
    );
    // The largest legal page (longest names) still fits the kind's body bound.
    let long: Vec<String> = (0..)
        .map(|k| format!("{k:06}{}", "x".repeat(MAX_INDEX_NAME_BYTES - 6)))
        .filter(|n| bucket_of(&[3; 16], 4, n) == 2)
        .take(MAX_CATALOG_PAGE_ENTRIES)
        .collect();
    let mut entries: Vec<CatalogEntry> = long
        .iter()
        .map(|n| entry(n, NameState::Deleted, 2, 7))
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let b = enc(|o| page(entries).encode(o, 4));
    assert!(CatalogPage::decode(&b, 4).is_ok());
}
