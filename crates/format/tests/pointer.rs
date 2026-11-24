//! M06.24: the base pointer's encoding and its successor rule.
mod common;
use common::patch_header;
use hsearch_format::{BasePointer, CommitToken, FormatError, Layout, PointerRefusal as R};

fn pointer(sequence: u64) -> BasePointer {
    BasePointer {
        index: [1; 16],
        shard: [2; 16],
        incarnation: 1,
        manifest: [3; 16],
        manifest_generation: 4,
        covered: 100,
        lane_domain: [5; 16],
        lane_incarnation: 1,
        lane_activation: 1,
        lane_number: 0,
        lane_cover: 40,
        sequence,
        token: CommitToken::new([7; 16]).unwrap(),
    }
}

fn encoded(p: &BasePointer) -> Vec<u8> {
    let mut out = Vec::new();
    p.encode(&mut out).unwrap();
    out
}

#[test]
fn a_pointer_round_trips_and_names_its_key() {
    let p = pointer(1);
    let bytes = encoded(&p);
    assert_eq!(BasePointer::decode(&bytes).unwrap(), p);
    assert_eq!(
        BasePointer::decode_for(&bytes, &[1; 16], &[2; 16], 1).unwrap(),
        p
    );
    for (index, shard, incarnation) in [
        ([9; 16], [2; 16], 1),
        ([1; 16], [9; 16], 1),
        ([1; 16], [2; 16], 2),
    ] {
        assert_eq!(
            BasePointer::decode_for(&bytes, &index, &shard, incarnation).unwrap_err(),
            FormatError::Mismatch("base pointer identity")
        );
    }
    let layout = Layout::new("local").unwrap();
    assert!(
        layout
            .base_pointer(&[1; 16], &[2; 16], 1)
            .ends_with("/base")
    );
}

#[test]
fn a_damaged_or_future_pointer_is_refused() {
    let bytes = encoded(&pointer(1));
    // One flipped body byte breaks the checksum.
    let mut damaged = bytes.clone();
    let last = damaged.len() - 1;
    damaged[last] ^= 1;
    assert!(BasePointer::decode(&damaged).is_err());
    // A newer layout version is refused, never read as this one.
    let newer = patch_header(&bytes, 8, &2u16.to_le_bytes());
    assert!(BasePointer::decode(&newer).is_err());
    // A zero sequence and an all-zero manifest are not pointers.
    for bad in [
        BasePointer {
            sequence: 0,
            ..pointer(1)
        },
        BasePointer {
            manifest: [0; 16],
            ..pointer(1)
        },
    ] {
        assert!(BasePointer::decode(&encoded(&bad)).is_err());
    }
}

#[test]
fn a_successor_cannot_lower_what_the_pointer_covers() {
    let old = pointer(1);
    let next = BasePointer {
        covered: 150,
        lane_cover: 90,
        manifest_generation: 5,
        ..pointer(2)
    };
    assert_eq!(next.validate_successor(Some(&old), 40), Ok(()));
    // The first pointer starts at 1.
    assert_eq!(old.validate_successor(None, 0), Ok(()));
    assert_eq!(pointer(2).validate_successor(None, 0), Err(R::Sequence));
    // One higher, exactly.
    assert_eq!(
        BasePointer {
            sequence: 3,
            ..next
        }
        .validate_successor(Some(&old), 0),
        Err(R::Sequence)
    );
    assert_eq!(
        BasePointer {
            sequence: 1,
            ..next
        }
        .validate_successor(Some(&old), 0),
        Err(R::Sequence)
    );
    // Nothing the log covers, the base's generation or the lane cover may fall.
    assert_eq!(
        BasePointer {
            covered: 99,
            ..next
        }
        .validate_successor(Some(&old), 0),
        Err(R::Covered)
    );
    assert_eq!(
        BasePointer {
            manifest_generation: 3,
            ..next
        }
        .validate_successor(Some(&old), 0),
        Err(R::Generation)
    );
    assert_eq!(
        BasePointer {
            lane_cover: 39,
            ..next
        }
        .validate_successor(Some(&old), 0),
        Err(R::LaneCover)
    );
    // Another index, shard or incarnation is not a successor.
    assert_eq!(
        BasePointer {
            index: [9; 16],
            ..next
        }
        .validate_successor(Some(&old), 0),
        Err(R::Identity)
    );
    assert_eq!(
        BasePointer {
            shard: [9; 16],
            ..next
        }
        .validate_successor(Some(&old), 0),
        Err(R::Identity)
    );
    assert_eq!(
        BasePointer {
            incarnation: 2,
            ..next
        }
        .validate_successor(Some(&old), 0),
        Err(R::Identity)
    );
    // A lane cover below the head's cover points at slots a reclaim may have removed.
    assert_eq!(
        next.validate_successor(Some(&old), 91),
        Err(R::BelowHeadCover)
    );
}

#[test]
fn a_promoted_index_may_start_a_lower_cover_on_its_new_lane() {
    // A successor lane's ordinals restart, so the cover is only compared on the same lane.
    let old = pointer(1);
    let promoted = BasePointer {
        lane_domain: [8; 16],
        lane_cover: 0,
        covered: 120,
        manifest_generation: 5,
        ..pointer(2)
    };
    assert_eq!(promoted.validate_successor(Some(&old), 0), Ok(()));
}
