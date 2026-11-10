//! Synthetic markers below are equality fixtures, not hashes of provider data.
//! Source intent only: compiler and test execution have not been authorized.
use hsearch_object_probe::{
    checker::{Inconclusive, Limits, Verdict, check},
    codec,
    model::*,
};
use std::time::Duration;

fn stamp(order: u32) -> Stamp {
    Stamp {
        order,
        micros: u64::from(order),
    }
}
fn span(start: u32, end: u32) -> Span {
    Span {
        start: stamp(start),
        end: Some(stamp(end)),
    }
}
fn binding() -> Binding {
    Binding {
        digests: [[0; 32]; 7],
        run: [0; 16],
        platform: 1,
    }
}
fn object() -> ObjectId {
    ObjectId { bucket: 0, key: 1 }
}
fn payload(marker: u8) -> Payload {
    Payload {
        length: 8,
        digest: [marker; 32],
    }
}
fn mutation(request: AtomId, marker: u8, precondition: Precondition) -> Mutation {
    Mutation {
        request,
        object: object(),
        payload: payload(marker),
        metadata: 2,
        precondition,
    }
}
fn version(token: AtomId, marker: u8) -> Version {
    Version {
        token,
        payload: payload(marker),
        metadata: 2,
    }
}
fn receipt(m: Mutation, token: AtomId) -> Outcome {
    Outcome::Receipt(Receipt {
        request: m.request,
        version: Version {
            token,
            payload: m.payload,
            metadata: m.metadata,
        },
    })
}
fn fixture() -> History {
    let atoms = [
        (AtomKind::Bucket, "bucket"),
        (AtomKind::Key, "key"),
        (AtomKind::Metadata, ""),
        (AtomKind::Request, "request-a"),
        (AtomKind::Request, "request-b"),
        (AtomKind::Request, "request-c"),
        (AtomKind::Token, "opaque-z"),
        (AtomKind::Token, "opaque-a"),
        (AtomKind::Token, "opaque-middle"),
        (AtomKind::Key, "other-key"),
    ]
    .into_iter()
    .map(|(kind, s)| Atom {
        kind,
        bytes: s.as_bytes().to_vec(),
    })
    .collect();
    History {
        binding: binding(),
        scenario: Scenario::Update,
        complete: true,
        declared_operations: 0,
        declared_attempts: 0,
        physical_attempts: 0,
        atoms,
        operations: vec![],
        attempts: vec![],
        races: vec![],
        errors: vec![],
        scenario_receipt: vec![],
    }
}
fn add(h: &mut History, action: Action, outcome: Outcome) {
    let id = h.operations.len() as u16 + 1;
    let start = u32::from(id) * 10;
    h.operations.push(Operation {
        id,
        client: 0,
        generation: 99 - u32::from(id),
        race: None,
        continuation_of: None,
        span: span(start, start + 3),
        action,
        outcome: Some(outcome),
    });
    let (boundary, status) = match outcome {
        Outcome::UnknownOutcome => (Boundary::AfterTransmitBeforeReply, None),
        Outcome::BeforeSendRefusal => (Boundary::NotTransmitted, None),
        Outcome::Conflict => (Boundary::ReplyReceived, Some(412)),
        Outcome::NotFound => (Boundary::ReplyReceived, Some(404)),
        Outcome::InvalidRange => (Boundary::ReplyReceived, Some(416)),
        Outcome::IdentityConflict => (Boundary::ReplyReceived, Some(409)),
        Outcome::ResolutionUnavailable | Outcome::Unsupported => {
            (Boundary::ReplyReceived, Some(503))
        }
        _ => (Boundary::ReplyReceived, Some(200)),
    };
    let transmitted = boundary != Boundary::NotTransmitted;
    h.attempts.push(Attempt {
        id,
        operation: id,
        span: span(start + 1, start + 2),
        boundary,
        status,
        request_bytes: if transmitted { 10 } else { 0 },
        response_bytes: 0,
        error: None,
    });
    h.declared_operations = id;
    h.declared_attempts = id;
    h.physical_attempts += u16::from(transmitted);
}
// Synthetic fixture planning only. Real capture plans must be independently
// admitted before execution, with the exact dictionary and intended calls.
fn plan(h: &History) -> Plan {
    Plan {
        binding: binding(),
        operations: h.declared_operations,
        scenario: h.scenario,
        atoms: h.atoms.clone(),
        invocations: h
            .operations
            .iter()
            .map(|o| Invocation {
                id: o.id,
                client: o.client,
                generation: o.generation,
                race: o.race,
                continuation_of: o.continuation_of,
                action: o.action,
            })
            .collect(),
        races: vec![],
        ranges: vec![],
    }
}
fn verdict(h: &History) -> Verdict {
    check(h, &plan(h), Limits::default()).unwrap()
}
fn consistent(h: &History) {
    assert!(matches!(verdict(h), Verdict::Consistent(_)));
}
fn rejected(h: &History) {
    assert!(matches!(verdict(h), Verdict::Rejected { .. }));
}
fn reason(h: &History, expected: Inconclusive) {
    assert!(matches!(verdict(h), Verdict::Inconclusive { reason, .. } if reason == expected));
}
fn created() -> History {
    let mut h = fixture();
    let m = mutation(3, 1, Precondition::Absent);
    add(&mut h, Action::Write(m), receipt(m, 6));
    h
}

#[test]
fn legal_chain_keeps_original_receipt_and_immutable_old_version() {
    let mut h = created();
    let update = mutation(4, 2, Precondition::Matches(6));
    add(&mut h, Action::Write(update), receipt(update, 7));
    add(&mut h, Action::Head(object()), Outcome::Head(version(7, 2)));
    add(
        &mut h,
        Action::Write(mutation(5, 3, Precondition::Matches(6))),
        Outcome::Conflict,
    );
    add(
        &mut h,
        Action::Resolve(mutation(3, 1, Precondition::Absent)),
        receipt(mutation(3, 1, Precondition::Absent), 6),
    );
    add(
        &mut h,
        Action::Write(mutation(3, 1, Precondition::Absent)),
        receipt(mutation(3, 1, Precondition::Absent), 6),
    );
    add(
        &mut h,
        Action::ReadRange {
            object: object(),
            token: 6,
            offset: 2,
            length: 3,
        },
        Outcome::Range {
            token: 6,
            length: 3,
            digest: [42; 32],
        },
    );
    add(&mut h, Action::Head(object()), Outcome::Head(version(7, 2)));
    let mut p = plan(&h);
    p.ranges.push(RangeOracle {
        payload: payload(1),
        offset: 2,
        length: 3,
        digest: [42; 32],
    });
    assert!(matches!(
        check(&h, &p, Limits::default()).unwrap(),
        Verdict::Consistent(_)
    ));
    assert_eq!(
        codec::decode(&codec::encode(&h).unwrap(), &binding()).unwrap(),
        h
    );
}

#[test]
fn intermediate_bad_read_is_rejected_even_with_correct_final_head() {
    let mut h = created();
    add(&mut h, Action::Head(object()), Outcome::Head(version(7, 2)));
    add(&mut h, Action::Head(object()), Outcome::Head(version(6, 1)));
    rejected(&h);
}

#[test]
fn stale_cas_and_aba_token_reuse_cannot_succeed() {
    let mut h = created();
    let m = mutation(4, 2, Precondition::Matches(6));
    add(&mut h, Action::Write(m), receipt(m, 7));
    let stale = mutation(5, 3, Precondition::Matches(6));
    add(&mut h, Action::Write(stale), receipt(stale, 8));
    rejected(&h);
    h.operations.pop();
    h.attempts.pop();
    h.declared_operations -= 1;
    h.declared_attempts -= 1;
    h.physical_attempts -= 1;
    let aba = mutation(5, 1, Precondition::Matches(7));
    add(&mut h, Action::Write(aba), receipt(aba, 6));
    rejected(&h);
}

#[test]
fn changed_identity_envelope_requires_identity_conflict() {
    let mut h = created();
    let changed = mutation(3, 2, Precondition::Matches(6));
    add(&mut h, Action::Write(changed), Outcome::IdentityConflict);
    consistent(&h);
    h.operations[1].outcome = Some(receipt(changed, 7));
    h.attempts[1].status = Some(200);
    rejected(&h);
}

#[test]
fn replay_must_return_original_receipt_without_restoring_old_head() {
    let mut h = created();
    let m = mutation(4, 2, Precondition::Matches(6));
    add(&mut h, Action::Write(m), receipt(m, 7));
    let old = mutation(3, 1, Precondition::Absent);
    add(&mut h, Action::Resolve(old), receipt(old, 6));
    consistent(&h);
    h.operations[2].outcome = Some(receipt(old, 7));
    rejected(&h);
    h.operations[2].outcome = Some(receipt(old, 6));
    add(&mut h, Action::Head(object()), Outcome::Head(version(6, 1)));
    rejected(&h);
}

#[test]
fn lost_response_resolves_original_commit_after_later_update() {
    let mut h = fixture();
    let m = mutation(3, 1, Precondition::Absent);
    add(&mut h, Action::Write(m), Outcome::UnknownOutcome);
    let later = mutation(4, 2, Precondition::Matches(6));
    add(&mut h, Action::Write(later), receipt(later, 7));
    add(&mut h, Action::Resolve(m), receipt(m, 6));
    h.operations[2].continuation_of = Some(1);
    add(&mut h, Action::Head(object()), Outcome::Head(version(7, 2)));
    consistent(&h);
    h.operations[2].action = Action::Write(m);
    assert_eq!(
        check(&h, &plan(&h), Limits::default()),
        Err(Invalid::Continuation)
    );
    h.operations[2].action = Action::Resolve(mutation(3, 3, Precondition::Absent));
    assert_eq!(h.validate(), Err(Invalid::Continuation));
}

#[test]
fn unknown_and_resolution_unavailability_never_mean_rollback() {
    let mut h = fixture();
    let m = mutation(3, 1, Precondition::Absent);
    add(&mut h, Action::Write(m), Outcome::UnknownOutcome);
    reason(&h, Inconclusive::UnknownOutcome);
    add(&mut h, Action::Resolve(m), Outcome::ResolutionUnavailable);
    reason(&h, Inconclusive::UnknownOutcome);
    add(&mut h, Action::Head(object()), Outcome::NotFound);
    reason(&h, Inconclusive::UnknownOutcome);
    let mut read = fixture();
    add(&mut read, Action::Head(object()), Outcome::UnknownOutcome);
    reason(&read, Inconclusive::CapabilityUnavailable);
}

#[test]
fn wrong_object_version_length_or_digest_cannot_pass_range_check() {
    let mut h = created();
    add(
        &mut h,
        Action::ReadRange {
            object: object(),
            token: 6,
            offset: 1,
            length: 2,
        },
        Outcome::Range {
            token: 6,
            length: 2,
            digest: [42; 32],
        },
    );
    reason(&h, Inconclusive::MissingRangeOracle);
    let mut p = plan(&h);
    p.ranges.push(RangeOracle {
        payload: payload(1),
        offset: 1,
        length: 2,
        digest: [41; 32],
    });
    assert!(matches!(
        check(&h, &p, Limits::default()).unwrap(),
        Verdict::Rejected { .. }
    ));
    p.ranges[0].digest = [42; 32];
    for action in [
        Action::ReadRange {
            object: ObjectId { bucket: 0, key: 9 },
            token: 6,
            offset: 1,
            length: 2,
        },
        Action::ReadRange {
            object: object(),
            token: 7,
            offset: 1,
            length: 2,
        },
    ] {
        h.operations[1].action = action;
        p.invocations[1].action = action;
        assert!(matches!(
            check(&h, &p, Limits::default()).unwrap(),
            Verdict::Rejected { .. }
        ));
    }
    h.operations[1].action = Action::ReadRange {
        object: object(),
        token: 6,
        offset: 1,
        length: 2,
    };
    p.invocations[1].action = h.operations[1].action;
    h.operations[1].outcome = Some(Outcome::Range {
        token: 6,
        length: 1,
        digest: [42; 32],
    });
    rejected(&h);
    p.ranges[0].length = u64::MAX;
    assert_eq!(check(&h, &p, Limits::default()), Err(Invalid::Bound));
    p.ranges[0].length = 2;
    p.ranges.push(p.ranges[0]);
    assert_eq!(check(&h, &p, Limits::default()), Err(Invalid::Shape));
}

fn race(two_successes: bool, overlapping: bool) -> (History, Plan) {
    let mut h = created();
    let a = mutation(4, 2, Precondition::Matches(6));
    let b = mutation(5, 3, Precondition::Matches(6));
    add(&mut h, Action::Write(a), receipt(a, 7));
    add(
        &mut h,
        Action::Write(b),
        if two_successes {
            receipt(b, 8)
        } else {
            Outcome::Conflict
        },
    );
    h.operations[1].client = 1;
    h.operations[2].client = 2;
    h.operations[1].race = Some(1);
    h.operations[2].race = Some(1);
    h.operations[1].span = span(20, 27);
    h.operations[2].span = span(21, 28);
    h.attempts[1].span = span(23, 25);
    h.attempts[2].span = if overlapping {
        span(24, 26)
    } else {
        span(26, 27)
    };
    // A completion and an attempt end are distinct capture events.
    if !overlapping {
        h.operations[1].span.end = Some(stamp(29));
        h.operations[2].span.end = Some(stamp(30));
    }
    h.races.push(Race {
        id: 1,
        released: stamp(22),
    });
    let mut p = plan(&h);
    p.races.push(RacePlan {
        id: 1,
        participants: (1 << 1) | (1 << 2),
    });
    (h, p)
}
#[test]
fn actual_overlap_and_all_observations_determine_race_verdict() {
    let (h, p) = race(false, true);
    assert!(matches!(
        check(&h, &p, Limits::default()).unwrap(),
        Verdict::Consistent(_)
    ));
    let (h, p) = race(true, true);
    assert!(matches!(
        check(&h, &p, Limits::default()).unwrap(),
        Verdict::Rejected { .. }
    ));
    let (h, p) = race(false, false);
    assert!(matches!(
        check(&h, &p, Limits::default()).unwrap(),
        Verdict::Inconclusive {
            reason: Inconclusive::AttemptOrOverlap,
            ..
        }
    ));
}

#[test]
fn missing_capture_mixed_binding_and_hidden_retries_are_not_passes() {
    let mut h = created();
    h.complete = false;
    reason(&h, Inconclusive::Incomplete);
    h.complete = true;
    let p = plan(&h);
    h.binding.run[0] = 1;
    assert_eq!(check(&h, &p, Limits::default()), Err(Invalid::Binding));
    h.binding = binding();
    h.operations[0].span.end = None;
    h.operations[0].outcome = None;
    reason(&h, Inconclusive::Incomplete);
    let mut h = created();
    h.attempts.clear();
    h.declared_attempts = 0;
    h.physical_attempts = 0;
    reason(&h, Inconclusive::Incomplete);
    let mut h = created();
    let mut p = plan(&h);
    p.operations = 2;
    let mut second = p.invocations[0];
    second.id = 2;
    p.invocations.push(second);
    h.declared_operations = 2;
    assert!(matches!(
        check(&h, &p, Limits::default()).unwrap(),
        Verdict::Inconclusive {
            reason: Inconclusive::Incomplete,
            ..
        }
    ));
    let mut h = created();
    h.operations[0].span.end = Some(stamp(18));
    let mut extra = h.attempts[0];
    extra.id = 2;
    extra.span = span(15, 16);
    h.attempts.push(extra);
    h.declared_attempts = 2;
    h.physical_attempts = 2;
    reason(&h, Inconclusive::AttemptOrOverlap);
    h.attempts[1].boundary = Boundary::NotTransmitted;
    h.attempts[1].status = None;
    h.attempts[1].request_bytes = 0;
    h.physical_attempts = 1;
    reason(&h, Inconclusive::AttemptOrOverlap);
}

#[test]
fn exhaustion_and_unmodeled_scenarios_are_explicitly_inconclusive() {
    let h = created();
    for limits in [
        Limits::bounded(0, Duration::from_secs(10), 64 * 1024 * 1024).unwrap(),
        Limits::bounded(100_000, Duration::ZERO, 64 * 1024 * 1024).unwrap(),
        Limits::bounded(100_000, Duration::from_secs(10), 0).unwrap(),
    ] {
        assert!(matches!(
            check(&h, &plan(&h), limits).unwrap(),
            Verdict::Inconclusive { .. }
        ));
    }
    assert!(Limits::bounded(100_001, Duration::ZERO, 0).is_err());
    for scenario in [Scenario::Restart, Scenario::Multipart] {
        let mut h = h.clone();
        h.scenario = scenario;
        reason(&h, Inconclusive::UnmodeledEvidence);
    }
    let mut h = h.clone();
    h.scenario_receipt.push(1);
    reason(&h, Inconclusive::UnmodeledEvidence);
}

#[test]
fn codec_refuses_all_truncations_trailing_and_mandatory_header_corruption() {
    let h = created();
    let raw = codec::encode(&h).unwrap();
    for end in 0..raw.len() {
        assert!(
            codec::decode(&raw[..end], &binding()).is_err(),
            "prefix {end}"
        );
    }
    let mut bad = raw.clone();
    bad.push(0);
    assert_eq!(codec::decode(&bad, &binding()), Err(codec::Error::Trailing));
    for index in [0, 8, 251, 252] {
        let mut bad = raw.clone();
        bad[index] = 255;
        assert!(codec::decode(&bad, &binding()).is_err());
    }
    let mut wrong = binding();
    wrong.digests[6][31] = 1;
    assert_eq!(codec::decode(&raw, &wrong), Err(codec::Error::Binding));
    // Header offsets are independently derived from the v1 wire layout.
    for index in [253, 255, 257, 259, 261, 263, 265, 267] {
        let mut bad = raw.clone();
        bad[index..index + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(codec::decode(&bad, &binding()).is_err());
    }
    let mut bad = raw.clone();
    bad[269..273].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(codec::decode(&bad, &binding()), Err(codec::Error::Bound));
    let mut bad = raw.clone();
    bad[273] = 255;
    assert_eq!(codec::decode(&bad, &binding()), Err(codec::Error::Format));
    let mut bad = raw;
    bad[274..276].copy_from_slice(&u16::MAX.to_le_bytes());
    assert_eq!(codec::decode(&bad, &binding()), Err(codec::Error::Bound));
}

#[test]
fn independent_golden_empty_capture_is_not_a_semantic_pass() {
    // Hand-built framing, never produced by encode. Binding fields are zero.
    let mut raw = b"HPROBE01\x01\x00".to_vec();
    raw.extend_from_slice(&[0; 240]);
    raw.extend_from_slice(&[1, 0, 0]); // platform, Create, incomplete
    raw.extend_from_slice(&[0; 20]); // three counters, five section counts, receipt length
    raw.extend_from_slice(b"HEND0001");
    assert_eq!(raw.len(), 281);
    let h = codec::decode(&raw, &binding()).unwrap();
    assert!(h.atoms.is_empty() && h.operations.is_empty() && !h.complete);
    assert_eq!(codec::encode(&h).unwrap(), raw);
    reason(&h, Inconclusive::Incomplete);
}

#[test]
fn structural_bounds_references_and_secret_omission_are_preserved() {
    let mut h = created();
    h.atoms.push(h.atoms[0].clone());
    assert_eq!(h.validate(), Err(Invalid::Shape));
    let mut h = created();
    h.atoms[6].bytes.resize(257, 1);
    assert!(h.validate().is_err());
    let mut h = created();
    h.operations[0].action = Action::Head(ObjectId { bucket: 6, key: 1 });
    assert_eq!(h.validate(), Err(Invalid::Shape));
    let mut h = created();
    h.errors.push(vec![1; 4097]);
    assert_eq!(h.validate(), Err(Invalid::Bound));
    let mut h = created();
    h.errors.push(b"private error".to_vec());
    h.scenario_receipt = b"private receipt".to_vec();
    let debug = format!("{h:?}");
    assert!(!debug.contains("private"));
    assert!(!format!("{:?}", h.atoms[0]).contains("bucket\""));
}

#[test]
fn independent_plan_refuses_substituted_calls_dictionary_and_scenario() {
    let mut h = created();
    let p = plan(&h);
    h.operations[0].action = Action::Head(object());
    assert_eq!(check(&h, &p, Limits::default()), Err(Invalid::Shape));
    let mut h = created();
    h.atoms[6].bytes = b"substituted-token".to_vec();
    assert_eq!(check(&h, &p, Limits::default()), Err(Invalid::Binding));
    let mut h = created();
    h.scenario = Scenario::Create;
    assert_eq!(check(&h, &p, Limits::default()), Err(Invalid::Binding));
}

#[test]
fn sixteen_client_create_race_requires_measured_common_overlap() {
    let mut h = fixture();
    h.scenario = Scenario::Create;
    for client in 0..16u8 {
        let request = h.atoms.len() as AtomId;
        h.atoms.push(Atom {
            kind: AtomKind::Request,
            bytes: format!("race-{client}").into_bytes(),
        });
        let m = mutation(request, client, Precondition::Absent);
        add(
            &mut h,
            Action::Write(m),
            if client == 0 {
                receipt(m, 6)
            } else {
                Outcome::Conflict
            },
        );
        let i = usize::from(client);
        h.operations[i].client = client;
        h.operations[i].race = Some(7);
        h.operations[i].span = span(u32::from(client) + 1, u32::from(client) + 50);
        h.attempts[i].span = span(u32::from(client) + 18, u32::from(client) + 34);
    }
    h.races.push(Race {
        id: 7,
        released: stamp(17),
    });
    let mut p = plan(&h);
    p.races.push(RacePlan {
        id: 7,
        participants: u16::MAX,
    });
    assert!(matches!(
        check(&h, &p, Limits::default()).unwrap(),
        Verdict::Consistent(_)
    ));
    p.races[0].participants &= !1;
    assert!(matches!(
        check(&h, &p, Limits::default()).unwrap(),
        Verdict::Inconclusive {
            reason: Inconclusive::AttemptOrOverlap,
            ..
        }
    ));
}

#[test]
fn invalid_ranges_and_certified_pre_send_refusal_have_no_effect() {
    let mut h = created();
    for (offset, length) in [(0, 0), (u64::MAX, 2), (7, 2)] {
        add(
            &mut h,
            Action::ReadRange {
                object: object(),
                token: 6,
                offset,
                length,
            },
            Outcome::InvalidRange,
        );
    }
    let m = mutation(4, 2, Precondition::Matches(6));
    add(&mut h, Action::Write(m), Outcome::BeforeSendRefusal);
    add(&mut h, Action::Head(object()), Outcome::Head(version(6, 1)));
    consistent(&h);
    let mut no_send = created();
    no_send.attempts[0].boundary = Boundary::NotTransmitted;
    no_send.attempts[0].status = None;
    no_send.attempts[0].request_bytes = 0;
    no_send.physical_attempts = 0;
    reason(&no_send, Inconclusive::Incomplete);
}

#[test]
fn suppressed_reply_is_unknown_at_the_caller_and_cannot_be_a_receipt() {
    let mut h = created();
    h.operations[0].outcome = Some(Outcome::UnknownOutcome);
    h.attempts[0].boundary = Boundary::ReplySuppressed;
    reason(&h, Inconclusive::UnknownOutcome);
    h.operations[0].outcome = Some(receipt(mutation(3, 1, Precondition::Absent), 6));
    assert_eq!(check(&h, &plan(&h), Limits::default()), Err(Invalid::Shape));
}

#[test]
fn omitted_continuation_label_cannot_authorize_blind_retry() {
    let mut h = fixture();
    let m = mutation(3, 1, Precondition::Absent);
    add(&mut h, Action::Write(m), Outcome::UnknownOutcome);
    add(&mut h, Action::Write(m), receipt(m, 6));
    assert_eq!(h.validate(), Err(Invalid::Continuation));
}
