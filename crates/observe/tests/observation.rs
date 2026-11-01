//! Independent allowlist, projection, loss, concurrency and cardinality contracts.

use hsearch_observe::*;
use hsearch_types::observation::*;
use hsearch_types::{Progress, Sequence};
use std::collections::BTreeSet;
use std::sync::{Arc, Barrier};

fn counter(value: u64) -> MetricCounter {
    MetricCounter::try_new(value).unwrap()
}
fn success() -> Observation {
    Observation::try_new(1, Operation::Search, Outcome::Success, None).unwrap()
}

#[test]
fn schema_golden_projection_preserves_all_fields_and_full_u64() {
    let scope = Scope {
        incarnation: ShardIncarnation::try_new([0x12; 16]).unwrap(),
        activation: u64::MAX,
        shard: 7,
    };
    let progress = Progress::try_new(
        Sequence::new(u64::MAX),
        Sequence::new(9),
        Sequence::new(8),
        Sequence::new(10),
    )
    .unwrap();
    let complete = Completion::try_new(
        counter(5),
        counter(3),
        Completeness::Partial,
        CountRelation::LowerBound,
        Some(counter(17)),
    )
    .unwrap();
    let event = Observation::try_new(
        1,
        Operation::Receipt,
        Outcome::Cancelled,
        Some(FailureClass::DeadlineExceeded),
    )
    .unwrap()
    .with_correlation(Correlation::try_new([0xab; 16], [0xcd; 8]).unwrap())
    .with_progress(scope, progress)
    .unwrap()
    .with_completion(complete)
    .unwrap()
    .with_receipt(ReceiptObservation::committed(
        [0xef; 16],
        scope,
        Sequence::new(u64::MAX),
        Visibility::Timeout,
    ))
    .unwrap()
    .with_resources(ResourceUsage {
        peak_rpcs: Some(counter(2)),
        remote_attempts: Some(counter(5)),
        retries: Some(counter(1)),
        payload_bytes: Some(counter(31)),
        cross_zone_bytes: Some(counter(11)),
        queue_ns: Some(counter(12)),
        cpu_ns: Some(counter(13)),
        pin_count: Some(counter(14)),
        pin_bytes: Some(counter(15)),
        metadata_attempts: Some(counter(2)),
        object_attempts: Some(counter(3)),
    })
    .unwrap();
    let attributes: Vec<_> = project(event).attributes().collect();
    let keys: Vec<_> = attributes.iter().map(|a| a.key).collect();
    assert_eq!(
        keys,
        [
            "hsearch.schema.version",
            "hsearch.operation",
            "hsearch.outcome",
            "error.type",
            "trace_id",
            "span_id",
            "hsearch.shard.incarnation",
            "hsearch.activation",
            "hsearch.shard",
            "hsearch.progress.committed",
            "hsearch.progress.applied",
            "hsearch.progress.searchable",
            "hsearch.progress.archived",
            "hsearch.partitions.selected",
            "hsearch.partitions.completed",
            "hsearch.completeness",
            "hsearch.count.relation",
            "hsearch.count.value",
            "hsearch.receipt.state",
            "hsearch.receipt.id",
            "hsearch.receipt.committed",
            "hsearch.rpc.peak",
            "hsearch.remote.attempts",
            "hsearch.remote.retries",
            "hsearch.payload.bytes",
            "hsearch.cross_zone.bytes",
            "hsearch.queue.ns",
            "hsearch.cpu.ns",
            "hsearch.pins.count",
            "hsearch.pins.bytes",
            "hsearch.metadata.attempts",
            "hsearch.object.attempts"
        ]
    );
    let value = |key| attributes.iter().find(|a| a.key == key).unwrap().value;

    assert_eq!(
        value("hsearch.shard.incarnation"),
        Value::Hex {
            bytes: *b"12121212121212121212121212121212",
            len: 32
        }
    );
    assert_eq!(
        value("hsearch.activation"),
        Value::Decimal(*b"18446744073709551615")
    );
    assert_eq!(
        value("hsearch.shard"),
        Value::Decimal(*b"00000000000000000007")
    );
    assert_eq!(
        value("hsearch.progress.committed"),
        Value::Decimal(*b"18446744073709551615")
    );
    assert_eq!(
        value("hsearch.receipt.committed"),
        Value::Decimal(*b"18446744073709551615")
    );
    assert_eq!(
        value("trace_id"),
        Value::Hex {
            bytes: *b"abababababababababababababababab",
            len: 32
        }
    );
    let mut span = [0; 32];
    span[..16].copy_from_slice(b"cdcdcdcdcdcdcdcd");
    assert_eq!(
        value("span_id"),
        Value::Hex {
            bytes: span,
            len: 16
        }
    );
    assert_eq!(
        value("hsearch.receipt.id"),
        Value::Hex {
            bytes: *b"efefefefefefefefefefefefefefefef",
            len: 32
        }
    );
    assert_eq!(
        value("hsearch.receipt.state"),
        Value::Text("visibility_timeout")
    );
    assert_eq!(
        value("error.type"),
        Value::Text("hsearch.deadline_exceeded")
    );
    assert_eq!(value("hsearch.count.value"), Value::Integer(17));
    assert_eq!(value("hsearch.object.attempts"), Value::Integer(3));
    assert_eq!(attributes.len(), 32);
    assert_eq!(keys.iter().copied().collect::<BTreeSet<_>>().len(), 32);
    assert_eq!(project(event), project(event).clone());
}

#[test]
fn minimization_discards_hostile_text_before_queueing_or_projection() {
    let (producer, consumer) = outbox(1).unwrap();
    let body = "SECRET_DOCUMENT_aa\u{2028}\r\nquery=* credentials=never log";
    let details = UntrustedDetails {
        document: body.as_bytes(),
        query: body,
        credentials: body.as_bytes(),
        provider_message: body,
        tenant: body,
        index: body,
        baggage: body,
    };
    assert_eq!(producer.record(success(), details), Delivery::Queued);
    let event = consumer.try_recv().unwrap();
    assert_eq!(event, success());
    let projection = project(event);
    assert_eq!(projection.attributes().count(), 3);
    for attribute in projection.attributes() {
        assert!(!attribute.key.contains("SECRET"));
        if let Value::Text(value) = attribute.value {
            assert!(!value.contains("SECRET"));
            assert!(!value.contains("query="));
        }
    }
    assert!(!format!("{event:?}{projection:?}{:?}", metric_dimensions(event)).contains(body));
}

#[test]
fn all_receipt_states_and_absence_are_explicit() {
    let scope = Scope {
        incarnation: ShardIncarnation::try_new([0x12; 16]).unwrap(),
        activation: 0,
        shard: 0,
    };
    for (visibility, expected) in [
        (Visibility::Pending, "committed"),
        (Visibility::Visible, "searchable"),
    ] {
        let event = Observation::try_new(1, Operation::Receipt, Outcome::Success, None)
            .unwrap()
            .with_receipt(ReceiptObservation::committed(
                [1; 16],
                scope,
                Sequence::new(0),
                visibility,
            ))
            .unwrap();
        assert!(
            project(event)
                .attributes()
                .any(|a| a.key == "hsearch.receipt.state" && a.value == Value::Text(expected))
        );
        assert!(!project(event).attributes().any(|a| a.key == "error.type"));
    }
    let event = Observation::try_new(
        1,
        Operation::Receipt,
        Outcome::Unknown,
        Some(FailureClass::UnknownOutcome),
    )
    .unwrap()
    .with_receipt(ReceiptObservation::unknown([2; 16], scope))
    .unwrap();
    assert!(
        project(event)
            .attributes()
            .any(|a| a.key == "hsearch.receipt.state" && a.value == Value::Text("unknown_outcome"))
    );
    assert!(
        !project(event)
            .attributes()
            .any(|a| a.key == "hsearch.receipt.committed")
    );
    let completion = Completion::try_new(
        counter(2),
        counter(1),
        Completeness::Unknown,
        CountRelation::Unknown,
        None,
    )
    .unwrap();
    let event = Observation::try_new(
        1,
        Operation::Search,
        Outcome::Partial,
        Some(FailureClass::Unavailable),
    )
    .unwrap()
    .with_completion(completion)
    .unwrap();
    assert!(
        !project(event)
            .attributes()
            .any(|a| a.key == "hsearch.count.value")
    );
}

#[test]
fn metric_dimensions_are_finite_and_exclude_every_diagnostic_identity() {
    let mut observed = BTreeSet::new();
    for operation in Operation::ALL {
        for outcome in Outcome::ALL {
            for failure in std::iter::once(None).chain(FailureClass::ALL.map(Some)) {
                if let Ok(event) = Observation::try_new(1, operation, outcome, failure) {
                    let dimensions = metric_dimensions(event);
                    assert_eq!(dimensions.operation, operation.as_str());
                    assert_eq!(dimensions.outcome, outcome.as_str());
                    assert_eq!(dimensions.error_type, failure.map(FailureClass::as_str));
                    observed.insert((
                        dimensions.operation,
                        dimensions.outcome,
                        dimensions.error_type,
                    ));
                }
            }
        }
    }
    assert!(observed.len() <= 864);
    let scope = Scope {
        incarnation: ShardIncarnation::try_new([0x12; 16]).unwrap(),
        activation: u64::MAX,
        shard: u64::MAX,
    };
    let detailed = success()
        .with_correlation(Correlation::try_new([7; 16], [9; 8]).unwrap())
        .with_progress(scope, Progress::default())
        .unwrap();
    assert_eq!(metric_dimensions(detailed), metric_dimensions(success()));
}

#[test]
fn bounded_queue_full_disconnect_and_fifo_are_observable() {
    assert!(matches!(outbox(0), Err(InvalidCapacity)));
    assert!(matches!(outbox(MAX_CAPACITY + 1), Err(InvalidCapacity)));
    let (producer, consumer) = outbox(2).unwrap();
    assert_eq!(
        consumer.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    );
    let a = success();
    let b = Observation::try_new(1, Operation::Count, Outcome::Success, None).unwrap();
    assert_eq!(
        producer.record(a, UntrustedDetails::default()),
        Delivery::Queued
    );
    assert_eq!(
        producer.record(b, UntrustedDetails::default()),
        Delivery::Queued
    );
    for _ in 0..10000 {
        assert_eq!(
            producer.record(a, UntrustedDetails::default()),
            Delivery::Full
        );
    }
    assert_eq!(consumer.try_recv(), Ok(a));
    assert_eq!(consumer.try_recv(), Ok(b));
    drop(consumer);
    assert_eq!(
        producer.record(a, UntrustedDetails::default()),
        Delivery::Disconnected
    );
    let (producer, consumer) = outbox(MAX_CAPACITY).unwrap();
    drop(producer);
    assert_eq!(
        consumer.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    );
    const {
        assert!(RECORD_BYTES <= 512);
    }
    assert_eq!(MAX_QUEUED_PAYLOAD_BYTES, 1024 * RECORD_BYTES);
}

#[test]
fn stalled_consumer_and_parallel_producers_never_grow_past_capacity() {
    let (producer, consumer) = outbox(8).unwrap();
    let barrier = Arc::new(Barrier::new(5));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let producer = producer.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                (0..1000)
                    .filter(|_| {
                        producer.record(success(), UntrustedDetails::default()) == Delivery::Queued
                    })
                    .count()
            })
        })
        .collect();
    barrier.wait();
    let admitted: usize = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .sum();
    assert_eq!(admitted, 8);
    assert_eq!(std::iter::from_fn(|| consumer.try_recv().ok()).count(), 8);
}

#[test]
fn scoped_progress_uses_existing_producer_and_preserves_all_coordinates() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Producer>();
    assert_send_sync::<ScopedProgressObserver<'_>>();
    let (producer, consumer) = outbox(1).unwrap();
    let scope = Scope {
        incarnation: ShardIncarnation::try_new([0x12; 16]).unwrap(),
        activation: 33,
        shard: 44,
    };
    let scoped = producer.scoped(scope);
    let progress = Progress::try_new(
        Sequence::new(9),
        Sequence::new(7),
        Sequence::new(6),
        Sequence::new(8),
    )
    .unwrap();
    assert_eq!(scoped.try_record(progress), Delivery::Queued);
    assert_eq!(scoped.try_record(progress), Delivery::Full);
    scoped.record(progress);
    let observed = consumer.try_recv().unwrap();
    assert_eq!(observed.operation(), Operation::Checkpoint);
    assert_eq!(observed.scope(), Some(scope));
    assert_eq!(observed.progress(), Some(progress));
    scoped.record(Progress::default());
    assert_eq!(
        consumer.try_recv().unwrap().progress(),
        Some(Progress::default())
    );
    drop(consumer);
    assert_eq!(scoped.try_record(progress), Delivery::Disconnected);
}

#[test]
fn metric_instrument_schema_units_and_samples_have_independent_goldens() {
    assert_eq!(
        INSTRUMENTS.map(|i| i.name),
        [
            "hsearch.observations",
            "hsearch.partitions.selected",
            "hsearch.partitions.completed",
            "hsearch.rpc.peak",
            "hsearch.remote.attempts",
            "hsearch.remote.retries",
            "hsearch.payload.bytes",
            "hsearch.cross_zone.bytes",
            "hsearch.queue.duration",
            "hsearch.cpu.duration",
            "hsearch.pins.count",
            "hsearch.pins.bytes",
            "hsearch.metadata.attempts",
            "hsearch.object.attempts"
        ]
    );
    assert_eq!(
        INSTRUMENTS.map(|i| i.unit),
        [
            "{observation}",
            "{partition}",
            "{partition}",
            "{call}",
            "{attempt}",
            "{attempt}",
            "By",
            "By",
            "ns",
            "ns",
            "{pin}",
            "By",
            "{attempt}",
            "{attempt}"
        ]
    );
    assert_eq!(INSTRUMENTS[0].kind, InstrumentKind::Counter);
    assert!(
        INSTRUMENTS[1..]
            .iter()
            .all(|i| i.kind == InstrumentKind::Histogram)
    );
    let completion = Completion::try_new(
        counter(5),
        counter(5),
        Completeness::Complete,
        CountRelation::Exact,
        Some(counter(99)),
    )
    .unwrap();
    let event = success()
        .with_completion(completion)
        .unwrap()
        .with_resources(ResourceUsage {
            peak_rpcs: Some(counter(2)),
            remote_attempts: Some(counter(5)),
            retries: Some(counter(1)),
            payload_bytes: Some(counter(31)),
            cross_zone_bytes: Some(counter(11)),
            queue_ns: Some(counter(12)),
            cpu_ns: Some(counter(13)),
            pin_count: Some(counter(14)),
            pin_bytes: Some(counter(15)),
            metadata_attempts: Some(counter(2)),
            object_attempts: Some(counter(3)),
        })
        .unwrap();
    let metrics: Vec<_> = measurements(event).collect();
    assert_eq!(metrics.len(), 14);
    assert_eq!(
        metrics.iter().map(|m| m.value).collect::<Vec<_>>(),
        [1, 5, 5, 2, 5, 1, 31, 11, 12, 13, 14, 15, 2, 3]
    );
    for (index, metric) in metrics.iter().enumerate() {
        assert_eq!(metric.instrument, &INSTRUMENTS[index]);
        assert_eq!(metric.dimensions, metric_dimensions(success()));
    }
    assert_eq!(measurements(success()).count(), 1);
    let max_event = success()
        .with_resources(ResourceUsage {
            payload_bytes: Some(counter(i64::MAX as u64)),
            ..ResourceUsage::default()
        })
        .unwrap();
    assert_eq!(
        measurements(max_event)
            .find(|m| m.instrument.name == "hsearch.payload.bytes")
            .unwrap()
            .value,
        i64::MAX
    );
}

#[test]
fn missing_resource_fields_are_not_projected_as_zero_measurements() {
    let missing = success().with_resources(ResourceUsage::default()).unwrap();
    assert_eq!(measurements(missing).count(), 1);
    assert_eq!(project(missing).attributes().count(), 3);
    let measured = success()
        .with_resources(ResourceUsage {
            payload_bytes: Some(counter(0)),
            ..ResourceUsage::default()
        })
        .unwrap();
    let metrics: Vec<_> = measurements(measured).collect();
    assert_eq!(metrics.len(), 2);
    assert_eq!(metrics[1].instrument.name, "hsearch.payload.bytes");
    assert_eq!(metrics[1].value, 0);
    assert_eq!(
        project(measured)
            .attributes()
            .filter(|a| a.key == "hsearch.payload.bytes")
            .map(|a| a.value)
            .collect::<Vec<_>>(),
        [Value::Integer(0)]
    );
    let independent = ResourceUsage {
        peak_rpcs: Some(counter(7)),
        retries: Some(counter(4)),
        metadata_attempts: Some(counter(3)),
        ..ResourceUsage::default()
    };
    assert!(success().with_resources(independent).is_ok());
    assert_eq!(
        success().with_resources(ResourceUsage {
            remote_attempts: Some(counter(0)),
            ..independent
        }),
        Err(InvalidObservation::Resources)
    );
}

#[test]
fn incarnation_is_lossless_diagnostic_identity_and_never_a_metric_dimension() {
    let first = Scope {
        incarnation: ShardIncarnation::try_new([
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
        ])
        .unwrap(),
        activation: u64::MAX,
        shard: u64::MAX,
    };
    let second = Scope {
        incarnation: ShardIncarnation::try_new([0xff; 16]).unwrap(),
        ..first
    };
    let a = success().with_progress(first, Progress::default()).unwrap();
    let b = success()
        .with_progress(second, Progress::default())
        .unwrap();
    let incarnation = |event| {
        project(event)
            .attributes()
            .find(|attribute| attribute.key == "hsearch.shard.incarnation")
            .unwrap()
            .value
    };
    assert_eq!(
        incarnation(a),
        Value::Hex {
            bytes: *b"000102030405060708090a0b0c0d0e0f",
            len: 32
        }
    );
    assert_ne!(incarnation(a), incarnation(b));
    assert_eq!(metric_dimensions(a), metric_dimensions(b));
    assert_eq!(
        measurements(a).collect::<Vec<_>>(),
        measurements(b).collect::<Vec<_>>()
    );
    let (producer, consumer) = outbox(1).unwrap();
    let scoped = producer.scoped(second);
    assert_eq!(scoped.try_record(Progress::default()), Delivery::Queued);
    assert_eq!(consumer.try_recv().unwrap().scope(), Some(second));
    println!(
        r#"{{"observability_layout":"pass","profile":"{}","architecture":"{}","record_bytes":{},"max_capacity":{},"queued_payload_bytes":{},"max_fields":{}}}"#,
        if cfg!(debug_assertions) {
            "dev"
        } else {
            "release"
        },
        std::env::consts::ARCH,
        RECORD_BYTES,
        MAX_CAPACITY,
        MAX_QUEUED_PAYLOAD_BYTES,
        MAX_FIELDS
    );
}

#[test]
fn committed_but_unavailable_projection_preserves_the_owner_facts() {
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
    let event = Observation::try_new(
        1,
        Operation::Receipt,
        Outcome::Failed,
        Some(FailureClass::Unavailable),
    )
    .unwrap()
    .with_receipt(receipt)
    .unwrap()
    .with_progress(scope, progress)
    .unwrap();
    let attributes: Vec<_> = project(event).attributes().collect();
    let value = |key| {
        attributes
            .iter()
            .find(|attribute| attribute.key == key)
            .unwrap()
            .value
    };
    assert_eq!(value("hsearch.outcome"), Value::Text("failed"));
    assert_eq!(value("error.type"), Value::Text("hsearch.unavailable"));
    assert_eq!(value("hsearch.receipt.state"), Value::Text("committed"));
    assert_eq!(
        value("hsearch.receipt.id"),
        Value::Hex {
            bytes: *b"34343434343434343434343434343434",
            len: 32
        }
    );
    assert_eq!(
        value("hsearch.receipt.committed"),
        Value::Decimal(*b"00000000000000000012")
    );
    for (key, digits) in [
        ("hsearch.progress.committed", *b"00000000000000000012"),
        ("hsearch.progress.applied", *b"00000000000000000007"),
        ("hsearch.progress.searchable", *b"00000000000000000005"),
        ("hsearch.progress.archived", *b"00000000000000000009"),
    ] {
        assert_eq!(value(key), Value::Decimal(digits));
    }
    assert_eq!(event.receipt(), Some(receipt));
    assert_eq!(event.progress(), Some(progress));
}
