//! Versioned observation category and lossless metric-counter contracts.

use hsearch_types::observation::{
    CounterOutOfRange, FailureClass, MetricCounter, Operation, Outcome, SCHEMA_VERSION,
};

#[test]
fn version_one_labels_and_cardinality_are_fixed() {
    assert_eq!(SCHEMA_VERSION, 1);
    assert_eq!(
        Operation::ALL.map(Operation::as_str),
        [
            "admission",
            "ingest",
            "search",
            "count",
            "receipt",
            "snapshot",
            "partition_rpc",
            "object_io",
            "checkpoint",
            "apply",
            "refresh",
            "maintenance"
        ]
    );
    assert_eq!(
        Outcome::ALL.map(Outcome::as_str),
        [
            "success",
            "partial",
            "rejected",
            "failed",
            "cancelled",
            "unknown"
        ]
    );
    assert_eq!(
        FailureClass::ALL.map(FailureClass::as_str),
        [
            "hsearch.invalid_input",
            "hsearch.unsupported",
            "hsearch.permission_denied",
            "hsearch.overloaded",
            "hsearch.conflict",
            "hsearch.unknown_outcome",
            "hsearch.unavailable",
            "hsearch.corrupt",
            "hsearch.cancelled",
            "hsearch.deadline_exceeded",
            "hsearch.internal"
        ]
    );
    assert_eq!(
        Operation::ALL.len() * Outcome::ALL.len() * (FailureClass::ALL.len() + 1),
        864
    );
}

#[test]
fn metric_boundaries_and_addition_preserve_exact_counts() {
    assert_eq!(MetricCounter::default(), MetricCounter::ZERO);
    assert_eq!(MetricCounter::ZERO.get(), 0);
    assert_eq!(MetricCounter::ZERO.as_i64(), 0);
    let max = MetricCounter::try_new(i64::MAX as u64).unwrap();
    assert_eq!(max.get(), i64::MAX as u64);
    assert_eq!(max.as_i64(), i64::MAX);
    assert_eq!(
        MetricCounter::try_new((i64::MAX as u64) + 1),
        Err(CounterOutOfRange)
    );
    assert_eq!(MetricCounter::try_new(u64::MAX), Err(CounterOutOfRange));
    assert_eq!(max.checked_add(MetricCounter::ZERO), Ok(max));
    assert_eq!(
        max.checked_add(MetricCounter::try_new(1).unwrap()),
        Err(CounterOutOfRange)
    );
    assert_eq!(
        MetricCounter::try_new(4)
            .unwrap()
            .checked_add(MetricCounter::try_new(5).unwrap())
            .unwrap()
            .get(),
        9
    );
}

use hsearch_types::observation::{
    Completeness, Completion, Correlation, CountRelation, InvalidObservation, Observation,
    ReceiptObservation, ResourceUsage, Scope, ShardIncarnation, Visibility,
};
use hsearch_types::{Progress, Sequence};

fn counter(value: u64) -> MetricCounter {
    MetricCounter::try_new(value).unwrap()
}
fn event(outcome: Outcome, failure: Option<FailureClass>) -> Observation {
    Observation::try_new(1, Operation::Receipt, outcome, failure).unwrap()
}

#[test]
fn versions_and_outcome_error_presence_follow_independent_matrix() {
    for version in [0, 2, u16::MAX] {
        assert_eq!(
            Observation::try_new(version, Operation::Search, Outcome::Success, None),
            Err(InvalidObservation::Version)
        );
    }
    for operation in Operation::ALL {
        for outcome in Outcome::ALL {
            for failure in std::iter::once(None).chain(FailureClass::ALL.map(Some)) {
                let valid = match (outcome, failure) {
                    (Outcome::Success, None) => true,
                    (Outcome::Unknown, Some(FailureClass::UnknownOutcome)) => true,
                    (Outcome::Success | Outcome::Unknown, _)
                    | (_, None | Some(FailureClass::UnknownOutcome)) => false,
                    _ => true,
                };
                let actual = Observation::try_new(1, operation, outcome, failure);
                assert_eq!(actual.is_ok(), valid);
                if let Ok(observation) = actual {
                    assert_eq!(observation.operation(), operation);
                    assert_eq!(observation.outcome(), outcome);
                    assert_eq!(observation.failure(), failure);
                    assert_eq!(observation.scope(), None);
                    assert_eq!(observation.progress(), None);
                    assert_eq!(observation.completion(), None);
                    assert_eq!(observation.receipt(), None);
                    assert_eq!(observation.resources(), None);
                    assert_eq!(observation.correlation(), None);
                } else {
                    assert_eq!(actual, Err(InvalidObservation::Outcome));
                }
            }
        }
    }
}

#[test]
fn completion_count_and_scope_oracles_preserve_unknown_and_partial() {
    for selected in 0..3 {
        for completed in 0..3 {
            for completeness in [
                Completeness::Complete,
                Completeness::Partial,
                Completeness::Unknown,
            ] {
                for relation in [
                    CountRelation::Exact,
                    CountRelation::LowerBound,
                    CountRelation::Unknown,
                ] {
                    for count in [None, Some(counter(0)), Some(counter(7))] {
                        let valid = completed <= selected
                            && (completeness != Completeness::Complete || completed == selected)
                            && (relation != CountRelation::Exact
                                || completeness == Completeness::Complete)
                            && match relation {
                                CountRelation::Unknown => count.is_none(),
                                _ => count.is_some(),
                            };
                        let actual = Completion::try_new(
                            counter(selected),
                            counter(completed),
                            completeness,
                            relation,
                            count,
                        );
                        assert_eq!(actual.is_ok(), valid);
                        if let Ok(completion) = actual {
                            assert_eq!(completion.selected(), counter(selected));
                            assert_eq!(completion.completed(), counter(completed));
                            assert_eq!(completion.completeness(), completeness);
                            assert_eq!(completion.relation(), relation);
                            assert_eq!(completion.count(), count);
                            for (outcome, failure) in [
                                (Outcome::Success, None),
                                (Outcome::Partial, Some(FailureClass::Unavailable)),
                                (Outcome::Failed, Some(FailureClass::Corrupt)),
                            ] {
                                assert_eq!(
                                    event(outcome, failure).with_completion(completion).is_ok(),
                                    (outcome == Outcome::Success)
                                        == (completeness == Completeness::Complete)
                                );
                            }
                        } else {
                            assert_eq!(actual, Err(InvalidObservation::Completion));
                        }
                    }
                }
            }
        }
    }
    assert_eq!(
        [
            Completeness::Complete,
            Completeness::Partial,
            Completeness::Unknown
        ]
        .map(Completeness::as_str),
        ["complete", "partial", "unknown"]
    );
    assert_eq!(
        [
            CountRelation::Exact,
            CountRelation::LowerBound,
            CountRelation::Unknown
        ]
        .map(CountRelation::as_str),
        ["eq", "gte", "unknown"]
    );
}

#[test]
fn resource_subsets_retries_and_overflow_are_checked() {
    for attempts in 0..4 {
        for peak in 0..4 {
            for retries in 0..4 {
                for metadata in 0..4 {
                    for objects in 0..4 {
                        let resource = ResourceUsage {
                            remote_attempts: Some(counter(attempts)),
                            peak_rpcs: Some(counter(peak)),
                            retries: Some(counter(retries)),
                            metadata_attempts: Some(counter(metadata)),
                            object_attempts: Some(counter(objects)),
                            ..ResourceUsage::default()
                        };
                        let valid = peak <= attempts
                            && retries <= attempts
                            && metadata + objects <= attempts;
                        assert_eq!(
                            event(Outcome::Success, None)
                                .with_resources(resource)
                                .is_ok(),
                            valid
                        );
                    }
                }
            }
        }
    }
    let resource = ResourceUsage {
        remote_attempts: Some(counter(i64::MAX as u64)),
        metadata_attempts: Some(counter(i64::MAX as u64)),
        object_attempts: Some(counter(1)),
        ..ResourceUsage::default()
    };
    assert_eq!(
        event(Outcome::Success, None).with_resources(resource),
        Err(InvalidObservation::Resources)
    );
    assert_eq!(
        event(Outcome::Success, None)
            .with_resources(ResourceUsage::default())
            .unwrap()
            .resources(),
        Some(ResourceUsage::default())
    );
}

#[test]
fn receipt_scope_checkpoint_and_visibility_timeout_do_not_rollback() {
    let scope = Scope {
        incarnation: ShardIncarnation::try_new([0x12; 16]).unwrap(),
        activation: 5,
        shard: u64::MAX,
    };
    let other = Scope {
        activation: 6,
        ..scope
    };
    let committed = Sequence::new(u64::MAX);
    let progress = Progress::try_new(
        committed,
        Sequence::new(7),
        Sequence::new(2),
        Sequence::new(9),
    )
    .unwrap();
    let timeout = ReceiptObservation::committed([3; 16], scope, committed, Visibility::Timeout);
    assert_eq!(timeout.id(), [3; 16]);
    assert_eq!(timeout.scope(), scope);
    assert_eq!(timeout.sequence(), Some(committed));
    assert_eq!(timeout.visibility(), Some(Visibility::Timeout));
    let observed = event(Outcome::Cancelled, Some(FailureClass::DeadlineExceeded))
        .with_progress(scope, progress)
        .unwrap()
        .with_receipt(timeout)
        .unwrap();
    assert_eq!(observed.scope(), Some(scope));
    assert_eq!(observed.progress(), Some(progress));
    assert_eq!(observed.receipt(), Some(timeout));
    assert_eq!(
        observed.with_progress(other, progress),
        Err(InvalidObservation::Scope)
    );
    assert_eq!(
        observed.with_progress(scope, Progress::default()),
        Err(InvalidObservation::Receipt)
    );
    assert_eq!(
        event(Outcome::Cancelled, Some(FailureClass::Cancelled)).with_receipt(timeout),
        Err(InvalidObservation::Receipt)
    );
    assert_eq!(
        event(Outcome::Success, None).with_receipt(timeout),
        Err(InvalidObservation::Receipt)
    );
    assert_eq!(
        event(Outcome::Cancelled, Some(FailureClass::DeadlineExceeded))
            .with_receipt(timeout)
            .unwrap()
            .with_progress(scope, Progress::default()),
        Err(InvalidObservation::Receipt)
    );
    assert_eq!(
        event(Outcome::Cancelled, Some(FailureClass::DeadlineExceeded))
            .with_progress(scope, Progress::default())
            .unwrap()
            .with_receipt(timeout),
        Err(InvalidObservation::Receipt)
    );
    assert_eq!(
        event(Outcome::Cancelled, Some(FailureClass::DeadlineExceeded))
            .with_progress(other, progress)
            .unwrap()
            .with_receipt(timeout),
        Err(InvalidObservation::Scope)
    );
    let unknown = ReceiptObservation::unknown([4; 16], scope);
    assert_eq!(unknown.sequence(), None);
    assert_eq!(unknown.visibility(), None);
    assert!(
        event(Outcome::Unknown, Some(FailureClass::UnknownOutcome))
            .with_receipt(unknown)
            .is_ok()
    );
    assert_eq!(
        event(Outcome::Success, None).with_receipt(unknown),
        Err(InvalidObservation::Receipt)
    );
    for visibility in [Visibility::Pending, Visibility::Visible] {
        let receipt = ReceiptObservation::committed([5; 16], scope, committed, visibility);
        assert!(event(Outcome::Success, None).with_receipt(receipt).is_ok());
        // Explicit migration of the private Success-only oracle: architecture
        // requires known commitment to survive a downstream application failure.
        let downstream = event(Outcome::Failed, Some(FailureClass::Internal))
            .with_receipt(receipt)
            .unwrap();
        assert_eq!(downstream.receipt(), Some(receipt));
        assert_eq!(downstream.outcome(), Outcome::Failed);
        assert_eq!(downstream.failure(), Some(FailureClass::Internal));
        assert_eq!(
            event(Outcome::Rejected, Some(FailureClass::Overloaded)).with_receipt(receipt),
            Err(InvalidObservation::Receipt)
        );
        assert_eq!(
            Observation::try_new(1, Operation::Search, Outcome::Success, None)
                .unwrap()
                .with_receipt(receipt),
            Err(InvalidObservation::Receipt)
        );
        assert!(
            Observation::try_new(1, Operation::Ingest, Outcome::Success, None)
                .unwrap()
                .with_receipt(receipt)
                .is_ok()
        );
    }
}

#[test]
fn trusted_correlation_rejects_all_zero_without_text_or_allocations() {
    assert_eq!(
        Correlation::try_new([0; 16], [1; 8]),
        Err(InvalidObservation::Correlation)
    );
    assert_eq!(
        Correlation::try_new([1; 16], [0; 8]),
        Err(InvalidObservation::Correlation)
    );
    let correlation = Correlation::try_new([0xab; 16], [0xcd; 8]).unwrap();
    assert_eq!(correlation.trace_id(), [0xab; 16]);
    assert_eq!(correlation.span_id(), [0xcd; 8]);
    assert_eq!(
        event(Outcome::Success, None)
            .with_correlation(correlation)
            .correlation(),
        Some(correlation)
    );
}

#[test]
fn incarnation_validation_and_cross_incarnation_composition_preserve_identity() {
    assert_eq!(
        ShardIncarnation::try_new([0; 16]),
        Err(InvalidObservation::Scope)
    );
    for index in 0..16 {
        let mut bytes = [0; 16];
        bytes[index] = 1;
        let incarnation = ShardIncarnation::try_new(bytes).unwrap();
        assert_eq!(incarnation.bytes(), bytes);
    }
    let scope = Scope {
        incarnation: ShardIncarnation::try_new([1; 16]).unwrap(),
        activation: 7,
        shard: 11,
    };
    let recreated = Scope {
        incarnation: ShardIncarnation::try_new([2; 16]).unwrap(),
        ..scope
    };
    assert_eq!(scope.activation, recreated.activation);
    assert_eq!(scope.shard, recreated.shard);
    assert_ne!(scope, recreated);
    let receipt =
        ReceiptObservation::committed([3; 16], scope, Sequence::new(0), Visibility::Visible);
    let start = event(Outcome::Success, None);
    assert_eq!(
        start
            .with_progress(recreated, Progress::default())
            .unwrap()
            .with_receipt(receipt),
        Err(InvalidObservation::Scope)
    );
    assert_eq!(
        start
            .with_receipt(receipt)
            .unwrap()
            .with_progress(recreated, Progress::default()),
        Err(InvalidObservation::Scope)
    );
    assert!(
        start
            .with_receipt(receipt)
            .unwrap()
            .with_progress(scope, Progress::default())
            .is_ok()
    );
}

#[test]
fn known_commit_and_application_failure_are_independent_in_both_orders() {
    let scope = Scope {
        incarnation: ShardIncarnation::try_new([0x12; 16]).unwrap(),
        activation: 17,
        shard: 3,
    };
    let progress = Progress::try_new(
        Sequence::new(12),
        Sequence::new(7),
        Sequence::new(5),
        Sequence::new(9),
    )
    .unwrap();
    let receipt =
        ReceiptObservation::committed([0x34; 16], scope, Sequence::new(12), Visibility::Pending);
    let start = event(Outcome::Failed, Some(FailureClass::Unavailable));
    let first = start
        .with_progress(scope, progress)
        .unwrap()
        .with_receipt(receipt)
        .unwrap();
    let second = start
        .with_receipt(receipt)
        .unwrap()
        .with_progress(scope, progress)
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(first.receipt(), Some(receipt));
    assert_eq!(first.progress(), Some(progress));
    assert_eq!(first.outcome(), Outcome::Failed);
    assert_eq!(first.failure(), Some(FailureClass::Unavailable));
    assert_eq!(receipt.sequence(), Some(Sequence::new(12)));
    for wrong_scope in [
        Scope {
            activation: 18,
            ..scope
        },
        Scope { shard: 4, ..scope },
        Scope {
            incarnation: ShardIncarnation::try_new([0x56; 16]).unwrap(),
            ..scope
        },
    ] {
        assert_eq!(
            start
                .with_progress(wrong_scope, progress)
                .unwrap()
                .with_receipt(receipt),
            Err(InvalidObservation::Scope)
        );
        assert_eq!(
            start
                .with_receipt(receipt)
                .unwrap()
                .with_progress(wrong_scope, progress),
            Err(InvalidObservation::Scope)
        );
    }
    let beyond =
        ReceiptObservation::committed([0x34; 16], scope, Sequence::new(13), Visibility::Pending);
    assert_eq!(
        start
            .with_progress(scope, progress)
            .unwrap()
            .with_receipt(beyond),
        Err(InvalidObservation::Receipt)
    );
    assert_eq!(
        start
            .with_receipt(beyond)
            .unwrap()
            .with_progress(scope, progress),
        Err(InvalidObservation::Receipt)
    );

    // Literal contract table from the architecture/design review, not a match-arm oracle.
    // Columns: outcome, error, known Pending/Visible, explicit Timeout, unknown receipt.
    for (outcome, failure, known, timeout, unknown) in [
        (Outcome::Success, None, true, false, false),
        (
            Outcome::Failed,
            Some(FailureClass::Unavailable),
            true,
            false,
            false,
        ),
        (
            Outcome::Failed,
            Some(FailureClass::Internal),
            true,
            false,
            false,
        ),
        (
            Outcome::Failed,
            Some(FailureClass::Corrupt),
            true,
            false,
            false,
        ),
        (
            Outcome::Rejected,
            Some(FailureClass::Overloaded),
            false,
            false,
            false,
        ),
        (
            Outcome::Partial,
            Some(FailureClass::Unavailable),
            false,
            false,
            false,
        ),
        (
            Outcome::Cancelled,
            Some(FailureClass::DeadlineExceeded),
            false,
            true,
            false,
        ),
        (
            Outcome::Cancelled,
            Some(FailureClass::Cancelled),
            false,
            false,
            false,
        ),
        (
            Outcome::Unknown,
            Some(FailureClass::UnknownOutcome),
            false,
            false,
            true,
        ),
    ] {
        for visibility in [
            Visibility::Pending,
            Visibility::Visible,
            Visibility::Timeout,
        ] {
            let receipt =
                ReceiptObservation::committed([0x34; 16], scope, Sequence::new(12), visibility);
            let expected = match visibility {
                Visibility::Pending | Visibility::Visible => known,
                Visibility::Timeout => timeout,
            };
            assert_eq!(
                event(outcome, failure).with_receipt(receipt).is_ok(),
                expected
            );
        }
        assert_eq!(
            event(outcome, failure)
                .with_receipt(ReceiptObservation::unknown([0x34; 16], scope))
                .is_ok(),
            unknown
        );
    }
}
