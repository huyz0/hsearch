//! Range-native remote inputs (M02.03 prototype): bounded cold reads by
//! `read_at`, one GET per coalesced run, a bounded access trace, injected range
//! errors, and real Lucene files read and verified through a cache smaller than
//! the file.
#[path = "support/remote_fixture.rs"]
mod fixture;

use hsearch_engine::{RangeCache, RangeError, RangeFiles, RangeLimits, RemoteError, TraceKind};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Deterministic bytes, so a wrong offset is a wrong value.
fn bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 56) as u8
        })
        .collect()
}

#[derive(Default)]
struct Store {
    files: BTreeMap<String, Vec<u8>>,
    /// GETs issued, as (name, offset, len).
    gets: Mutex<Vec<(String, u64, usize)>>,
    /// Fail the GET with this zero-based index, once.
    fail_at: Mutex<Option<(usize, RemoteError)>>,
}
impl RangeFiles for &Store {
    fn read_at(&self, name: &str, offset: u64, buf: &mut [u8]) -> Result<(), RemoteError> {
        let mut gets = self.gets.lock().unwrap();
        let index = gets.len();
        gets.push((name.to_owned(), offset, buf.len()));
        drop(gets);
        let mut fail = self.fail_at.lock().unwrap();
        if let Some((at, error)) = *fail
            && at == index
        {
            *fail = None;
            return Err(error);
        }
        let file = self.files.get(name).ok_or(RemoteError::NotFound)?;
        let start = usize::try_from(offset).unwrap();
        let end = start + buf.len();
        buf.copy_from_slice(file.get(start..end).ok_or(RemoteError::Unavailable)?);
        Ok(())
    }
}
impl Store {
    fn with(files: &[(&str, usize)]) -> Self {
        let mut store = Store::default();
        for (i, (name, len)) in files.iter().enumerate() {
            store
                .files
                .insert((*name).to_owned(), bytes(*len, i as u64 + 7));
        }
        store
    }
    fn manifest(&self) -> Vec<(String, u64)> {
        self.files
            .iter()
            .map(|(n, b)| (n.clone(), b.len() as u64))
            .collect()
    }
    fn get_count(&self) -> usize {
        self.gets.lock().unwrap().len()
    }
}

fn limits(page: usize, cache: u64, run: usize) -> RangeLimits {
    RangeLimits {
        page_bytes: page,
        cache_bytes: cache,
        max_run_pages: run,
        trace_events: 64,
    }
}

#[test]
fn reads_equal_the_source_across_page_boundaries_and_the_tail() {
    let store = Store::with(&[("a", 1000), ("b", 64)]);
    let cache = RangeCache::new(&store, store.manifest(), limits(64, 64 * 6, 4)).unwrap();
    let input = cache.input("a").unwrap();
    assert_eq!(input.len(), 1000);
    let expected = &store.files["a"];
    let mut state = 11u64;
    for _ in 0..300 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let offset = (state >> 33) as usize % 1000;
        let len = ((state >> 20) as usize % 200).min(1000 - offset);
        let mut buf = vec![0xAA; len];
        input.read_at(offset as u64, &mut buf).unwrap();
        assert_eq!(buf, expected[offset..offset + len], "at {offset}+{len}");
    }
    // The last, short page and an exactly-at-EOF read.
    let mut tail = [0u8; 8];
    input.read_at(992, &mut tail).unwrap();
    assert_eq!(tail, expected[992..]);
    input.read_at(1000, &mut []).unwrap();
    // A file of exactly one page.
    let b = cache.input("b").unwrap();
    let mut whole = [0u8; 64];
    b.read_at(0, &mut whole).unwrap();
    assert_eq!(&whole[..], &store.files["b"][..]);
}

#[test]
fn working_set_larger_than_the_cache_stays_bounded_and_correct() {
    // 40 pages of data through a 5-page cache, scanned and then read at random.
    let store = Store::with(&[("big", 64 * 40)]);
    let cache = RangeCache::new(&store, store.manifest(), limits(64, 64 * 5, 2)).unwrap();
    let bound = cache.memory_bound_bytes();
    let input = cache.input("big").unwrap();
    let expected = &store.files["big"];
    let mut all = vec![0u8; expected.len()];
    input.read_at(0, &mut all).unwrap(); // one read 8x the cache
    assert_eq!(&all, expected);
    assert!(cache.stats().resident_bytes <= 64 * 5);
    let mut state = 3u64;
    for _ in 0..500 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let offset = (state >> 33) as usize % expected.len();
        let len = 1 + (state >> 20) as usize % 300;
        let len = len.min(expected.len() - offset);
        let mut buf = vec![0; len];
        input.read_at(offset as u64, &mut buf).unwrap();
        assert_eq!(buf, expected[offset..offset + len]);
        assert!(cache.stats().resident_bytes <= 64 * 5);
    }
    assert!(cache.stats().evictions > 0);
    assert_eq!(cache.memory_bound_bytes(), bound, "the bound never moves");
    assert!(bound <= 64 * 5 + 64 * 2 + 4096, "slab plus one run");
}

#[test]
fn one_cold_run_is_one_get_and_a_repeat_is_none() {
    let store = Store::with(&[("f", 64 * 10 + 5)]);
    let cache = RangeCache::new(&store, store.manifest(), limits(64, 64 * 16, 8)).unwrap();
    let input = cache.input("f").unwrap();
    let mut buf = vec![0u8; 64 * 6];
    input.read_at(10, &mut buf).unwrap(); // pages 0..=6: 7 pages, run cap 8
    assert_eq!(store.get_count(), 1, "adjacent pages coalesce");
    let (_, offset, len) = store.gets.lock().unwrap()[0].clone();
    assert_eq!((offset, len), (0, 64 * 7));
    input.read_at(10, &mut buf).unwrap();
    assert_eq!(store.get_count(), 1, "a hit costs no request");
    let s = cache.stats();
    assert_eq!((s.gets, s.get_bytes), (1, 64 * 7));
    assert_eq!((s.misses, s.hits), (7, 7));
    // A request beyond the run cap splits, never exceeds it.
    let mut wide = vec![0u8; 64 * 10 + 5];
    input.read_at(0, &mut wide).unwrap();
    assert_eq!(wide, store.files["f"]);
    for (_, _, len) in store.gets.lock().unwrap().iter() {
        assert!(*len <= 64 * 8);
    }
}

#[test]
fn a_gap_between_hits_is_fetched_as_its_own_run() {
    let store = Store::with(&[("f", 64 * 8)]);
    let cache = RangeCache::new(&store, store.manifest(), limits(64, 64 * 8, 8)).unwrap();
    let input = cache.input("f").unwrap();
    let mut page = [0u8; 64];
    input.read_at(64, &mut page).unwrap(); // page 1
    input.read_at(64 * 4, &mut page).unwrap(); // page 4
    let before = store.get_count();
    let mut all = vec![0u8; 64 * 8];
    input.read_at(0, &mut all).unwrap();
    assert_eq!(all, store.files["f"]);
    // Missing: page 0, pages 2-3, pages 5-7. Three runs, none re-fetching a hit.
    let gets = store.gets.lock().unwrap()[before..].to_vec();
    let spans: Vec<(u64, usize)> = gets.iter().map(|(_, o, l)| (*o, *l)).collect();
    assert_eq!(spans, vec![(0, 64), (128, 128), (320, 192)]);
}

#[test]
fn bad_ranges_and_names_fail_before_any_request() {
    let store = Store::with(&[("f", 100)]);
    let cache = RangeCache::new(&store, store.manifest(), limits(64, 256, 2)).unwrap();
    assert_eq!(cache.input("nope").err(), Some(RangeError::NotFound));
    let input = cache.input("f").unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(input.read_at(93, &mut buf), Err(RangeError::OutOfRange));
    assert_eq!(
        input.read_at(u64::MAX, &mut buf),
        Err(RangeError::OutOfRange)
    );
    assert_eq!(input.read_at(101, &mut []), Err(RangeError::OutOfRange));
    assert_eq!(input.prefetch(90, 11), Err(RangeError::OutOfRange));
    assert_eq!(store.get_count(), 0);
    assert!(RangeCache::new(&store, store.manifest(), limits(0, 256, 2)).is_err());
    assert!(RangeCache::new(&store, store.manifest(), limits(64, 63, 2)).is_err());
    assert!(RangeCache::new(&store, store.manifest(), limits(64, 256, 0)).is_err());
    let dup = vec![("f".to_owned(), 1), ("f".to_owned(), 2)];
    assert!(RangeCache::new(&store, dup, limits(64, 256, 2)).is_err());
    let nested = vec![("a/b".to_owned(), 1)];
    assert!(RangeCache::new(&store, nested, limits(64, 256, 2)).is_err());
}

#[test]
fn an_injected_range_error_is_an_error_never_partial_bytes_and_recovers() {
    let store = Store::with(&[("f", 64 * 12)]);
    let cache = RangeCache::new(&store, store.manifest(), limits(64, 64 * 12, 3)).unwrap();
    let input = cache.input("f").unwrap();
    let expected = &store.files["f"];

    // Second of four runs fails: the read fails, nothing claims success.
    *store.fail_at.lock().unwrap() = Some((1, RemoteError::Unavailable));
    let mut buf = vec![0u8; 64 * 12];
    assert_eq!(input.read_at(0, &mut buf), Err(RangeError::Unavailable));
    let s = cache.stats();
    assert_eq!(s.errors, 1);
    assert_eq!(s.gets, 1, "only the run that succeeded counts as a GET");
    assert_eq!(s.resident_bytes, 64 * 3, "the failed run cached nothing");

    // Retry: the first run is a hit, the failed one is fetched again.
    let before = store.get_count();
    input.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf, expected);
    assert_eq!(store.get_count() - before, 3, "runs 2-4 only");

    // NotFound from the store maps to NotFound, and is counted.
    *store.fail_at.lock().unwrap() = Some((store.get_count(), RemoteError::NotFound));
    let cold = RangeCache::new(&store, store.manifest(), limits(64, 64 * 12, 3)).unwrap();
    let mut one = [0u8; 4];
    assert_eq!(
        cold.input("f").unwrap().read_at(0, &mut one),
        Err(RangeError::NotFound)
    );
    assert_eq!(cold.stats().errors, 1);
}

#[test]
fn a_failure_on_every_position_leaves_the_cache_consistent() {
    // Fail each GET index in turn during a mixed workload: whatever failed, the
    // next full read is byte-exact and the cache respects its bound.
    for fail in 0..8 {
        let store = Store::with(&[("f", 64 * 9 + 3)]);
        let cache = RangeCache::new(&store, store.manifest(), limits(64, 64 * 4, 2)).unwrap();
        let input = cache.input("f").unwrap();
        *store.fail_at.lock().unwrap() = Some((fail, RemoteError::Unavailable));
        let mut buf = vec![0u8; 64 * 9 + 3];
        let mut failed = false;
        for start in [0usize, 200, 64 * 5] {
            let len = buf.len() - start;
            failed |= input.read_at(start as u64, &mut buf[..len]).is_err();
            assert!(cache.stats().resident_bytes <= 64 * 4);
        }
        assert!(failed || fail >= store.get_count());
        input.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, store.files["f"], "fail at {fail}");
    }
}

#[test]
fn slices_are_views_with_their_own_bounds() {
    let store = Store::with(&[("f", 500)]);
    let cache = RangeCache::new(&store, store.manifest(), limits(64, 64 * 8, 4)).unwrap();
    let input = cache.input("f").unwrap();
    let part = input.slice(100, 200).unwrap();
    assert_eq!(part.len(), 200);
    let inner = part.slice(50, 50).unwrap();
    let mut buf = [0u8; 50];
    inner.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, store.files["f"][150..200]);
    assert_eq!(inner.read_at(1, &mut buf), Err(RangeError::OutOfRange));
    assert_eq!(part.slice(150, 51).err(), Some(RangeError::OutOfRange));
    assert_eq!(input.slice(u64::MAX, 2).err(), Some(RangeError::OutOfRange));
    assert!(input.slice(500, 0).is_ok());
}

#[test]
fn prefetch_announces_a_range_in_one_batch_and_reports_errors() {
    let store = Store::with(&[("f", 64 * 20)]);
    let cache = RangeCache::new(&store, store.manifest(), limits(64, 64 * 8, 8)).unwrap();
    let input = cache.input("f").unwrap();
    input.prefetch(64 * 2, 64 * 6).unwrap();
    assert_eq!(store.get_count(), 1);
    let mut buf = vec![0u8; 64 * 6];
    input.read_at(64 * 2, &mut buf).unwrap();
    assert_eq!(store.get_count(), 1, "the reads after a prefetch are hits");
    assert_eq!(buf, store.files["f"][128..512]);
    // More than the cache can hold at once is a refusal, not an eviction storm.
    assert_eq!(input.prefetch(0, 64 * 9), Err(RangeError::OutOfRange));
    // A failed prefetch reaches the caller as the error, not at a later read.
    *store.fail_at.lock().unwrap() = Some((store.get_count(), RemoteError::Unavailable));
    assert_eq!(input.prefetch(64 * 12, 64), Err(RangeError::Unavailable));
}

#[test]
fn the_trace_records_reads_and_gets_and_is_bounded() {
    let store = Store::with(&[("f", 64 * 4), ("g", 64)]);
    let mut config = limits(64, 64 * 4, 4);
    config.trace_events = 3;
    let cache = RangeCache::new(&store, store.manifest(), config).unwrap();
    let capacity = cache.trace_capacity_bytes();
    let f = cache.input("f").unwrap();
    let mut buf = [0u8; 16];
    f.read_at(70, &mut buf).unwrap();
    let trace = cache.trace();
    assert_eq!(trace.len(), 2);
    assert_eq!(
        (
            trace[0].kind,
            trace[0].file.as_str(),
            trace[0].offset,
            trace[0].len
        ),
        (TraceKind::Read, "f", 70, 16)
    );
    assert_eq!(
        (
            trace[1].kind,
            trace[1].file.as_str(),
            trace[1].offset,
            trace[1].len
        ),
        (TraceKind::Get, "f", 64, 64)
    );
    // Hits record only the read; the failed GET is recorded as failed.
    f.read_at(70, &mut buf).unwrap();
    *store.fail_at.lock().unwrap() = Some((store.get_count(), RemoteError::Unavailable));
    let g = cache.input("g").unwrap();
    assert!(g.read_at(0, &mut buf[..8]).is_err());
    let (kept, dropped) = (cache.trace(), cache.trace_dropped());
    assert_eq!(kept.len(), 3, "bounded at trace_events");
    assert_eq!(dropped, 2, "the rest are counted, not stored");
    assert_eq!(cache.trace_capacity_bytes(), capacity, "never grows");

    let mut off = limits(64, 64 * 4, 4);
    off.trace_events = 0;
    let quiet = RangeCache::new(&store, store.manifest(), off).unwrap();
    quiet.input("f").unwrap().read_at(0, &mut buf).unwrap();
    assert!(quiet.trace().is_empty());
    assert_eq!(quiet.trace_dropped(), 0);
    let failed = cache
        .trace()
        .iter()
        .filter(|e| e.kind == TraceKind::GetFailed)
        .count();
    assert!(failed <= 1);
}

/// CRC-32 (IEEE) as Lucene's `CodecUtil` footer uses it.
fn crc32(state: u32, data: &[u8]) -> u32 {
    let mut crc = !state;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// Real Lucene files, read only through ranges: the codec header and footer of
/// every file are two small reads, and the checksum footer verifies the whole
/// file by streaming it through a cache a fraction of its size.
#[test]
fn real_segment_files_verify_through_a_cache_smaller_than_the_largest_file() {
    let (_scratch, files) = fixture::write_documents();
    let mut store = Store::default();
    for (name, bytes) in &files {
        store.files.insert(name.clone(), bytes.clone());
    }
    let largest = files.iter().map(|(_, b)| b.len()).max().unwrap();
    eprintln!("largest {largest} of {} files", files.len());
    assert!(largest > 256, "the fixture must have a file worth paging");
    let page = 32usize;
    let cache_bytes = (largest as u64 / 4).max(page as u64 * 2);
    assert!(cache_bytes < largest as u64);
    let cache = RangeCache::new(&store, store.manifest(), limits(page, cache_bytes, 4)).unwrap();

    let mut verified = 0;
    let mut chunk = [0u8; 100]; // deliberately not a page multiple
    for (name, source) in &files {
        let input = cache.input(name).unwrap();
        let len = input.len();
        assert_eq!(len, source.len() as u64);
        if name == "segments.gen" || len < 16 {
            continue;
        }
        // Header magic (0x3fd76c17, big-endian) and footer: magic, algorithm 0, crc.
        let mut head = [0u8; 4];
        input.read_at(0, &mut head).unwrap();
        assert_eq!(u32::from_be_bytes(head), 0x3fd7_6c17, "{name} header");
        let mut footer = [0u8; 16];
        input.read_at(len - 16, &mut footer).unwrap();
        assert_eq!(&footer[..4], &0xc028_93e8u32.to_be_bytes(), "{name} footer");
        let stored = u64::from_be_bytes(footer[8..].try_into().unwrap());
        // Stream everything before the CRC field.
        let (mut crc, mut at) = (0u32, 0u64);
        let end = len - 8;
        while at < end {
            let n = usize::try_from((end - at).min(chunk.len() as u64)).unwrap();
            input.read_at(at, &mut chunk[..n]).unwrap();
            crc = crc32(crc, &chunk[..n]);
            at += n as u64;
        }
        assert_eq!(u64::from(crc), stored, "{name} checksum");
        verified += 1;
    }
    assert!(verified >= 10, "verified {verified} files");
    let s = cache.stats();
    assert!(s.resident_bytes <= cache_bytes);
    assert!(s.evictions > 0, "the working set exceeded the cache");
    assert!(s.get_bytes >= largest as u64);
}
