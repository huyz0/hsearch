//! Required-matrix completeness, real race overlap and decisive negative cases.
use hsearch_object_probe::{
    eligibility::{self, Status},
    model::*,
};

fn race(winners: usize, overlap: bool) -> (CapturePlan, Capture) {
    let run = "0".repeat(32);
    let mut plan = CapturePlan {
        binding: Binding {
            digests: [[0; 32]; 7],
            run: [0; 16],
            platform: 1,
        },
        bucket: "bucket".into(),
        specifications: Vec::new(),
    };
    let mut capture = Capture {
        complete: true,
        attempts: 16,
        ..Capture::default()
    };
    for i in 0..16u16 {
        plan.specifications.push(Specification {
            id: i + 1,
            scenario: 0,
            client: i as u8,
            action: "create".into(),
            key: "key".into(),
            race: 1,
            length: 1,
            ..Specification::default()
        });
        capture.events.push(CaptureEvent {
            id: i + 1,
            attempt: i + 1,
            start: if overlap {
                u64::from(i)
            } else {
                u64::from(i) * 2
            },
            end: if overlap { 100 } else { u64::from(i) * 2 + 1 },
            status: if usize::from(i) < winners { 200 } else { 412 },
            boundary: "reply".into(),
            sent: 100,
            request: format!("{run}-{}", i + 1),
            condition: "*".into(),
            etag: format!("tag-{i}"),
            version: format!("version-{i}"),
            ..CaptureEvent::default()
        });
    }
    (plan, capture)
}

#[test]
fn serialized_sends_are_inconclusive_not_a_many_client_race() {
    let (plan, capture) = race(1, false);
    let report = eligibility::evaluate(&plan, &capture, "{}");
    assert!(!report.ready());
    assert_eq!(report.scenarios[0], Status::Inconclusive);
}

#[test]
fn two_successors_are_decisively_rejected_even_if_other_matrix_areas_missing() {
    let (plan, capture) = race(2, true);
    let report = eligibility::evaluate(&plan, &capture, "{}");
    assert!(!report.ready());
    assert_eq!(report.scenarios[0], Status::Failed);
}

#[test]
fn malformed_typed_values_and_missing_attempts_never_panic_or_pass() {
    let (mut plan, mut capture) = race(1, true);
    plan.specifications[0].scenario = 255;
    assert!(!eligibility::evaluate(&plan, &capture, "{}").ready());
    plan.specifications[0].scenario = 0;
    capture.events[1].attempt = capture.events[0].attempt;
    assert!(!eligibility::evaluate(&plan, &capture, "{}").ready());
    capture.complete = false;
    assert!(!eligibility::evaluate(&plan, &capture, "{}").ready());
}

#[test]
fn unavailable_winner_can_have_committed_and_is_not_inferred_rollback() {
    let (plan, mut capture) = race(1, true);
    capture.events[0].status = 503;
    let report = eligibility::evaluate(&plan, &capture, "{}");
    assert_eq!(report.scenarios[0], Status::Inconclusive);
    assert!(!report.ready());
}
