//! Golden corpus written by `fixtures/formats/gen.py`, an independent encoder: the Rust encoder must
//! reproduce its bytes, the Rust decoder must read them, and the refusal files must be refused for
//! the stated reason.
use hsearch_format::*;
use sha2::Digest as _;
use std::path::PathBuf;

fn read(rel: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/formats")
        .join(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn id(n: u8) -> Id {
    [n; 16]
}

fn tok(n: u8) -> CommitToken {
    CommitToken::new([n; 16]).unwrap()
}

fn enc(f: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> Vec<u8> {
    let mut v = Vec::new();
    f(&mut v).unwrap();
    v
}

#[test]
fn fleet_matches_the_independent_encoder() {
    let r = FleetRecord {
        generation: 7,
        write_format: 3,
        min_read_format: 1,
        required_caps: 0b10,
        token: tok(9),
    };
    let golden = read("good/fleet.bin");
    assert_eq!(enc(|o| r.encode(o)), golden);
    assert_eq!(FleetRecord::decode(&golden).unwrap(), r);
}

#[test]
fn lane_matches_the_independent_encoder() {
    let d = LaneDescriptor {
        generation: 1,
        domain: id(0x0D),
        policy: PolicyTuple {
            key: KeyIdentity::DeploymentDefault,
            residency: "eu-west-1".into(),
            durability: Durability::Object,
            format_version: 1,
        },
        group_id: id(0xAA),
        group_ceiling: DEFAULT_LANE_GROUP,
        members: vec![id(1), id(2), id(9)],
        token: tok(1),
    };
    let golden = read("good/lane.bin");
    assert_eq!(enc(|o| d.encode(o)), golden);
    assert_eq!(LaneDescriptor::decode(&golden).unwrap(), d);
}

#[test]
fn slots_match_the_independent_encoder() {
    let payload = br#"{"title":"hello"}"#;
    let recs = [
        RecordSpec {
            index: id(1),
            seq: 10,
            op: RecordOp::Put,
            payload: PayloadRef::Inline(payload),
        },
        RecordSpec {
            index: id(2),
            seq: 11,
            op: RecordOp::Delete,
            payload: PayloadRef::None,
        },
        RecordSpec {
            index: id(1),
            seq: 12,
            op: RecordOp::Put,
            payload: PayloadRef::External {
                object: id(8),
                length: 200_000_000,
                digest: [5; 32],
            },
        },
    ];
    let at = |ordinal| SlotPosition {
        domain: id(3),
        activation: 2,
        lane: 1,
        ordinal,
    };
    let golden = read("good/slot-data.bin");
    assert_eq!(
        enc(|o| encode_slot(o, &at(41), tok(6), &SlotBody::Data(&recs))),
        golden
    );
    assert_eq!(
        SlotView::parse(&golden)
            .unwrap()
            .records()
            .collect::<Vec<_>>(),
        recs
    );
    let fence = read("good/slot-fence.bin");
    assert_eq!(
        enc(|o| encode_slot(
            o,
            &at(5),
            tok(2),
            &SlotBody::Fence {
                successor_epoch: 12
            }
        )),
        fence
    );
    assert_eq!(SlotView::parse(&fence).unwrap().successor_epoch, Some(12));
}

#[test]
fn manifests_match_the_independent_encoder() {
    let e = |name: &str, n: u8| FileEntry {
        name: name.into(),
        object: id(n),
        length: u64::from(n) * 1000,
        digest: [n; 32],
    };
    let page = ManifestPage {
        entries: vec![e("_0.cfs", 1), e("_0.si", 2), e("segments_1", 3)],
    };
    let golden_page = read("good/manifest-page.bin");
    assert_eq!(enc(|o| page.encode(o)), golden_page);
    assert_eq!(ManifestPage::decode(&golden_page).unwrap(), page);
    let root = ManifestRoot {
        generation: 3,
        index: id(1),
        engine_format: "lucene-10.3".into(),
        mapping: [7; 32],
        covered: 99,
        crypto_envelope: id(4),
        pages: vec![PageRef {
            page: id(8),
            entry_count: 3,
            digest: sha2::Sha256::digest(&golden_page).into(),
        }],
        token: tok(3),
    };
    let golden_root = read("good/manifest-root.bin");
    assert_eq!(enc(|o| root.encode(o)), golden_root);
    assert_eq!(ManifestRoot::decode(&golden_root).unwrap(), root);
    assert_eq!(root.verify_page(0, &golden_page).unwrap(), page);
}

#[test]
fn mapping_pin_and_cursor_match_the_independent_encoder() {
    let m = MappingObject::new(br#"{"properties":{"a":{"type":"keyword"}}}"#).unwrap();
    let g = read("good/mapping.bin");
    assert_eq!(enc(|o| m.encode(o)), g);
    assert_eq!(MappingObject::decode(&g).unwrap(), m);

    let p = PinRecord {
        pin: id(1),
        index: id(2),
        manifest: id(3),
        manifest_generation: 5,
        expires_unix_ms: 1_700_000_000_000,
        token: tok(1),
    };
    let g = read("good/pin.bin");
    assert_eq!(enc(|o| p.encode(o)), g);
    assert_eq!(PinRecord::decode(&g).unwrap(), p);

    let c = Cursor {
        index: id(1),
        routing_generation: 4,
        body: CursorBody::Vector(vec![(id(0), 0), (id(1), 10)]),
    };
    let g = read("good/cursor.bin");
    assert_eq!(enc(|o| c.encode(o)), g);
    assert_eq!(Cursor::decode(&g).unwrap(), c);
}

#[test]
fn refusal_corpus_is_refused_for_the_stated_reason() {
    let cases = [
        (
            "unknown-envelope-version",
            FormatError::UnsupportedEnvelope(2),
        ),
        (
            "unknown-kind-version",
            FormatError::UnknownVersion {
                kind: 1,
                version: 2,
            },
        ),
        ("unknown-kind", FormatError::UnknownKind(99)),
        (
            "unknown-mandatory-capability-high",
            FormatError::UnknownMandatoryCapability(1 << 63),
        ),
        (
            "unknown-mandatory-capability",
            FormatError::UnknownMandatoryCapability(1),
        ),
        ("forged-length-4gib", FormatError::TooLarge("body")),
        ("trailing-byte", FormatError::Trailing),
        ("truncated", FormatError::Truncated),
    ];
    for (name, want) in cases {
        assert_eq!(
            FleetRecord::decode(&read(&format!("bad/{name}.bin"))).unwrap_err(),
            want,
            "{name}"
        );
    }
}
