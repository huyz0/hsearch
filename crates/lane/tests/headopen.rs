//! M03.08: a lane opened from its structural head instead of the reclaim marker.
mod support;

use std::sync::Arc;

use hsearch_format::Durability;
use hsearch_lane::counting::Requests;
use hsearch_lane::{
    Advance, Change, Error, LaneId, LaneWriter, advance_head, block_on, create_head, create_lane,
    read_head, shared_group,
};
use support::*;

use hsearch_object::{
    ByteRange, MetadataStore, Mutation, ObjectBackend, ObjectInfo, ObjectKey, Operation, Payload,
    RequestContext, VersionToken, WriteReceipt,
};
use std::sync::Mutex;

type Trip = Mutex<Option<(String, Box<dyn FnOnce() + Send>)>>;
type Inner = hsearch_lane::counting::CountingStore<hsearch_object::memory::MemoryStore>;

/// Records the key of every request, so a test can say which objects an operation touched.
struct Store {
    inner: Arc<Inner>,
    keys: Mutex<Vec<String>>,
    /// Runs once, just before the second HEAD of `trip_key` (the re-read of the cover after replay).
    trip: Trip,
    trip_heads: std::sync::atomic::AtomicUsize,
}

impl Store {
    fn note(&self, key: &ObjectKey) {
        self.keys
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(key.as_bytes()).into_owned());
    }
    fn touched(&self, suffix: &str) -> bool {
        self.keys
            .lock()
            .unwrap()
            .iter()
            .any(|k| k.ends_with(suffix))
    }
    fn forget(&self) {
        self.keys.lock().unwrap().clear();
    }
    fn requests(&self) -> Requests {
        self.inner.requests()
    }
}

impl ObjectBackend for Store {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.note(k);
        let armed =
            matches!(&*self.trip.lock().unwrap(), Some((t, _)) if t.as_bytes() == k.as_bytes());
        if armed
            && self
                .trip_heads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 1
        {
            let taken = self.trip.lock().unwrap().take();
            if let Some((_, f)) = taken {
                f();
            }
        }
        self.inner.head(c, k)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Mutation) -> Operation<'a, WriteReceipt> {
        self.note(&r.key);
        self.inner.create(c, r)
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        self.note(k);
        self.inner.read_range(c, k, v, r)
    }
}

impl MetadataStore for Store {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.note(&r.key);
        self.inner.compare_exchange(c, r)
    }
    fn remove_version<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
    ) -> Operation<'a, ()> {
        self.note(k);
        self.inner.remove_version(c, k, v)
    }
    fn resolve<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.inner.resolve(c, r)
    }
}

fn world() -> (Arc<Store>, LaneId) {
    let store = Arc::new(Store {
        inner: counted(),
        keys: Mutex::new(Vec::new()),
        trip: Mutex::new(None),
        trip_heads: std::sync::atomic::AtomicUsize::new(0),
    });
    let (domain, group) = shared_group(&policy(), 0);
    let lane = LaneId::new(domain);
    block_on(create_lane(
        &*store,
        &layout(),
        &lane,
        &policy(),
        group,
        1000,
        &[id(1)],
    ))
    .unwrap();
    block_on(create_head(&*store, &layout(), &lane, Durability::Object)).unwrap();
    (store, lane)
}

fn open(
    store: &Arc<Store>,
    lane: LaneId,
) -> Result<(LaneWriter<Store>, hsearch_lane::OpenReport), Error> {
    block_on(LaneWriter::open_with_head(
        Arc::clone(store),
        layout(),
        lane,
        |_| Ok(()),
    ))
}

fn checkpoint(store: &Arc<Store>, lane: &LaneId, cover: u64) {
    let (h, v) = block_on(read_head(&**store, &layout(), lane)).unwrap();
    let change = Change::Checkpoint {
        cover,
        root: [9; 16],
    };
    match block_on(advance_head(&**store, &layout(), lane, &h, &v, change)).unwrap() {
        Advance::Done(..) => {}
        Advance::Lost => panic!("lost"),
    }
}

#[test]
fn an_open_records_its_epoch_and_fence_in_the_head_and_appends_do_not_touch_it() {
    let (store, lane) = world();
    let (mut writer, report) = open(&store, lane).unwrap();
    assert_eq!((report.head_epoch, report.fence_ordinal), (1, 0));
    let (head, _) = block_on(read_head(&*store, &layout(), &lane)).unwrap();
    assert_eq!(head.writer_epoch, 1);
    assert_eq!(head.inventory.len(), 1);
    assert_eq!(head.inventory[0].fence, 0);
    assert_eq!(head.tail.ordinal, 1, "a successor looks above the fence");
    assert_eq!(head.generation, report.head_generation);
    for n in 0..5u64 {
        block_on(writer.append(&[put(id(1), n + 1, b"x")])).unwrap();
    }
    let (after, _) = block_on(read_head(&*store, &layout(), &lane)).unwrap();
    assert_eq!(
        after.generation, head.generation,
        "no head write per append"
    );
}

#[test]
fn a_second_open_is_a_second_epoch_above_the_first_and_fences_the_first_writer() {
    let (store, lane) = world();
    let (mut first, _) = open(&store, lane).unwrap();
    block_on(first.append(&[put(id(1), 1, b"a")])).unwrap();
    let (mut second, report) = open(&store, lane).unwrap();
    assert_eq!((report.head_epoch, report.fence_ordinal), (2, 2));
    assert_eq!(
        block_on(first.append(&[put(id(1), 2, b"b")])),
        Err(Error::Fenced)
    );
    block_on(second.append(&[put(id(1), 3, b"c")])).unwrap();
    let (head, _) = block_on(read_head(&*store, &layout(), &lane)).unwrap();
    let fences: Vec<u64> = head.inventory.iter().map(|e| e.fence).collect();
    assert_eq!(fences, [0, 2], "epochs ascend and each names its fence");
}

#[test]
fn replay_starts_at_the_heads_cover_and_the_marker_is_never_read() {
    let (store, lane) = world();
    let (mut writer, _) = open(&store, lane).unwrap();
    for n in 1..=6u64 {
        block_on(writer.append(&[put(id(1), n, b"x")])).unwrap();
    }
    // A checkpoint raises the cover in the head; only then are slots below it removed.
    checkpoint(&store, &lane, 4);
    let removed = block_on(writer.reclaim_to_checkpoint()).unwrap();
    assert_eq!(
        (removed.removed, removed.fences_kept),
        (3, 1),
        "slots 1-3 go, the fence at 0 stays"
    );
    drop(writer);
    let before = store.requests().puts();
    store.forget();
    let (_, report) = open(&store, lane).unwrap();
    let puts = store.requests().puts() - before;
    assert_eq!(report.from, 4);
    assert_eq!(
        report.slots_replayed, 3,
        "slots 4, 5, 6 and the first fence are above the cover"
    );
    // No object whose key ends in COVER was ever touched: the open never asked the marker.
    assert!(
        !store.touched("COVER"),
        "the marker is not read by a head-driven open"
    );
    assert_eq!(puts, 2, "the fence and one head transition");
}

#[test]
fn a_cover_above_the_end_found_at_the_start_is_a_legitimate_end() {
    let (store, lane) = world();
    let (mut writer, _) = open(&store, lane).unwrap();
    for n in 1..=3u64 {
        block_on(writer.append(&[put(id(1), n, b"x")])).unwrap();
    }
    drop(writer);
    let (h, v) = block_on(read_head(&*store, &layout(), &lane)).unwrap();
    block_on(advance_head(
        &*store,
        &layout(),
        &lane,
        &h,
        &v,
        Change::Checkpoint {
            cover: 10,
            root: [9; 16],
        },
    ))
    .unwrap();
    // Replay starts at the cover and finds it absent: an end at or above the cover is real.
    assert!(open(&store, lane).is_ok());
}

#[test]
fn a_checkpoint_landing_during_replay_that_passes_the_end_found_is_refused_without_a_fence() {
    let (store, lane) = world();
    let (mut writer, _) = open(&store, lane).unwrap();
    block_on(writer.append(&[put(id(1), 1, b"x")])).unwrap();
    drop(writer);
    let inner = store.inner.clone();
    let key = layout().structural_head(&lane.domain, lane.incarnation);
    *store.trip.lock().unwrap() = Some((
        key,
        Box::new(move || {
            let (h, v) = block_on(read_head(&*inner, &layout(), &lane)).unwrap();
            block_on(advance_head(
                &*inner,
                &layout(),
                &lane,
                &h,
                &v,
                Change::Checkpoint {
                    cover: 50,
                    root: [9; 16],
                },
            ))
            .unwrap();
        }),
    ));
    store.forget();
    let creates = store.requests().create;
    let result = open(&store, lane);
    assert!(matches!(result, Err(Error::BadCover)), "{:?}", result.err());
    assert_eq!(
        store.requests().create,
        creates,
        "no fence slot was created"
    );
    let (head, _) = block_on(read_head(&*store, &layout(), &lane)).unwrap();
    assert_eq!(head.inventory.len(), 1, "no epoch was established");
}

#[test]
fn an_open_without_a_head_is_refused() {
    let (domain, group) = shared_group(&policy(), 0);
    let lane = LaneId::new(domain);
    let store = Arc::new(Store {
        inner: counted(),
        keys: Mutex::new(Vec::new()),
        trip: Mutex::new(None),
        trip_heads: std::sync::atomic::AtomicUsize::new(0),
    });
    block_on(create_lane(
        &*store,
        &layout(),
        &lane,
        &policy(),
        group,
        1000,
        &[id(1)],
    ))
    .unwrap();
    assert_eq!(open(&store, lane).err(), Some(Error::NoHead));
}

#[test]
fn a_lane_id_older_than_the_heads_activation_has_no_authority() {
    let (store, lane) = world();
    // The head moves to activation 2 (a promoted lane's new activation).
    let (h, v) = block_on(read_head(&*store, &layout(), &lane)).unwrap();
    block_on(advance_head(
        &*store,
        &layout(),
        &lane,
        &h,
        &v,
        Change::Establish {
            activation: 2,
            lane: 0,
            fence: 0,
        },
    ))
    .unwrap();
    assert_eq!(open(&store, lane).err(), Some(Error::Fenced));
}
