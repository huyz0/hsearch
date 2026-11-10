//! Independent multipart replacement, ordering and selected-byte oracles.
use hsearch_object_probe::{codec, model::*, multipart};
fn part(id: u16, number: u64, seed: u64, length: u64) -> Specification {
    Specification {
        id,
        scenario: 5,
        action: "part".into(),
        parent: 1,
        offset: number,
        seed,
        length,
        ..Specification::default()
    }
}
#[test]
fn range_across_part_boundary_uses_selected_bytes_not_whole_object_or_etag() {
    let first = part(2, 1, 1, 5 * 1024 * 1024);
    let second = part(3, 2, 2, 4096);
    let actual = multipart::range_digest(&[&first, &second], 5 * 1024 * 1024 - 16, 32).unwrap();
    assert_eq!(
        actual,
        codec::payload_digest(&[(1, 16, 5 * 1024 * 1024 - 16), (2, 16, 0)]).unwrap()
    );
    assert_ne!(
        actual,
        codec::payload_digest(&[(1, first.length, 0)]).unwrap()
    );
    assert!(multipart::range_digest(&[&first, &second], u64::MAX, 32).is_none());
}
#[test]
fn last_replacement_and_numeric_part_order_define_completion_payload() {
    let plan = CapturePlan {
        binding: Binding {
            digests: [[0; 32]; 7],
            run: [0; 16],
            platform: 1,
        },
        bucket: "bucket".into(),
        specifications: vec![
            part(2, 1, 1, 5 * 1024 * 1024),
            part(3, 2, 2, 4096),
            part(4, 2, 3, 4096),
        ],
    };
    let completion = Specification {
        id: 5,
        parent: 1,
        action: "complete".into(),
        ..Specification::default()
    };
    let digest = multipart::completion_digest(&plan, &completion, 5 * 1024 * 1024 + 4096).unwrap();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "7b326c8167d8fb807ce969f5f4d81a027434734e4967d356e9110cf3acda7295"
    );
    assert!(multipart::completion_digest(&plan, &completion, 17).is_none());
    assert!(!multipart::verify(&plan, &Capture::default()));
}
