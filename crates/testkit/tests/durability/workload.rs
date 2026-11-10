//! Fixed offered schedules and admission accounting, independent of protocol completion.
use super::common::{Batch, Context, Operation};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) type Lane = (u64, u64);

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Event {
    pub(super) id: String,
    pub(super) at_us: u64,
    stream: u64,
    tenant: u64,
    key: u64,
    payload_sha256: String,
}

impl Event {
    fn operation(&self) -> Operation {
        let mut payload = format!(
            "id={};tenant={};key={};stream={};",
            self.id, self.tenant, self.key, self.stream
        );
        assert!(payload.len() <= 1024);
        payload.extend(std::iter::repeat_n('#', 1024 - payload.len()));
        Operation {
            id: self.id.clone(),
            tenant: self.tenant,
            key: self.key,
            stream: self.stream,
            payload,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Trace {
    pub(super) workload: String,
    pub(super) seed: u64,
    pub(super) events: Vec<Event>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: u64,
    offer_window_us: u64,
    payload_bytes: u64,
    generator: String,
    traces: Vec<Trace>,
}

pub(super) fn fixtures() -> Vec<Trace> {
    let fixture: Fixture =
        serde_json::from_slice(include_bytes!("../fixtures/durability-traces.json")).unwrap();
    assert_eq!(fixture.schema_version, 1);
    assert_eq!(fixture.offer_window_us, 500000);
    assert_eq!(fixture.payload_bytes, 1024);
    assert_eq!(fixture.generator, "lcg32-fisher-yates-v1");
    assert_eq!(fixture.traces.len(), 9);
    let mut cases = BTreeSet::new();
    for trace in &fixture.traces {
        assert!(
            cases.insert((trace.workload.clone(), trace.seed)) && [7, 19, 43].contains(&trace.seed)
        );
        let count = if trace.workload == "sparse" {
            16
        } else {
            assert!(["busy", "skewed"].contains(&trace.workload.as_str()));
            128
        };
        assert_eq!(trace.events.len(), count);
        let mut ids = BTreeSet::new();
        for (ordinal, event) in trace.events.iter().enumerate() {
            assert!(ids.insert(&event.id) && event.id.len() <= 128 && event.id.is_ascii());
            assert!(
                event.at_us < 500000
                    && (ordinal == 0 || trace.events[ordinal - 1].at_us <= event.at_us)
            );
            assert!(event.stream < 16 && event.tenant < 4 && event.key < 2);
            assert_eq!(event.tenant, event.stream / 4);
            assert_eq!(event.key, event.stream % 2);
            assert!(
                event.payload_sha256.len() == 64
                    && event
                        .payload_sha256
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
        }
        if trace.workload == "skewed" {
            assert_eq!(
                trace
                    .events
                    .iter()
                    .filter(|event| event.tenant == 0)
                    .count(),
                102
            );
        }
    }
    fixture.traces
}

#[derive(Clone)]
pub(super) struct Planned {
    pub(super) ticket: usize,
    pub(super) lane: Lane,
    pub(super) emit_us: u64,
    pub(super) trigger: &'static str,
    pub(super) batch: Batch,
    pub(super) offered_us: Vec<u64>,
    pub(super) first_ordinal: usize,
}

pub(super) fn plan(trace: &Trace, wait_us: u64) -> Vec<Planned> {
    assert!([50000, 250000].contains(&wait_us));
    let mut lanes: BTreeMap<Lane, Vec<(usize, &Event)>> = BTreeMap::new();
    for (ordinal, event) in trace.events.iter().enumerate() {
        lanes
            .entry((event.tenant, event.key))
            .or_default()
            .push((ordinal, event));
    }
    let mut output = vec![];
    // A lane scan is independent of the Python generator's pending-map implementation.
    for (lane, events) in lanes {
        let mut cursor = 0;
        while cursor < events.len() {
            let begin = cursor;
            let deadline = events[begin].1.at_us + wait_us;
            while cursor < events.len() && events[cursor].1.at_us < deadline && cursor - begin < 16
            {
                cursor += 1;
            }
            assert!(cursor > begin);
            let full = cursor - begin == 16;
            let emit_us = if full {
                events[cursor - 1].1.at_us
            } else {
                deadline
            };
            let batch = Batch {
                operations: events[begin..cursor]
                    .iter()
                    .map(|(_, event)| event.operation())
                    .collect(),
            };
            batch.validate();
            output.push(Planned {
                ticket: 0,
                lane,
                emit_us,
                trigger: if full { "bytes" } else { "timer" },
                batch,
                offered_us: events[begin..cursor]
                    .iter()
                    .map(|(_, event)| event.at_us)
                    .collect(),
                first_ordinal: events[begin].0,
            });
        }
    }
    output.sort_by_key(|row| (row.emit_us, row.lane, row.first_ordinal));
    for (ticket, row) in output.iter_mut().enumerate() {
        row.ticket = ticket;
        assert!(row.emit_us <= 750000);
    }
    assert_eq!(
        output
            .iter()
            .map(|row| row.batch.operations.len())
            .sum::<usize>(),
        trace.events.len()
    );
    output
}

#[derive(Clone)]
pub(super) struct Pending {
    pub(super) planned: Planned,
    pub(super) enqueued_us: u64,
    encoded_bytes: usize,
}

#[derive(Clone, Debug)]
pub(super) struct Offer {
    pub(super) ticket: usize,
    pub(super) ids: Vec<String>,
    pub(super) planned_us: u64,
    pub(super) observed_us: u64,
    pub(super) admitted: bool,
}

#[derive(Default)]
pub(super) struct Mailbox {
    queues: BTreeMap<Lane, VecDeque<Pending>>,
    // Includes queued AND active records until their result is resolved.
    resident: BTreeMap<Lane, usize>,
    active: BTreeMap<Lane, (usize, usize, usize)>,
    encoded_resident: usize,
    pub(super) peak_records: usize,
    pub(super) peak_encoded_bytes: usize,
    pub(super) offers: Vec<Offer>,
    pub(super) done: bool,
}

impl Mailbox {
    fn offer(&mut self, planned: Planned, observed_us: u64) {
        assert!(
            self.offers.len() < 128 && observed_us >= planned.emit_us && observed_us < 15000000
        );
        planned.batch.validate();
        let count = planned.batch.operations.len();
        let encoded_bytes = serde_json::to_vec(&planned.batch).unwrap().len();
        assert!(encoded_bytes <= 32768);
        let lane = planned.lane;
        let resident = *self.resident.get(&lane).unwrap_or(&0);
        let total = self.resident.values().sum::<usize>();
        let admitted = resident + count <= 32
            && total + count <= 512
            && self.encoded_resident + encoded_bytes <= 1048576;
        self.offers.push(Offer {
            ticket: planned.ticket,
            ids: planned
                .batch
                .operations
                .iter()
                .map(|op| op.id.clone())
                .collect(),
            planned_us: planned.emit_us,
            observed_us,
            admitted,
        });
        if admitted {
            self.resident.insert(lane, resident + count);
            self.encoded_resident += encoded_bytes;
            self.peak_records = self.peak_records.max(total + count);
            self.peak_encoded_bytes = self.peak_encoded_bytes.max(self.encoded_resident);
            self.queues.entry(lane).or_default().push_back(Pending {
                planned,
                enqueued_us: observed_us,
                encoded_bytes,
            });
        }
    }
    pub(super) fn take_available(&mut self) -> Vec<Pending> {
        assert!(self.active.len() <= 4);
        let mut available = self
            .queues
            .iter()
            .filter(|(lane, queue)| !queue.is_empty() && !self.active.contains_key(*lane))
            .map(|(lane, queue)| (queue[0].planned.emit_us, *lane))
            .collect::<Vec<_>>();
        available.sort();
        let mut output = vec![];
        for (_, lane) in available.into_iter().take(4 - self.active.len()) {
            let pending = self.queues.get_mut(&lane).unwrap().pop_front().unwrap();
            assert!(
                self.active
                    .insert(
                        lane,
                        (
                            pending.planned.ticket,
                            pending.planned.batch.operations.len(),
                            pending.encoded_bytes
                        )
                    )
                    .is_none()
            );
            output.push(pending);
        }
        output
    }
    pub(super) fn complete(&mut self, lane: Lane, ticket: usize) {
        let (actual, count, encoded) = self.active.remove(&lane).unwrap();
        assert_eq!(ticket, actual);
        *self.resident.get_mut(&lane).unwrap() -= count;
        self.encoded_resident -= encoded;
    }
    pub(super) fn drained(&self) -> bool {
        self.done
            && self.active.is_empty()
            && self.queues.values().all(VecDeque::is_empty)
            && self.resident.values().all(|count| *count == 0)
            && self.encoded_resident == 0
    }
}

pub(super) fn produce(
    schedule: Vec<Planned>,
    begin: Instant,
    mailbox: Arc<Mutex<Mailbox>>,
) -> JoinHandle<()> {
    assert!(schedule.len() <= 128 && schedule.iter().all(|row| row.emit_us <= 750000));
    thread::spawn(move || {
        for planned in schedule {
            let deadline = begin + Duration::from_micros(planned.emit_us);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                thread::sleep(remaining.min(Duration::from_millis(1)));
            }
            mailbox
                .lock()
                .unwrap()
                .offer(planned, u64::try_from(begin.elapsed().as_micros()).unwrap());
        }
        mailbox.lock().unwrap().done = true;
    })
}

pub(super) trait Driver {
    fn submit(&mut self, ticket: usize, batch: &Batch);
    fn advance(&mut self);
    fn finished(&mut self, ticket: usize, batch: &Batch) -> bool;
    fn heartbeat(&mut self);
    fn stop(self);
}

pub(super) fn run_case<D: Driver>(
    mut driver: D,
    context: &Context,
    trace: &Trace,
    wait_us: u64,
) -> Value {
    let schedule = plan(trace, wait_us);
    let planned_batches = schedule.len();
    let beginning = Instant::now();
    let origin = context.relative_us(beginning);
    context.event(json!({"kind":"offered_begin","origin_us":origin,"workload":trace.workload,"seed":trace.seed,
        "wait_us":wait_us,"grouping":"compatible","offered_records":trace.events.len(),"planned_batches":planned_batches}));
    let mailbox = Arc::new(Mutex::new(Mailbox::default()));
    let producer = produce(schedule, beginning, mailbox.clone());
    let mut active: BTreeMap<usize, (Pending, u64)> = BTreeMap::new();
    let mut published = BTreeSet::new();
    let mut recorded = 0;
    let mut next_tick = Duration::from_millis(100);
    loop {
        context.deadline();
        driver.advance();
        let finished = active
            .iter()
            .filter_map(|(ticket, (pending, _))| {
                driver
                    .finished(*ticket, &pending.planned.batch)
                    .then_some(*ticket)
            })
            .collect::<Vec<_>>();
        for ticket in finished {
            let (pending, started_us) = active.remove(&ticket).unwrap();
            let completed_us = u64::try_from(beginning.elapsed().as_micros()).unwrap();
            let ids = pending
                .planned
                .batch
                .operations
                .iter()
                .map(|operation| operation.id.clone())
                .collect::<Vec<_>>();
            for identity in &ids {
                assert!(published.insert(identity.clone()));
            }
            context.event(json!({"kind":"batch_completed","ticket":ticket,"ids":ids,"lane":[pending.planned.lane.0,pending.planned.lane.1],
                "planned_emit_us":pending.planned.emit_us,"enqueued_us":pending.enqueued_us,"started_us":started_us,"completed_us":completed_us,
                "offered_us":pending.planned.offered_us,"trigger":pending.planned.trigger,"origin_us":origin}));
            mailbox
                .lock()
                .unwrap()
                .complete(pending.planned.lane, ticket);
        }
        let (offered, ready, drained) = {
            let mut mailbox = mailbox.lock().unwrap();
            let offered = mailbox.offers[recorded..].to_vec();
            recorded = mailbox.offers.len();
            let ready = mailbox.take_available();
            let drained = mailbox.drained();
            (offered, ready, drained)
        };
        for offer in offered {
            context.event(json!({"kind":"offered_batch","ticket":offer.ticket,"ids":offer.ids,"planned_emit_us":offer.planned_us,
                "observed_emit_us":offer.observed_us,"admitted":offer.admitted,"reason":if offer.admitted {"accepted"} else {"resident_queue_budget"},"origin_us":origin}));
        }
        for pending in ready {
            let started_us = u64::try_from(beginning.elapsed().as_micros()).unwrap();
            let ticket = pending.planned.ticket;
            driver.submit(ticket, &pending.planned.batch);
            assert!(active.insert(ticket, (pending, started_us)).is_none());
            assert!(active.len() <= 4);
        }
        if beginning.elapsed() >= next_tick {
            driver.heartbeat();
            next_tick = beginning.elapsed() + Duration::from_millis(100);
        }
        if drained {
            assert!(active.is_empty());
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    producer.join().unwrap();
    driver.stop();
    let mailbox = mailbox.lock().unwrap();
    assert!(mailbox.drained() && recorded == planned_batches);
    let rejected = mailbox
        .offers
        .iter()
        .filter(|row| !row.admitted)
        .flat_map(|row| row.ids.iter().cloned())
        .collect::<BTreeSet<_>>();
    let original = trace
        .events
        .iter()
        .map(|event| event.id.clone())
        .collect::<BTreeSet<_>>();
    assert!(
        published.is_disjoint(&rejected)
            && published.union(&rejected).cloned().collect::<BTreeSet<_>>() == original
    );
    assert!(!published.is_empty());
    let result = json!({"workload":trace.workload,"seed":trace.seed,"wait_us":wait_us,"grouping":"compatible","offered":original.len(),
        "published":published,"rejected":rejected,"unknown":[],"peak_resident_records":mailbox.peak_records,"peak_encoded_pending_bytes":mailbox.peak_encoded_bytes,
        "planned_batches":planned_batches,"offered_origin_us":origin,"observed_duration_us":beginning.elapsed().as_micros()});
    context.event(json!({"kind":"offered_complete","result":result}));
    result
}

pub(super) fn controls() {
    let traces = fixtures();
    assert_eq!(
        traces.iter().map(|trace| trace.events.len()).sum::<usize>(),
        816
    );
    let mut case_count = 0;
    for trace in &traces {
        for wait in [50000, 250000] {
            let schedule = plan(trace, wait);
            case_count += 1;
            assert!(
                schedule
                    .iter()
                    .all(|row| row.offered_us.len() == row.batch.operations.len()
                        && matches!(row.trigger, "timer" | "bytes"))
            );
            let mut observed = BTreeSet::new();
            for row in schedule {
                for operation in row.batch.operations {
                    assert!(observed.insert(operation.id));
                }
            }
            assert_eq!(observed.len(), trace.events.len());
        }
    }
    assert_eq!(case_count, 18);
    // A boundary arrival belongs to the next batch: timer wins at exactly50ms.
    let mut boundary = traces[0].clone();
    boundary.events.truncate(2);
    for (i, event) in boundary.events.iter_mut().enumerate() {
        event.stream = 0;
        event.tenant = 0;
        event.key = 0;
        event.at_us = i as u64 * 50000;
    }
    let schedule = plan(&boundary, 50000);
    assert_eq!(schedule.len(), 2);
    assert_eq!(schedule[0].emit_us, 50000);
    assert_eq!(schedule[1].emit_us, 100000);
    let mut late = boundary.clone();
    late.events.truncate(1);
    late.events[0].at_us = 490000;
    assert_eq!(plan(&late, 250000)[0].emit_us, 740000);
    // A real independent producer can fill/reject while its consumer does no work.
    let base = plan(&boundary, 50000)[0].clone();
    let mut offers = vec![];
    for (ticket, count) in [16, 16, 1].into_iter().enumerate() {
        let mut row = base.clone();
        row.ticket = ticket;
        row.emit_us = ticket as u64 * 2000;
        row.batch.operations = (0..count)
            .map(|i| Operation::fixture(&format!("producer-{ticket}-{i}")))
            .collect();
        row.offered_us = vec![row.emit_us; count];
        offers.push(row);
    }
    let mailbox = Arc::new(Mutex::new(Mailbox::default()));
    let producer = produce(offers, Instant::now(), mailbox.clone());
    producer.join().unwrap();
    let mut mailbox = mailbox.lock().unwrap();
    assert_eq!(
        mailbox
            .offers
            .iter()
            .map(|offer| offer.admitted)
            .collect::<Vec<_>>(),
        vec![true, true, false]
    );
    assert_eq!(mailbox.peak_records, 32);
    assert!(!mailbox.drained());
    for expected in 0..2 {
        let ready = mailbox.take_available();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].planned.ticket, expected);
        assert!(ready[0].enqueued_us >= ready[0].planned.emit_us);
        assert!(
            mailbox.take_available().is_empty(),
            "One active batch per lane"
        );
        mailbox.complete(ready[0].planned.lane, ready[0].planned.ticket);
    }
    assert!(mailbox.drained());
    assert_eq!(
        mailbox
            .offers
            .iter()
            .map(|offer| offer.ids.len())
            .sum::<usize>(),
        33
    );
    assert_eq!(
        mailbox
            .offers
            .iter()
            .map(|offer| offer.ticket)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(
        mailbox
            .offers
            .iter()
            .all(|offer| offer.observed_us >= offer.planned_us)
    );
}
