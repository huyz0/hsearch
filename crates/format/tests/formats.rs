//! Format hardening: round trips, every-byte truncation and corruption, forged lengths, limits.
mod common;
use common::*;
use hsearch_format::*;

fn tok(n: u8) -> CommitToken {
    CommitToken::new([n; 16]).unwrap()
}

fn id(n: u8) -> Id {
    [n; 16]
}

fn fleet(gen_: u64, write: u16, token: u8) -> FleetRecord {
    FleetRecord {
        generation: gen_,
        write_format: write,
        min_read_format: 1,
        required_caps: 0,
        token: tok(token),
    }
}

fn enc<T>(f: impl FnOnce(&mut Vec<u8>) -> Result<T>) -> Vec<u8> {
    let mut v = Vec::new();
    f(&mut v).unwrap();
    v
}

// ---- envelope -------------------------------------------------------------------------------

#[test]
fn unknown_mandatory_versions_fail() {
    let b = enc(|o| fleet(1, 1, 1).encode(o));
    // Envelope layout version 2.
    let e = patch_header(&b, 4, &2u16.to_le_bytes());
    assert_eq!(
        FleetRecord::decode(&e).unwrap_err(),
        FormatError::UnsupportedEnvelope(2)
    );
    // Kind version 2 and 0.
    let v2 = patch_header(&b, 8, &2u16.to_le_bytes());
    assert_eq!(
        FleetRecord::decode(&v2).unwrap_err(),
        FormatError::UnknownVersion {
            kind: 1,
            version: 2
        }
    );
    let v0 = patch_header(&b, 8, &0u16.to_le_bytes());
    assert_eq!(
        FleetRecord::decode(&v0).unwrap_err(),
        FormatError::UnknownVersion {
            kind: 1,
            version: 0
        }
    );
    // Unknown kind.
    let k = patch_header(&b, 6, &99u16.to_le_bytes());
    assert_eq!(
        FleetRecord::decode(&k).unwrap_err(),
        FormatError::UnknownKind(99)
    );
    // Unknown mandatory capability: refused with the unknown bits named.
    let c = patch_header(&b, 12, &0b101u64.to_le_bytes());
    assert_eq!(
        FleetRecord::decode(&c).unwrap_err(),
        FormatError::UnknownMandatoryCapability(0b101)
    );
    // Wrong kind for the decoder.
    let pin = enc(|o| {
        PinRecord {
            pin: id(1),
            index: id(2),
            manifest: id(3),
            manifest_generation: 1,
            expires_unix_ms: 9,
            token: tok(4),
        }
        .encode(o)
    });
    assert_eq!(
        FleetRecord::decode(&pin).unwrap_err(),
        FormatError::Mismatch("object kind")
    );
    assert_eq!(kind_of(&pin), Kind::Pin);
}

#[test]
fn header_bounds_the_body_before_it_is_read() {
    let b = enc(|o| fleet(1, 1, 1).encode(o));
    // A forged 4 GiB length is refused from the 40 header bytes alone: no body is needed.
    let huge = patch_header(&b, 28, &u32::MAX.to_le_bytes());
    assert_eq!(
        Header::parse(&huge[..Header::LEN]).unwrap_err(),
        FormatError::TooLarge("body")
    );
    let just_over = patch_header(&b, 28, &((Kind::Fleet.max_body() + 1) as u32).to_le_bytes());
    assert_eq!(
        Header::parse(&just_over).unwrap_err(),
        FormatError::TooLarge("body")
    );
    let ok = Header::parse(&b[..Header::LEN]).unwrap();
    assert_eq!(ok.total_len(), b.len());
    assert_eq!(
        Header::parse(&b[..Header::LEN - 1]).unwrap_err(),
        FormatError::Truncated
    );
    let mut bad = b.clone();
    bad[0] = b'X';
    assert_eq!(Header::parse(&bad).unwrap_err(), FormatError::BadMagic);
}

#[test]
fn encoders_leave_the_buffer_untouched_on_refusal() {
    let mut out = vec![7u8; 5];
    let rec = [RecordSpec {
        index: id(1),
        seq: 1,
        op: RecordOp::Put,
        payload: PayloadRef::Inline(&vec![0u8; MAX_INLINE_RECORD_BYTES + 1]),
    }];
    let at = SlotPosition {
        domain: id(1),
        activation: 1,
        lane: 0,
        ordinal: 0,
    };
    assert!(encode_slot(&mut out, &at, tok(1), &SlotBody::Data(&rec)).is_err());
    assert_eq!(out, vec![7u8; 5]);
    assert_eq!(
        encode_slot(&mut out, &at, tok(1), &SlotBody::Data(&[])).unwrap_err(),
        FormatError::Malformed("empty data slot")
    );
    assert_eq!(out, vec![7u8; 5]);
}

#[test]
fn an_etag_is_not_a_digest() {
    // The only content hash is SHA-256 of the canonical bytes: it is neither the envelope's
    // checksum nor the hash of the stored object, which is what a provider might call an ETag.
    let m = MappingObject::new(br#"{"properties":{"a":{"type":"keyword"}}}"#).unwrap();
    let bytes = enc(|o| m.encode(o));
    assert_ne!(&m.hash[..4], &crc32c(&bytes).to_le_bytes());
    let by_name = MappingObject::decode_expecting(&bytes, &m.hash).unwrap();
    assert_eq!(by_name, m);
    // Pretending the object's own digest is the name does not match.
    let wrong: Digest = {
        use sha2::Digest as _;
        sha2::Sha256::digest(&bytes).into()
    };
    assert_eq!(
        MappingObject::decode_expecting(&bytes, &wrong).unwrap_err(),
        FormatError::Mismatch("mapping hash")
    );
}

// ---- fleet ----------------------------------------------------------------------------------

#[test]
fn fleet_roundtrip_and_hardening() {
    let r = FleetRecord {
        required_caps: 0b10,
        ..fleet(7, 3, 9)
    };
    let b = enc(|o| r.encode(o));
    assert_eq!(FleetRecord::decode(&b).unwrap(), r);
    assert_hardened(&b, FleetRecord::decode);
}

#[test]
fn fleet_rejects_invalid_fields() {
    for bad in [
        FleetRecord {
            generation: 0,
            ..fleet(1, 1, 1)
        },
        FleetRecord {
            min_read_format: 0,
            ..fleet(1, 1, 1)
        },
        FleetRecord {
            min_read_format: 2,
            ..fleet(1, 1, 1)
        },
    ] {
        assert!(bad.encode(&mut Vec::new()).is_err());
    }
    assert!(CommitToken::new([0; 16]).is_err());
    let b = enc(|o| fleet(1, 1, 1).encode(o));
    // A zero token, generation 0 and min_read 0: forged bodies with valid checksums.
    for at in [20usize, 0, 10] {
        let f = forge(&b, |body| {
            if at == 20 {
                body[20..36].fill(0)
            } else {
                body[at..at + 2].fill(0)
            }
        });
        assert!(FleetRecord::decode(&f).is_err(), "forged at {at}");
    }
}

#[test]
fn a_node_that_cannot_honour_the_fleet_refuses_explicitly() {
    let node = NodeSupport {
        min_read_format: 1,
        max_read_format: 2,
        max_write_format: 2,
        caps: 0b1,
    };
    assert_eq!(fleet(1, 2, 1).admit(&node), Ok(2));
    // Mid-roll: fleet may write 3 only after every node reads 3; an N node must not serve.
    assert_eq!(
        fleet(2, 3, 2).admit(&node),
        Err(Refusal::FleetWritesNewerThanNodeReads)
    );
    // A binary that dropped format 1 must not join a fleet that still holds format-1 objects.
    let modern = NodeSupport {
        min_read_format: 2,
        ..node
    };
    assert_eq!(
        fleet(1, 2, 1).admit(&modern),
        Err(Refusal::NodeCannotReadOldestFormat)
    );
    let migrated = FleetRecord {
        min_read_format: 2,
        ..fleet(2, 2, 2)
    };
    assert_eq!(migrated.admit(&modern), Ok(2));
    let caps = FleetRecord {
        required_caps: 0b110,
        ..fleet(2, 2, 3)
    };
    assert_eq!(
        caps.admit(&node),
        Err(Refusal::UnknownMandatoryCapability(0b110))
    );
    // A node whose write format is below the fleet's readable minimum is refused, never 0.
    let high_min = FleetRecord {
        min_read_format: 3,
        write_format: 3,
        ..fleet(3, 3, 3)
    };
    let low_writer = NodeSupport {
        min_read_format: 1,
        max_read_format: 3,
        max_write_format: 2,
        caps: 0,
    };
    assert_eq!(
        high_min.admit(&low_writer),
        Err(Refusal::NodeWritesBelowReadableMinimum)
    );
    assert!(
        high_min
            .admit(&NodeSupport {
                max_write_format: 0,
                ..low_writer
            })
            .is_err()
    );
    // A node that reads 3 but writes 2 keeps writing 2 until it is upgraded.
    let n3 = NodeSupport {
        min_read_format: 1,
        max_read_format: 3,
        max_write_format: 2,
        caps: 0,
    };
    assert_eq!(fleet(2, 3, 2).admit(&n3), Ok(2));
}

#[test]
fn fleet_successor_rules() {
    let a = fleet(1, 2, 1);
    a.validate_successor(&fleet(2, 3, 2)).unwrap();
    assert!(
        a.validate_successor(&fleet(3, 3, 2)).is_err(),
        "generation gap"
    );
    assert!(
        a.validate_successor(&fleet(1, 3, 2)).is_err(),
        "generation repeat"
    );
    assert!(
        a.validate_successor(&fleet(2, 2, 1)).is_err(),
        "same token with different bytes is still refused"
    );
    // Raising write_format is the point of no return: it can never fall afterwards, and the
    // required capabilities can only grow.
    assert!(
        a.validate_successor(&fleet(2, 1, 2)).is_err(),
        "write format fell"
    );
    let capped = FleetRecord {
        required_caps: 0b11,
        ..fleet(2, 2, 2)
    };
    a.validate_successor(&capped).unwrap();
    let dropped = FleetRecord {
        required_caps: 0b01,
        ..fleet(3, 2, 3)
    };
    assert!(
        capped.validate_successor(&dropped).is_err(),
        "capability removed"
    );
    let low = FleetRecord {
        min_read_format: 1,
        ..fleet(2, 2, 2)
    };
    let high = FleetRecord {
        min_read_format: 2,
        ..fleet(1, 2, 1)
    };
    assert!(
        high.validate_successor(&low).is_err(),
        "minimum readable format cannot fall"
    );
}

#[test]
fn identical_content_with_a_different_token_has_different_bytes() {
    // M03.11: the stale writer and the current writer never produce equal bytes.
    let a = enc(|o| fleet(4, 2, 1).encode(o));
    let b = enc(|o| fleet(4, 2, 2).encode(o));
    assert_ne!(a, b);
    assert_eq!(a.len(), b.len());
}

// ---- lanes ----------------------------------------------------------------------------------

fn policy() -> PolicyTuple {
    PolicyTuple {
        key: KeyIdentity::DeploymentDefault,
        residency: "eu-west-1".into(),
        durability: Durability::Object,
        format_version: 1,
    }
}

fn lane(members: Vec<Id>) -> LaneDescriptor {
    LaneDescriptor {
        generation: 1,
        domain: id(3),
        policy: policy(),
        group_id: id(0xAA),
        group_ceiling: DEFAULT_LANE_GROUP,
        members,
        token: tok(1),
    }
}

#[test]
fn lane_roundtrip_and_hardening() {
    let d = lane(vec![id(1), id(2), id(9)]);
    let b = enc(|o| d.encode(o));
    assert_eq!(LaneDescriptor::decode(&b).unwrap(), d);
    assert_hardened(&b, LaneDescriptor::decode);
    let q = LaneDescriptor {
        policy: PolicyTuple {
            durability: Durability::Quorum,
            ..policy()
        },
        ..d
    };
    assert_eq!(LaneDescriptor::decode(&enc(|o| q.encode(o))).unwrap(), q);
}

#[test]
fn lane_membership_rules() {
    // A BYOK key never wraps a shared lane; one member is fine, two are refused.
    let byok = |m: Vec<Id>| LaneDescriptor {
        policy: PolicyTuple {
            key: KeyIdentity::Byok("kms:abc".into()),
            ..policy()
        },
        members: m,
        ..lane(vec![])
    };
    assert!(byok(vec![id(1)]).encode(&mut Vec::new()).is_ok());
    assert!(byok(vec![id(1), id(2)]).encode(&mut Vec::new()).is_err());
    // Unsorted and duplicate members are refused, so equal sets have equal bytes.
    assert!(lane(vec![id(2), id(1)]).encode(&mut Vec::new()).is_err());
    assert!(lane(vec![id(1), id(1)]).encode(&mut Vec::new()).is_err());
    // Ceiling.
    let mut d = lane(vec![id(1), id(2)]);
    d.group_ceiling = 1;
    assert_eq!(
        d.encode(&mut Vec::new()).unwrap_err(),
        FormatError::TooLarge("lane members")
    );
    d.group_ceiling = MAX_LANE_GROUP + 1;
    assert!(d.encode(&mut Vec::new()).is_err());
    // can_host demands an identical tuple and room.
    let full = LaneDescriptor {
        group_ceiling: 2,
        ..lane(vec![id(1), id(2)])
    };
    assert!(!full.can_host(&policy()));
    let open = lane(vec![id(1)]);
    assert!(open.can_host(&policy()));
    assert!(!open.can_host(&PolicyTuple {
        residency: "us-east-1".into(),
        ..policy()
    }));
    assert!(!open.can_host(&PolicyTuple {
        format_version: 2,
        ..policy()
    }));
    assert!(!open.can_host(&PolicyTuple {
        key: KeyIdentity::Byok("k".into()),
        ..policy()
    }));
    // Joining keeps order, bumps the generation and demands a fresh token.
    let j = open.with_member(id(0), tok(2)).unwrap();
    assert_eq!((j.generation, j.members.clone()), (2, vec![id(0), id(1)]));
    assert!(open.with_member(id(1), tok(2)).is_err());
    assert!(open.with_member(id(5), tok(1)).is_err());
    assert!(full.with_member(id(7), tok(3)).is_err());
}

#[test]
fn lane_forged_counts_cost_nothing() {
    let b = enc(|o| lane(vec![id(1)]).encode(o));
    // The member count sits after: gen 8, domain 16, key kind 1, key len 2, residency 1+9, durability 1, version 2, group 16, ceiling 4.
    let at = 8 + 16 + 1 + 2 + 1 + 9 + 1 + 2 + 16 + 4;
    for forged in [u32::MAX, MAX_LANE_GROUP + 1, 2] {
        let f = forge(&b, |body| {
            body[at..at + 4].copy_from_slice(&forged.to_le_bytes())
        });
        assert!(LaneDescriptor::decode(&f).is_err(), "count {forged}");
    }
}

fn slot_at(ordinal: u64) -> SlotPosition {
    SlotPosition {
        domain: id(3),
        activation: 2,
        lane: 1,
        ordinal,
    }
}

fn sample_records<'a>(inline: &'a [u8]) -> Vec<RecordSpec<'a>> {
    vec![
        RecordSpec {
            index: id(1),
            seq: 10,
            op: RecordOp::Put,
            payload: PayloadRef::Inline(inline),
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
    ]
}

#[test]
fn slot_roundtrip_hardening_and_zero_copy() {
    let payload = br#"{"title":"hello"}"#;
    let recs = sample_records(payload);
    let b = enc(|o| encode_slot(o, &slot_at(41), tok(6), &SlotBody::Data(&recs)));
    let v = SlotView::parse(&b).unwrap();
    assert_eq!(
        (v.kind, v.len(), v.position, v.token),
        (SlotKind::Data, 3, slot_at(41), tok(6))
    );
    let got: Vec<_> = v.records().collect();
    assert_eq!(got, recs);
    // The inline payload is a borrow of the input buffer, not a copy.
    let PayloadRef::Inline(p) = got[0].payload else {
        panic!()
    };
    let range = b.as_ptr_range();
    assert!(range.contains(&p.as_ptr()));
    assert_hardened(&b, |x| {
        SlotView::parse(x).map(|v| v.records().collect::<Vec<_>>().len())
    });
}

#[test]
fn fence_slot_roundtrip_and_hardening() {
    let b = enc(|o| {
        encode_slot(
            o,
            &slot_at(5),
            tok(2),
            &SlotBody::Fence {
                successor_epoch: 12,
            },
        )
    });
    let v = SlotView::parse(&b).unwrap();
    assert_eq!(
        (v.kind, v.successor_epoch, v.is_empty()),
        (SlotKind::Fence, Some(12), true)
    );
    assert_eq!(v.records().count(), 0);
    assert_hardened(&b, |x| SlotView::parse(x).map(|v| v.successor_epoch));
}

#[test]
fn slot_document_limits_are_exact() {
    let ext = |length| {
        [RecordSpec {
            index: id(1),
            seq: 1,
            op: RecordOp::Put,
            payload: PayloadRef::External {
                object: id(1),
                length,
                digest: [0; 32],
            },
        }]
    };
    let try_ext = |length| {
        encode_slot(
            &mut Vec::new(),
            &slot_at(0),
            tok(1),
            &SlotBody::Data(&ext(length)),
        )
    };
    try_ext(MAX_DOCUMENT_BYTES).unwrap();
    assert_eq!(
        try_ext(MAX_DOCUMENT_BYTES + 1).unwrap_err(),
        FormatError::TooLarge("document")
    );
    assert!(try_ext(0).is_err());
    let max_inline = vec![1u8; MAX_INLINE_RECORD_BYTES];
    let one = |p: &[u8]| {
        encode_slot(
            &mut Vec::new(),
            &slot_at(0),
            tok(1),
            &SlotBody::Data(&[RecordSpec {
                index: id(1),
                seq: 1,
                op: RecordOp::Put,
                payload: PayloadRef::Inline(p),
            }]),
        )
    };
    one(&max_inline).unwrap();
    assert!(one(&[]).is_err());
    // Op and payload must agree.
    let bad = [RecordSpec {
        index: id(1),
        seq: 1,
        op: RecordOp::Delete,
        payload: PayloadRef::Inline(b"x"),
    }];
    assert!(encode_slot(&mut Vec::new(), &slot_at(0), tok(1), &SlotBody::Data(&bad)).is_err());
    // A slot over the body ceiling is refused: nine 1 MiB records are 9 MiB.
    let many: Vec<_> = (0..9)
        .map(|i| RecordSpec {
            index: id(1),
            seq: i,
            op: RecordOp::Put,
            payload: PayloadRef::Inline(&max_inline),
        })
        .collect();
    let mut out = Vec::new();
    assert_eq!(
        encode_slot(&mut out, &slot_at(0), tok(1), &SlotBody::Data(&many)).unwrap_err(),
        FormatError::TooLarge("body")
    );
    assert!(out.is_empty());
    // MAX_SLOT_RECORDS + 1 records.
    let tiny: Vec<_> = (0..=MAX_SLOT_RECORDS as u64)
        .map(|i| RecordSpec {
            index: id(1),
            seq: i,
            op: RecordOp::Delete,
            payload: PayloadRef::None,
        })
        .collect();
    assert_eq!(
        encode_slot(&mut Vec::new(), &slot_at(0), tok(1), &SlotBody::Data(&tiny)).unwrap_err(),
        FormatError::TooLarge("slot records")
    );
}

#[test]
fn slot_forged_lengths_are_refused_before_allocation() {
    let recs = sample_records(b"abc");
    let b = enc(|o| encode_slot(o, &slot_at(1), tok(1), &SlotBody::Data(&recs)));
    // Body: domain 16, activation 8, lane 4, ordinal 8, token 16, kind 1, count 4, then records.
    let count_at = 16 + 8 + 4 + 8 + 16 + 1;
    for forged in [0u32, 4, u32::MAX, MAX_SLOT_RECORDS as u32 + 1] {
        let f = forge(&b, |body| {
            body[count_at..count_at + 4].copy_from_slice(&forged.to_le_bytes())
        });
        assert!(SlotView::parse(&f).is_err(), "count {forged}");
    }
    // First record: id 16, seq 8, op 1, kind 1, inline length 4.
    let len_at = count_at + 4 + 16 + 8 + 1 + 1;
    for forged in [0u32, 4, u32::MAX, MAX_INLINE_RECORD_BYTES as u32 + 1] {
        let f = forge(&b, |body| {
            body[len_at..len_at + 4].copy_from_slice(&forged.to_le_bytes())
        });
        assert!(SlotView::parse(&f).is_err(), "inline length {forged}");
    }
    // The external record's length is the u64 after its object id.
    let ext_len_at = b.len() - Header::LEN - 32 - 8;
    let f = forge(&b, |body| {
        body[ext_len_at..ext_len_at + 8].copy_from_slice(&(MAX_DOCUMENT_BYTES + 1).to_le_bytes())
    });
    assert_eq!(
        SlotView::parse(&f).unwrap_err(),
        FormatError::TooLarge("document")
    );
    // Unknown slot kind and record op.
    let f = forge(&b, |body| body[count_at - 1] = 9);
    assert!(SlotView::parse(&f).is_err());
    let f = forge(&b, |body| body[count_at + 4 + 24] = 7);
    assert!(SlotView::parse(&f).is_err());
}

#[test]
fn aad_binds_the_index_and_position() {
    let base = aad(&id(1), 2, 7, 3, 4, &id(5));
    assert_ne!(base, aad(&id(1), 2, 7, 3, 4, &id(6)), "index id");
    assert_ne!(base, aad(&id(9), 2, 7, 3, 4, &id(5)), "domain");
    assert_ne!(base, aad(&id(1), 9, 7, 3, 4, &id(5)), "activation");
    assert_ne!(base, aad(&id(1), 2, 9, 3, 4, &id(5)), "lane");
    assert_ne!(base, aad(&id(1), 2, 7, 9, 4, &id(5)), "ordinal");
    assert_ne!(base, aad(&id(1), 2, 7, 3, 9, &id(5)), "record position");
    assert_eq!(base.len(), 72);
}

// ---- manifests ------------------------------------------------------------------------------

fn entry(name: &str, n: u8) -> FileEntry {
    FileEntry {
        name: name.into(),
        object: id(n),
        length: u64::from(n) * 1000,
        digest: [n; 32],
    }
}

#[test]
fn manifest_page_and_root() {
    let page = ManifestPage {
        entries: vec![
            entry("_0.cfs", 1),
            entry("_0.si", 2),
            entry("segments_1", 3),
        ],
    };
    let pb = enc(|o| page.encode(o));
    assert_eq!(ManifestPage::decode(&pb).unwrap(), page);
    assert_hardened(&pb, ManifestPage::decode);

    use sha2::Digest as _;
    let digest: Digest = sha2::Sha256::digest(&pb).into();
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
            digest,
        }],
        token: tok(3),
    };
    let rb = enc(|o| root.encode(o));
    assert_eq!(ManifestRoot::decode(&rb).unwrap(), root);
    assert_hardened(&rb, ManifestRoot::decode);
    // Page verification: digest first, then count.
    assert_eq!(root.verify_page(0, &pb).unwrap(), page);
    assert_eq!(
        root.verify_page(1, &pb).unwrap_err(),
        FormatError::Mismatch("page index")
    );
    let mut tampered = pb.clone();
    *tampered.last_mut().unwrap() ^= 1;
    assert_eq!(
        root.verify_page(0, &tampered).unwrap_err(),
        FormatError::Checksum
    );
    let wrong_count = ManifestRoot {
        pages: vec![PageRef {
            entry_count: 2,
            ..root.pages[0]
        }],
        ..root.clone()
    };
    assert_eq!(
        wrong_count.verify_page(0, &pb).unwrap_err(),
        FormatError::Mismatch("page entry count")
    );
}

#[test]
fn manifest_bounds_and_names() {
    for bad in [
        "",
        ".",
        "..",
        "a/b",
        "a\0b",
        &"x".repeat(MAX_FILE_NAME_BYTES + 1),
    ] {
        assert!(
            ManifestPage {
                entries: vec![entry(bad, 1)]
            }
            .encode(&mut Vec::new())
            .is_err(),
            "{bad:?}"
        );
    }
    assert!(
        ManifestPage {
            entries: vec![entry(&"x".repeat(MAX_FILE_NAME_BYTES), 1)]
        }
        .encode(&mut Vec::new())
        .is_ok()
    );
    assert!(
        ManifestPage {
            entries: vec![entry("b", 1), entry("a", 2)]
        }
        .encode(&mut Vec::new())
        .is_err(),
        "unsorted"
    );
    assert!(
        ManifestPage {
            entries: vec![entry("a", 1), entry("a", 2)]
        }
        .encode(&mut Vec::new())
        .is_err(),
        "duplicate"
    );
    let full = ManifestPage {
        entries: (0..MAX_PAGE_ENTRIES)
            .map(|i| entry(&format!("f{i:05}"), 1))
            .collect(),
    };
    assert!(full.encode(&mut Vec::new()).is_ok());
    let over = ManifestPage {
        entries: (0..=MAX_PAGE_ENTRIES)
            .map(|i| entry(&format!("f{i:05}"), 1))
            .collect(),
    };
    assert_eq!(
        over.encode(&mut Vec::new()).unwrap_err(),
        FormatError::TooLarge("page entries")
    );
    // Worst-case page fits the page body bound.
    let worst = ManifestPage {
        entries: (0..MAX_PAGE_ENTRIES)
            .map(|i| entry(&format!("{i:05}{}", "x".repeat(MAX_FILE_NAME_BYTES - 5)), 1))
            .collect(),
    };
    assert!(worst.encode(&mut Vec::new()).is_ok());
    // Forged entry count on a real page.
    let pb = enc(|o| {
        ManifestPage {
            entries: vec![entry("a", 1)],
        }
        .encode(o)
    });
    for forged in [0u32, 2, u32::MAX, MAX_PAGE_ENTRIES as u32 + 1] {
        let f = forge(&pb, |b| b[0..4].copy_from_slice(&forged.to_le_bytes()));
        assert!(ManifestPage::decode(&f).is_err(), "count {forged}");
    }
    // Root with the maximum pages and worst-case name fits its body bound.
    let root = |n: usize| ManifestRoot {
        generation: 1,
        index: id(1),
        engine_format: "x".repeat(MAX_NAME_BYTES),
        mapping: [0; 32],
        covered: 0,
        crypto_envelope: id(1),
        pages: vec![
            PageRef {
                page: id(1),
                entry_count: 1,
                digest: [0; 32]
            };
            n
        ],
        token: tok(1),
    };
    assert!(root(MAX_ROOT_PAGES).encode(&mut Vec::new()).is_ok());
    assert_eq!(
        root(MAX_ROOT_PAGES + 1)
            .encode(&mut Vec::new())
            .unwrap_err(),
        FormatError::TooLarge("root pages")
    );
    assert!(
        ManifestRoot {
            engine_format: String::new(),
            ..root(1)
        }
        .encode(&mut Vec::new())
        .is_err()
    );
    let rb = enc(|o| root(1).encode(o));
    let at = 8 + 16 + 1 + MAX_NAME_BYTES + 32 + 8 + 16;
    for forged in [u32::MAX, MAX_ROOT_PAGES as u32 + 1, 2] {
        let f = forge(&rb, |b| {
            b[at..at + 4].copy_from_slice(&forged.to_le_bytes())
        });
        assert!(ManifestRoot::decode(&f).is_err(), "pages {forged}");
    }
}

// ---- mapping, pin, cursor -------------------------------------------------------------------

#[test]
fn mapping_roundtrip_hash_and_bounds() {
    let m = MappingObject::new(b"canonical-mapping-bytes").unwrap();
    let b = enc(|o| m.encode(o));
    assert_eq!(MappingObject::decode(&b).unwrap(), m);
    assert_hardened(&b, MappingObject::decode);
    // Equal canonical bytes, equal hash: two indexes share one object.
    assert_eq!(
        MappingObject::new(b"canonical-mapping-bytes").unwrap().hash,
        m.hash
    );
    assert_ne!(
        MappingObject::new(b"canonical-mapping-bytez").unwrap().hash,
        m.hash
    );
    // A stored hash that does not match the bytes is refused even with valid checksums.
    let f = forge(&b, |body| body[0] ^= 1);
    assert_eq!(
        MappingObject::decode(&f).unwrap_err(),
        FormatError::Checksum
    );
    assert!(
        MappingObject {
            hash: [0; 32],
            canonical: b"x".to_vec()
        }
        .encode(&mut Vec::new())
        .is_err()
    );
    assert!(MappingObject::new(&[]).is_err());
    assert!(MappingObject::new(&vec![0; MAX_MAPPING_BYTES]).is_ok());
    assert!(MappingObject::new(&vec![0; MAX_MAPPING_BYTES + 1]).is_err());
    let empty = forge(&b, |body| body.truncate(32));
    assert!(MappingObject::decode(&empty).is_err());
}

#[test]
fn pin_and_cursor() {
    let p = PinRecord {
        pin: id(1),
        index: id(2),
        manifest: id(3),
        manifest_generation: 5,
        expires_unix_ms: 1_700_000_000_000,
        token: tok(1),
    };
    let b = enc(|o| p.encode(o));
    assert_eq!(PinRecord::decode(&b).unwrap(), p);
    assert_hardened(&b, PinRecord::decode);

    let v = Cursor {
        index: id(1),
        routing_generation: 4,
        body: CursorBody::Vector((0..4).map(|i| (id(i), u64::from(i) * 10)).collect()),
    };
    let vb = enc(|o| v.encode(o));
    assert_eq!(Cursor::decode(&vb).unwrap(), v);
    assert_hardened(&vb, Cursor::decode);
    let d = Cursor {
        index: id(1),
        routing_generation: 4,
        body: CursorBody::Pin(id(9)),
    };
    let db = enc(|o| d.encode(o));
    assert_eq!(Cursor::decode(&db).unwrap(), d);
    assert_hardened(&db, Cursor::decode);

    let max = Cursor {
        body: CursorBody::Vector(vec![(id(1), 1); MAX_CURSOR_ENTRIES]),
        ..v.clone()
    };
    let mb = enc(|o| max.encode(o));
    assert!(
        mb.len() * 4 / 3 + 4 < 1200,
        "a maximal cursor stays a header-sized token: {} bytes",
        mb.len()
    );
    let over = Cursor {
        body: CursorBody::Vector(vec![(id(1), 1); MAX_CURSOR_ENTRIES + 1]),
        ..v
    };
    assert_eq!(
        over.encode(&mut Vec::new()).unwrap_err(),
        FormatError::TooLarge("cursor entries")
    );
    let at = 16 + 8 + 1;
    for forged in [u32::MAX, MAX_CURSOR_ENTRIES as u32 + 1, 5] {
        let f = forge(&vb, |body| {
            body[at..at + 4].copy_from_slice(&forged.to_le_bytes())
        });
        assert!(Cursor::decode(&f).is_err(), "count {forged}");
    }
}

// ---- layout ---------------------------------------------------------------------------------

#[test]
fn layout_keys_are_derived_bounded_and_ordered() {
    let l = Layout::new(&"a".repeat(MAX_DEPLOYMENT_BYTES)).unwrap();
    let keys = [
        l.fleet(),
        l.lane_head(&id(1), u64::MAX),
        l.lane_slot(&id(1), u64::MAX, u64::MAX, u32::MAX, u64::MAX),
        l.payload(&id(1), &id(2)),
        l.index_descriptor(&id(1), u64::MAX),
        l.manifest_root(&id(1), &id(2), u64::MAX, &id(3)),
        l.manifest_page(&id(1), &id(2), u64::MAX, &id(3)),
        l.mapping(&[0xEE; 32]),
        l.pin(&id(1), &id(2)),
    ];
    for k in &keys {
        assert!(k.len() <= MAX_KEY_BYTES, "{k}");
        assert!(k.starts_with("v1/"));
    }
    let l = Layout::new("prod-1").unwrap();
    assert!(
        l.lane_slot(&id(1), 1, 1, 0, 9) < l.lane_slot(&id(1), 1, 1, 0, 10),
        "ordinals sort numerically"
    );
    assert!(l.lane_slot(&id(1), 1, 2, 0, 0) > l.lane_slot(&id(1), 1, 1, 9, 99));
    assert_eq!(l.fleet(), "v1/prod-1/fleet/FORMAT");
    assert_eq!(
        l.mapping(&[0xAB; 32]),
        format!("v1/prod-1/mappings/{}", "ab".repeat(32))
    );
    // Per-index objects sit under that index's prefix only.
    assert!(
        l.pin(&id(1), &id(2))
            .starts_with(&format!("v1/prod-1/indexes/{}/", "01".repeat(16)))
    );
    for bad in [
        "",
        "-a",
        "a-",
        "A",
        "a/b",
        "a_b",
        &"a".repeat(MAX_DEPLOYMENT_BYTES + 1),
    ] {
        assert!(Layout::new(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn every_error_has_a_message_without_payload() {
    for e in [
        FormatError::Truncated,
        FormatError::Trailing,
        FormatError::BadMagic,
        FormatError::Checksum,
        FormatError::UnsupportedEnvelope(2),
        FormatError::UnknownKind(9),
        FormatError::UnknownVersion {
            kind: 1,
            version: 2,
        },
        FormatError::UnknownMandatoryCapability(4),
        FormatError::TooLarge("body"),
        FormatError::Malformed("x"),
        FormatError::Mismatch("y"),
    ] {
        let s = e.to_string();
        assert!(!s.is_empty());
        let _: &dyn std::error::Error = &e;
    }
}

#[test]
fn objects_are_bound_to_the_identity_their_key_named() {
    let d = lane(vec![id(1)]);
    let b = enc(|o| d.encode(o));
    assert_eq!(LaneDescriptor::decode_at(&b, &id(3)).unwrap(), d);
    assert_eq!(
        LaneDescriptor::decode_at(&b, &id(4)).unwrap_err(),
        FormatError::Mismatch("lane domain")
    );
    let at = slot_at(9);
    let sb = enc(|o| encode_slot(o, &at, tok(1), &SlotBody::Fence { successor_epoch: 1 }));
    SlotView::parse_at(&sb, &at).unwrap();
    for other in [
        SlotPosition { ordinal: 8, ..at },
        SlotPosition { lane: 9, ..at },
        SlotPosition {
            activation: 9,
            ..at
        },
        SlotPosition {
            domain: id(9),
            ..at
        },
    ] {
        assert!(SlotView::parse_at(&sb, &other).is_err());
    }
    let pin = PinRecord {
        pin: id(1),
        index: id(2),
        manifest: id(3),
        manifest_generation: 1,
        expires_unix_ms: 1,
        token: tok(1),
    };
    let pb = enc(|o| pin.encode(o));
    PinRecord::decode_for(&pb, &id(2), &id(1)).unwrap();
    assert!(PinRecord::decode_for(&pb, &id(9), &id(1)).is_err());
    assert!(PinRecord::decode_for(&pb, &id(2), &id(9)).is_err());
    let root = ManifestRoot {
        generation: 1,
        index: id(1),
        engine_format: "e".into(),
        mapping: [0; 32],
        covered: 0,
        crypto_envelope: id(1),
        pages: vec![],
        token: tok(1),
    };
    let rb = enc(|o| root.encode(o));
    ManifestRoot::decode_for(&rb, &id(1)).unwrap();
    assert!(ManifestRoot::decode_for(&rb, &id(2)).is_err());
}

#[test]
fn optional_bits_are_ignored_and_long_byok_ids_fit() {
    // A newer writer may set optional capabilities and flags; this reader still decodes.
    let b = enc(|o| fleet(1, 1, 1).encode(o));
    let flagged = patch_header(&patch_header(&b, 20, &[0xFF]), 10, &[1]);
    assert_eq!(
        FleetRecord::decode(&flagged).unwrap(),
        FleetRecord::decode(&b).unwrap()
    );
    // A KMS resource name of the maximum length round-trips; one byte more is refused.
    let byok = |n: usize| LaneDescriptor {
        policy: PolicyTuple {
            key: KeyIdentity::Byok("k".repeat(n)),
            ..policy()
        },
        ..lane(vec![id(1)])
    };
    let lb = enc(|o| byok(MAX_KEY_ID_BYTES).encode(o));
    assert_eq!(LaneDescriptor::decode(&lb).unwrap(), byok(MAX_KEY_ID_BYTES));
    assert!(byok(MAX_KEY_ID_BYTES + 1).encode(&mut Vec::new()).is_err());
    // A BYOK lane with its one member has no room; an empty one does.
    let own = byok(8);
    assert!(!own.can_host(&own.policy));
    assert!(
        LaneDescriptor {
            members: vec![],
            ..own.clone()
        }
        .can_host(&own.policy)
    );
    // Residency is a restricted charset and the format version is non-zero.
    for bad in ["", "eu west", "eu/west", "r\u{e9}gion"] {
        let d = LaneDescriptor {
            policy: PolicyTuple {
                residency: bad.into(),
                ..policy()
            },
            ..lane(vec![])
        };
        assert!(d.encode(&mut Vec::new()).is_err(), "{bad:?}");
    }
    let d = LaneDescriptor {
        policy: PolicyTuple {
            format_version: 0,
            ..policy()
        },
        ..lane(vec![])
    };
    assert!(d.encode(&mut Vec::new()).is_err());
}

#[test]
fn a_root_never_encodes_what_its_decoder_refuses() {
    for bad in [0u32, MAX_PAGE_ENTRIES as u32 + 1, u32::MAX] {
        let root = ManifestRoot {
            generation: 1,
            index: id(1),
            engine_format: "e".into(),
            mapping: [0; 32],
            covered: 0,
            crypto_envelope: id(1),
            pages: vec![PageRef {
                page: id(1),
                entry_count: bad,
                digest: [0; 32],
            }],
            token: tok(1),
        };
        let mut out = vec![1u8];
        assert!(root.encode(&mut out).is_err(), "{bad}");
        assert_eq!(out, vec![1u8]);
    }
}
