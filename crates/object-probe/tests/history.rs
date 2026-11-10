//! Synthetic markers below are equality fixtures, not hashes of provider data.
//! Counterfactual controls do not establish real provider qualification.
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

#[path = "codec_golden.rs"]
mod v2_golden;

fn v2_scope(area: u8) -> (CapturePlan, Capture) {
    let mut plan = codec::decode_plan_v2(v2_golden::FULL_PLAN).unwrap();
    let mut capture =
        codec::decode_capture_v2(v2_golden::FULL_CAPTURE, codec::sha256(v2_golden::FULL_PLAN))
            .unwrap();
    plan.specifications.retain(|s| s.scenario == area);
    v2_recount(&plan, &mut capture);
    (plan, capture)
}
fn v2_recount(plan: &CapturePlan, capture: &mut Capture) {
    capture
        .events
        .retain(|e| plan.specifications.iter().any(|s| s.id == e.id));
    capture.attempts = u16::try_from(capture.events.len()).unwrap();
    for (index, event) in capture.events.iter_mut().enumerate() {
        event.attempt = u16::try_from(index + 1).unwrap();
    }
}
fn v2_event(capture: &mut Capture, id: u16) -> &mut CaptureEvent {
    capture.events.iter_mut().find(|e| e.id == id).unwrap()
}
fn v2_id(plan: &CapturePlan, action: &str) -> u16 {
    plan.specifications
        .iter()
        .find(|s| s.action == action)
        .unwrap()
        .id
}
fn v2_status(
    plan: &CapturePlan,
    capture: &Capture,
    area: usize,
) -> hsearch_object_probe::eligibility::Status {
    hsearch_object_probe::eligibility::evaluate(plan, capture, v2_golden::RESTART).scenarios[area]
}

#[test]
fn malformed_version_inventory_is_missing_proof_not_a_negative_commit_receipt() {
    use hsearch_object_probe::eligibility::Status;
    let (plan, capture) = v2_scope(2);
    assert_eq!(v2_status(&plan, &capture, 2), Status::Supported);
    let versions = v2_id(&plan, "versions");
    let mut malformed = vec![
        "not-json".to_string(),
        "[unquoted]".into(),
        "[\"unterminated]".into(),
        "[\"\"]".into(),
        "[\"null\"]".into(),
        "[\"same\",\"same\"]".into(),
        "[\"bad\\q\"]".into(),
        "[\"line\nfeed\"]".into(),
        "[\"one\";\"two\"]".into(),
        format!("[\"{}\"]", "v".repeat(257)),
    ];
    malformed.push(format!(
        "[{}]",
        (0..17)
            .map(|i| format!("\"v-{i}\""))
            .collect::<Vec<_>>()
            .join(",")
    ));
    for raw in malformed {
        let mut changed = capture.clone();
        v2_event(&mut changed, versions).auxiliary = raw;
        assert_eq!(v2_status(&plan, &changed, 2), Status::Inconclusive);
    }
}

#[test]
fn escaped_opaque_versions_can_resolve_without_treating_them_as_hashes() {
    use hsearch_object_probe::eligibility::Status;
    let (plan, mut capture) = v2_scope(2);
    let original = v2_event(&mut capture, v2_id(&plan, "create"))
        .version
        .clone();
    let replacement = "opaque-\"quote\\slash";
    for event in &mut capture.events {
        if event.version == original {
            event.version = replacement.into();
        }
        if event.selected == original {
            event.selected = replacement.into();
        }
        event.auxiliary = event
            .auxiliary
            .replace(&format!("{original:?}"), &format!("{replacement:?}"));
    }
    assert_eq!(v2_status(&plan, &capture, 2), Status::Supported);
}

#[test]
fn resolution_binds_original_envelope_selected_version_length_and_bytes() {
    use hsearch_object_probe::eligibility::Status;
    let (plan, capture) = v2_scope(2);
    let id = v2_id(&plan, "resolve");
    let corruptions: [fn(&mut CaptureEvent); 6] = [
        |e| e.condition = "other-condition".into(),
        |e| e.selected = "foreign-version".into(),
        |e| e.version = "different-receipt".into(),
        |e| e.metadata = "other-envelope".into(),
        |e| e.received += 1,
        |e| e.digest[0] ^= 1,
    ];
    for change in corruptions {
        let mut changed = capture.clone();
        change(v2_event(&mut changed, id));
        assert_eq!(v2_status(&plan, &changed, 2), Status::Failed);
    }
    let mut unbound = plan.clone();
    unbound
        .specifications
        .iter_mut()
        .find(|s| s.id == id)
        .unwrap()
        .parent = 39;
    let mut changed = capture.clone();
    v2_event(&mut changed, id).request = format!("{}-39", "22".repeat(16));
    assert_eq!(v2_status(&unbound, &changed, 2), Status::Failed);
}

#[test]
fn failed_resolve_and_invented_missing_version_head_do_not_prove_noncommit() {
    use hsearch_object_probe::eligibility::Status;
    let (plan, capture) = v2_scope(2);
    let mut unavailable = capture.clone();
    v2_event(&mut unavailable, v2_id(&plan, "resolve")).status = 404;
    assert_eq!(v2_status(&plan, &unavailable, 2), Status::Inconclusive);
    let absent = plan
        .specifications
        .iter()
        .find(|s| s.action == "resolve-head" && s.offset == 15)
        .unwrap()
        .id;
    let mut invented = capture.clone();
    let event = v2_event(&mut invented, absent);
    event.status = 404;
    event.boundary = "reply".into();
    event.sent = 1;
    assert_eq!(v2_status(&plan, &invented, 2), Status::Failed);
    let mut wrong = capture.clone();
    v2_event(&mut wrong, v2_id(&plan, "resolve-head")).selected = "unlisted-version".into();
    assert_eq!(v2_status(&plan, &wrong, 2), Status::Failed);
}

#[test]
fn actual_mutation_conflicts_and_unsupported_responses_have_distinct_meaning() {
    use hsearch_object_probe::eligibility::Status;
    let (plan, capture) = v2_scope(2);
    let create = v2_id(&plan, "create");
    for failure in 0..3 {
        let mut changed = capture.clone();
        let event = v2_event(&mut changed, create);
        match failure {
            0 => event.boundary = "reply".into(),
            1 => {
                event.condition = "unconditional".into();
            }
            _ => {
                event.sent = 0;
            }
        }
        assert_eq!(v2_status(&plan, &changed, 2), Status::Failed);
    }
    let reuse = v2_id(&plan, "reuse-id");
    for (status, expected) in [
        (200, Status::Failed),
        (404, Status::Inconclusive),
        (503, Status::Inconclusive),
    ] {
        let mut changed = capture.clone();
        v2_event(&mut changed, reuse).status = status;
        assert_eq!(v2_status(&plan, &changed, 2), expected);
    }
    let mut changed = capture.clone();
    v2_event(&mut changed, reuse).error = "provider-envelope-missing".into();
    v2_event(&mut changed, v2_id(&plan, "resolve")).status = 503;
    assert_eq!(v2_status(&plan, &changed, 2), Status::Failed);
}

#[test]
fn exact_version_reads_refuse_wrong_metadata_lengths_ranges_and_stale_success() {
    use hsearch_object_probe::eligibility::Status;
    let (plan, capture) = v2_scope(3);
    assert_eq!(v2_status(&plan, &capture, 3), Status::Supported);
    let head = v2_id(&plan, "head");
    let corruptions: [fn(&mut CaptureEvent); 4] = [
        |e| e.head_length = None,
        |e| e.etag = "foreign-tag".into(),
        |e| e.metadata = "foreign-envelope".into(),
        |e| e.status = 404,
    ];
    for change in corruptions {
        let mut changed = capture.clone();
        change(v2_event(&mut changed, head));
        assert_eq!(v2_status(&plan, &changed, 3), Status::Failed);
    }
    let range = v2_id(&plan, "range");
    let mut wrong = capture.clone();
    v2_event(&mut wrong, range).selected = "newer-version".into();
    assert_eq!(v2_status(&plan, &wrong, 3), Status::Failed);
    let invalid = plan
        .specifications
        .iter()
        .find(|s| s.action == "range" && s.range_length == 1)
        .unwrap()
        .id;
    let mut wrong = capture.clone();
    v2_event(&mut wrong, invalid).status = 206;
    assert_eq!(v2_status(&plan, &wrong, 3), Status::Failed);
    let mut wrong = capture.clone();
    v2_event(&mut wrong, v2_id(&plan, "stale-update")).status = 200;
    assert_eq!(v2_status(&plan, &wrong, 3), Status::Failed);
    let mut wrong = capture.clone();
    v2_event(&mut wrong, v2_id(&plan, "replay")).condition = "new-condition".into();
    assert_eq!(v2_status(&plan, &wrong, 3), Status::Failed);
}

#[test]
fn race_readback_cannot_substitute_a_loser_or_claim_two_winners() {
    use hsearch_object_probe::eligibility::Status;
    let (plan, capture) = v2_scope(0);
    assert_eq!(v2_status(&plan, &capture, 0), Status::Supported);
    let head = v2_id(&plan, "race-head");
    let range = v2_id(&plan, "race-range");
    for failure in 0..4 {
        let mut changed = capture.clone();
        match failure {
            0 => v2_event(&mut changed, head).head_length = None,
            1 => v2_event(&mut changed, head).metadata = "loser-envelope".into(),
            2 => v2_event(&mut changed, range).digest[0] ^= 1,
            _ => {
                let losing = plan
                    .specifications
                    .iter()
                    .find(|s| {
                        s.action == "create"
                            && capture
                                .events
                                .iter()
                                .any(|e| e.id == s.id && e.status == 412)
                    })
                    .unwrap()
                    .id;
                let event = v2_event(&mut changed, losing);
                event.status = 200;
                event.etag = "loser-tag".into();
                event.version = "loser-version".into();
            }
        }
        assert_eq!(v2_status(&plan, &changed, 0), Status::Failed);
    }
}

fn multipart_state_fixture() -> (CapturePlan, Capture) {
    let (mut plan, mut capture) = v2_scope(5);
    // These controls isolate upload state. Dedicated range tests verify bytes.
    plan.specifications.retain(|s| s.action != "range");
    v2_recount(&plan, &mut capture);
    (plan, capture)
}

#[test]
fn multipart_identity_part_bounds_and_missing_evidence_cannot_complete() {
    use hsearch_object_probe::multipart;
    let (plan, capture) = multipart_state_fixture();
    assert!(multipart::verify(&plan, &capture));
    let init = v2_id(&plan, "init");
    for value in [String::new(), "x".repeat(257)] {
        let mut changed = capture.clone();
        v2_event(&mut changed, init).auxiliary = value;
        assert!(!multipart::verify(&plan, &changed));
    }
    let inits: Vec<_> = plan
        .specifications
        .iter()
        .filter(|s| s.action == "init")
        .map(|s| s.id)
        .collect();
    let mut duplicate = capture.clone();
    let identity = v2_event(&mut duplicate, inits[0]).auxiliary.clone();
    v2_event(&mut duplicate, inits[1]).auxiliary = identity;
    assert!(!multipart::verify(&plan, &duplicate));
    for number in [0, 17] {
        let mut wrong = plan.clone();
        wrong
            .specifications
            .iter_mut()
            .find(|s| s.action == "part")
            .unwrap()
            .offset = number;
        assert!(!multipart::verify(&wrong, &capture));
    }
    let mut missing = capture.clone();
    missing.events.retain(|e| e.id != init);
    assert!(!multipart::verify(&plan, &missing));
    let mut missing_parts = plan.clone();
    missing_parts
        .specifications
        .retain(|s| !(s.parent == init && matches!(s.action.as_str(), "part" | "parts")));
    assert!(!multipart::verify(&missing_parts, &capture));
}

#[test]
fn multipart_part_inventory_and_completion_use_the_final_replacement() {
    use hsearch_object_probe::multipart;
    let (plan, capture) = multipart_state_fixture();
    let parts = v2_id(&plan, "parts");
    let listing = capture
        .events
        .iter()
        .find(|e| e.id == parts)
        .unwrap()
        .auxiliary
        .clone();
    for raw in [
        "PartNumber:31".to_string(),
        listing.replacen("PartNumber:", "PartNumber", 1),
        listing.replacen("PartNumber:", "Other:", 1),
        listing.replacen("PartNumber:31", "PartNumber:zz", 1),
        listing.replacen("ETag:", "ETag:zz", 1),
        listing.replacen("Size:35323432383830", "Size:31", 1),
    ] {
        let mut changed = capture.clone();
        v2_event(&mut changed, parts).auxiliary = raw;
        assert!(!multipart::verify(&plan, &changed));
    }
    let mut unavailable = capture.clone();
    v2_event(&mut unavailable, parts).status = 404;
    assert!(!multipart::verify(&plan, &unavailable));
    let completion = v2_id(&plan, "complete");
    let mut changed = capture.clone();
    let stale = v2_event(&mut changed, 117).etag.clone();
    let current = v2_event(&mut changed, 118).etag.clone();
    let hex = |raw: &str| raw.bytes().map(|b| format!("{b:02x}")).collect::<String>();
    let original = v2_event(&mut changed, completion).auxiliary.clone();
    v2_event(&mut changed, completion).auxiliary = original.replace(&hex(&current), &hex(&stale));
    assert!(!multipart::verify(&plan, &changed));
    let mut changed = capture.clone();
    v2_event(&mut changed, completion).condition.clear();
    assert!(!multipart::verify(&plan, &changed));
}

#[test]
fn aborted_uploads_and_foreign_orphans_cannot_be_reused_as_task_evidence() {
    use hsearch_object_probe::multipart;
    let (plan, capture) = multipart_state_fixture();
    let uploads = v2_id(&plan, "uploads");
    for raw in [
        "Key:zz;UploadId:00",
        "Wrong:00;UploadId:00",
        "Key:00;Wrong:00",
        "Key:00;UploadId:00",
        "Key:00;UploadId:zz",
        "Key:00",
    ] {
        let mut changed = capture.clone();
        v2_event(&mut changed, uploads).auxiliary = raw.into();
        assert!(!multipart::verify(&plan, &changed));
    }
    let mut retained = capture.clone();
    let before = v2_event(&mut retained, uploads).auxiliary.clone();
    v2_event(&mut retained, 154).auxiliary = before;
    assert!(!multipart::verify(&plan, &retained));
    let mut wrong = capture.clone();
    v2_event(&mut wrong, 128).status = 200;
    assert!(!multipart::verify(&plan, &wrong));
    let mut reused = plan.clone();
    let mut part = reused
        .specifications
        .iter()
        .find(|s| s.id == 126)
        .unwrap()
        .clone();
    part.id = 155;
    part.offset = 2;
    part.length = 4096;
    reused.specifications.push(part);
    let mut changed = capture.clone();
    let mut event = v2_event(&mut changed, 126).clone();
    event.id = 155;
    event.attempt = 39;
    event.start = 2_000_000;
    event.end = 2_000_100;
    event.sent = 4160;
    event.request = format!("{}-155", "22".repeat(16));
    changed.events.push(event);
    changed.attempts = 39;
    assert!(!multipart::verify(&reused, &changed));
}

#[test]
fn multipart_range_oracle_refuses_empty_excess_and_overflowed_extents() {
    use hsearch_object_probe::multipart;
    let part = Specification {
        length: 1,
        ..Specification::default()
    };
    assert!(multipart::range_digest(&[], 0, 1).is_none());
    assert!(multipart::range_digest(&[&part], 0, 0).is_none());
    assert!(multipart::range_digest(&[&part; 17], 0, 1).is_none());
    let huge = Specification {
        length: u64::MAX,
        ..Specification::default()
    };
    assert!(multipart::range_digest(&[&huge, &part], 0, 1).is_none());
}

#[test]
fn same_volume_restart_requires_canonical_host_receipt_and_all_three_records() {
    use hsearch_object_probe::restart;
    let (plan, capture) = v2_scope(4);
    assert!(restart::verify(&plan, &capture, v2_golden::RESTART));
    for raw in ["", "[]", "{}", "{unquoted}", "{\"key\":64}"] {
        assert!(!restart::verify(&plan, &capture, raw));
    }
    let duplicate = v2_golden::RESTART.replacen("after_boot", "before_boot", 1);
    let nonhex = v2_golden::RESTART.replacen(&"2".repeat(64), &"g".repeat(64), 1);
    for raw in [duplicate, nonhex, "x".repeat(1025)] {
        assert!(!restart::verify(&plan, &capture, &raw));
    }
    for id in [111, 112, 113] {
        let mut missing = capture.clone();
        missing.events.retain(|e| e.id != id);
        assert!(!restart::verify(&plan, &missing, v2_golden::RESTART));
    }
    let mut duplicate = plan.clone();
    duplicate.specifications.push(
        plan.specifications
            .iter()
            .find(|s| s.action == "restart")
            .unwrap()
            .clone(),
    );
    assert!(!restart::verify(&duplicate, &capture, v2_golden::RESTART));
    let mut missing_head = plan.clone();
    missing_head.specifications.retain(|s| s.action != "head");
    assert!(!restart::verify(
        &missing_head,
        &capture,
        v2_golden::RESTART
    ));
}

#[test]
fn restart_cannot_accept_guest_io_or_an_unversioned_original() {
    use hsearch_object_probe::restart;
    let (plan, capture) = v2_scope(4);
    let corruptions: [fn(&mut CaptureEvent); 4] = [
        |e| e.sent = 1,
        |e| e.received = 1,
        |e| e.status = 200,
        |e| e.boundary = "reply".into(),
    ];
    for change in corruptions {
        let mut changed = capture.clone();
        change(v2_event(&mut changed, 112));
        assert!(!restart::verify(&plan, &changed, v2_golden::RESTART));
    }
    for version in ["", "null"] {
        let mut changed = capture.clone();
        v2_event(&mut changed, 111).version = version.into();
        assert!(!restart::verify(&plan, &changed, v2_golden::RESTART));
    }
}

#[test]
fn typed_capture_error_budgets_clock_bounds_and_continuation_origin_are_checked() {
    let mut h = created();
    h.errors = vec![vec![b'x'; 4096]; 33];
    assert_eq!(h.validate(), Err(Invalid::Bound));
    for bad_stamp in [
        Stamp {
            order: 4097,
            micros: 1,
        },
        Stamp {
            order: 1,
            micros: HISTORY_MICROS + 1,
        },
    ] {
        let mut h = created();
        h.operations[0].span.start = bad_stamp;
        assert_eq!(h.validate(), Err(Invalid::Bound));
    }
    let mut h = created();
    h.operations[0].span.end.as_mut().unwrap().micros = HISTORY_MICROS + 1;
    assert_eq!(h.validate(), Err(Invalid::Bound));
    let mut h = created();
    h.attempts[0].operation = 255;
    assert_eq!(h.validate(), Err(Invalid::Shape));
    let mut h = created();
    let m = mutation(4, 2, Precondition::Matches(6));
    add(&mut h, Action::Resolve(m), receipt(m, 7));
    h.operations[1].continuation_of = Some(255);
    assert_eq!(h.validate(), Err(Invalid::Continuation));
    assert_eq!(Invalid::Continuation.to_string(), "Continuation");
}

// Keep the successful call in the initial cohort so every rejection can be
// linearized after its write, even when later clients miss the first reply.
fn v3_race_cohort(area: u8, count: usize) -> (CapturePlan, Capture) {
    let (plan, mut capture) = v2_scope(area);
    let mut ids: Vec<_> = plan
        .specifications
        .iter()
        .filter(|s| s.race != 0)
        .map(|s| s.id)
        .collect();
    let winner = capture
        .events
        .iter()
        .find(|e| ids.contains(&e.id) && e.status == 200)
        .unwrap()
        .id;
    ids.retain(|id| *id != winner);
    ids.insert(0, winner);
    let base = capture
        .events
        .iter()
        .filter(|e| ids.contains(&e.id))
        .map(|e| e.start)
        .min()
        .unwrap();
    for (index, id) in ids.iter().enumerate() {
        let event = v2_event(&mut capture, *id);
        event.start = base
            + if index < count {
                index as u64
            } else {
                100 + index as u64
            };
        event.end = base + if index == 0 { 100 } else { 200 + index as u64 };
    }
    (plan, capture)
}

#[test]
fn prospective_create_and_update_require_eight_starts_before_first_completion() {
    use hsearch_object_probe::eligibility::Status;
    for area in [0, 1] {
        for (count, expected) in [
            (1, Status::Inconclusive),
            (7, Status::Inconclusive),
            (8, Status::Supported),
            (16, Status::Supported),
        ] {
            let (plan, capture) = v3_race_cohort(area, count);
            assert_eq!(v2_status(&plan, &capture, usize::from(area)), expected);
        }
        let (plan, mut capture) = v3_race_cohort(area, 8);
        let first_end = plan
            .specifications
            .iter()
            .filter(|s| s.race != 0)
            .map(|s| capture.events.iter().find(|e| e.id == s.id).unwrap().end)
            .min()
            .unwrap();
        let eighth = plan
            .specifications
            .iter()
            .filter(|s| s.race != 0)
            .find(|s| {
                capture
                    .events
                    .iter()
                    .any(|e| e.id == s.id && e.start + 93 == first_end)
            })
            .unwrap()
            .id;
        v2_event(&mut capture, eighth).start = first_end;
        assert_eq!(
            v2_status(&plan, &capture, usize::from(area)),
            Status::Inconclusive
        );
    }
}

#[test]
fn partial_race_cohort_retains_all_clients_winner_and_readback_obligations() {
    use hsearch_object_probe::eligibility::Status;
    for area in [0, 1] {
        let (plan, capture) = v3_race_cohort(area, 8);
        let ids: Vec<_> = plan
            .specifications
            .iter()
            .filter(|s| s.race != 0)
            .map(|s| s.id)
            .collect();
        let loser = capture
            .events
            .iter()
            .find(|e| ids.contains(&e.id) && e.status == 412)
            .unwrap()
            .id;
        let winner = capture
            .events
            .iter()
            .find(|e| ids.contains(&e.id) && e.status == 200)
            .unwrap()
            .id;
        let status = |p: &CapturePlan, c: &Capture| v2_status(p, c, usize::from(area));
        let mut duplicate = plan.clone();
        let client = duplicate
            .specifications
            .iter()
            .find(|s| s.id == winner)
            .unwrap()
            .client;
        duplicate
            .specifications
            .iter_mut()
            .find(|s| s.id == loser)
            .unwrap()
            .client = client;
        assert_eq!(status(&duplicate, &capture), Status::Failed);
        let mut missing = capture.clone();
        missing.events.retain(|e| e.id != loser);
        assert_eq!(status(&plan, &missing), Status::Inconclusive);
        let mut missing = plan.clone();
        missing.specifications.retain(|s| s.id != loser);
        let mut reduced = capture.clone();
        v2_recount(&missing, &mut reduced);
        assert_eq!(status(&missing, &reduced), Status::Inconclusive);
        let mut twice = capture.clone();
        let event = v2_event(&mut twice, loser);
        event.status = 200;
        event.etag = "second-tag".into();
        event.version = "second-version".into();
        assert_eq!(status(&plan, &twice), Status::Failed);
        let mut absent = capture.clone();
        v2_event(&mut absent, winner).status = 412;
        assert_eq!(status(&plan, &absent), Status::Failed);
        let mut corrupt = capture.clone();
        v2_event(&mut corrupt, v2_id(&plan, "race-range")).digest[0] ^= 1;
        assert_eq!(status(&plan, &corrupt), Status::Failed);
        let mut reversed = capture.clone();
        let start = v2_event(&mut reversed, loser).start;
        v2_event(&mut reversed, loser).end = start - 1;
        assert_eq!(status(&plan, &reversed), Status::Inconclusive);
        let mut empty = capture.clone();
        let start = v2_event(&mut empty, loser).start;
        v2_event(&mut empty, loser).end = start;
        assert_eq!(status(&plan, &empty), Status::Inconclusive);
        let mut before_winner = capture.clone();
        let start = v2_event(&mut before_winner, winner).start;
        let event = v2_event(&mut before_winner, loser);
        event.start = start - 2;
        event.end = start;
        assert_eq!(status(&plan, &before_winner), Status::Failed);
    }
}
