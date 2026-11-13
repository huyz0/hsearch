//! Range-native remote inputs (M02.03 prototype): `read_at` over a bounded
//! page cache, one request per coalesced run of missing pages, and a bounded
//! trace of what was asked for and what was fetched.
//!
//! [`RangeFiles`] is the seam, like [`crate::RemoteFiles`] but for a byte range
//! instead of a whole file. A [`RangeCache`] knows each file's length from the
//! manifest the caller passes, so a read costs no HEAD and nothing here lists
//! the store.
//!
//! Memory is fixed at construction: one slab of `cache_bytes`, one scratch of
//! `max_run_pages` pages, per-slot bookkeeping and the trace ring. Nothing
//! allocates afterwards, whatever the working set. A source writes a run into
//! the scratch, which is then copied into the slab and the caller's buffer; that
//! one extra copy is what lets a run span slots that are not adjacent.
//!
//! Eviction is CLOCK with a probation start: a page is inserted unreferenced and
//! becomes referenced when read again, so one pass over a large file does not push
//! out pages that are being re-read.
//!
//! This is a prototype of what an upstream demand-filled input needs (see
//! `docs/plan/evidence/m02-03-range-inputs.md`): the pinned `lucene_store::Input`
//! is a byte slice, so Lucene itself cannot be handed a [`RangeInput`] until the
//! upstream `ExternalBytes::will_need` input is adopted. v0 limits: one lock
//! covers the cache and is held across a fetch, so concurrent misses queue.
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use crate::RemoteError;

/// Byte-range source for immutable files.
pub trait RangeFiles: Send + Sync {
    /// Fill `buf` with exactly the bytes `offset..offset + buf.len()` of `name`,
    /// or fail. A short or partial read is a failure, never a success.
    fn read_at(&self, name: &str, offset: u64, buf: &mut [u8]) -> Result<(), RemoteError>;
}

/// Why a range read failed. No provider text, request or payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeError {
    /// The file is not in the manifest, or the store does not have it.
    NotFound,
    /// The store could not serve the range now; retrying later may succeed.
    Unavailable,
    /// The range lies outside the file, or is larger than the cache can hold at once.
    OutOfRange,
    /// The limits or the manifest are invalid.
    Invalid,
    /// The cache lock was poisoned by a panic.
    Poisoned,
}

impl From<RemoteError> for RangeError {
    fn from(error: RemoteError) -> Self {
        match error {
            RemoteError::NotFound => RangeError::NotFound,
            RemoteError::Unavailable => RangeError::Unavailable,
        }
    }
}

/// Fixed sizes of a [`RangeCache`].
#[derive(Clone, Copy, Debug)]
pub struct RangeLimits {
    /// Bytes per cached page, and the unit of every request: a request is a whole
    /// number of pages except at a file's end.
    pub page_bytes: usize,
    /// Bytes of page data held at once; at least one page.
    pub cache_bytes: u64,
    /// Most adjacent pages one request may cover; at least one.
    pub max_run_pages: usize,
    /// Trace events kept; later ones are counted and dropped. Zero disables it.
    pub trace_events: usize,
}

/// What an access was.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraceKind {
    /// A caller's `read_at` or `prefetch`, as asked.
    Read,
    /// A request to the store that succeeded.
    Get,
    /// A request to the store that failed.
    GetFailed,
}

/// One traced access.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Access {
    /// What happened.
    pub kind: TraceKind,
    /// The file.
    pub file: String,
    /// First byte, within the file.
    pub offset: u64,
    /// Bytes.
    pub len: u64,
}

/// Counters since the cache was created.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RangeStats {
    /// Caller reads served or attempted.
    pub reads: u64,
    /// Pages found in the cache.
    pub hits: u64,
    /// Pages fetched.
    pub misses: u64,
    /// Successful store requests.
    pub gets: u64,
    /// Bytes those requests returned.
    pub get_bytes: u64,
    /// Failed store requests.
    pub errors: u64,
    /// Pages dropped to make room.
    pub evictions: u64,
    /// Bytes of page data held now.
    pub resident_bytes: u64,
}

#[derive(Clone, Copy)]
struct Event {
    kind: TraceKind,
    file: u32,
    offset: u64,
    len: u64,
}

const NO_SLOT: u32 = u32::MAX;

struct State {
    slab: Vec<u8>,
    scratch: Vec<u8>,
    /// Per slot: the page it holds as (file, page), and how many bytes of it.
    key: Vec<Option<(u32, u64)>>,
    len: Vec<u32>,
    referenced: Vec<bool>,
    map: HashMap<(u32, u64), u32>,
    free: Vec<u32>,
    hand: usize,
    /// Per page of the window being served: was it found in the cache.
    window_hit: Vec<bool>,
    stats: RangeStats,
    trace: Vec<Event>,
    trace_cap: usize,
    trace_dropped: u64,
}

/// A bounded page cache over a [`RangeFiles`] source.
pub struct RangeCache<S: RangeFiles> {
    source: S,
    page: usize,
    pages: usize,
    run: usize,
    files: Vec<(String, u64)>,
    ids: BTreeMap<String, u32>,
    state: Mutex<State>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

impl<S: RangeFiles> RangeCache<S> {
    /// Create the cache over `source`. `files` is the manifest: each file's name
    /// and exact length. Fails on an invalid limit, a duplicate or non-component
    /// name, or more files than a `u32` can number.
    pub fn new(
        source: S,
        files: impl IntoIterator<Item = (String, u64)>,
        limits: RangeLimits,
    ) -> Result<Self, RangeError> {
        let pages = usize::try_from(limits.cache_bytes / limits.page_bytes.max(1) as u64)
            .map_err(|_| RangeError::Invalid)?;
        if limits.page_bytes == 0
            || limits.page_bytes > u32::MAX as usize
            || pages == 0
            || pages > u32::MAX as usize - 1
            || limits.max_run_pages == 0
        {
            return Err(RangeError::Invalid);
        }
        let run = limits.max_run_pages.min(pages);
        let files: Vec<(String, u64)> = files.into_iter().collect();
        let mut ids = BTreeMap::new();
        for (id, (name, _)) in files.iter().enumerate() {
            let id = u32::try_from(id).map_err(|_| RangeError::Invalid)?;
            if !valid_name(name) || ids.insert(name.clone(), id).is_some() {
                return Err(RangeError::Invalid);
            }
        }
        let slab_bytes = pages
            .checked_mul(limits.page_bytes)
            .ok_or(RangeError::Invalid)?;
        let scratch_bytes = run
            .checked_mul(limits.page_bytes)
            .ok_or(RangeError::Invalid)?;
        let state = State {
            // Zeroed allocations are lazily committed: untouched pages cost nothing.
            slab: vec![0; slab_bytes],
            scratch: vec![0; scratch_bytes],
            key: vec![None; pages],
            len: vec![0; pages],
            referenced: vec![false; pages],
            map: HashMap::with_capacity(pages),
            free: (0..pages as u32).rev().collect(),
            hand: 0,
            window_hit: vec![false; run],
            stats: RangeStats::default(),
            trace: Vec::with_capacity(limits.trace_events),
            trace_cap: limits.trace_events,
            trace_dropped: 0,
        };
        Ok(Self {
            source,
            page: limits.page_bytes,
            pages,
            run,
            files,
            ids,
            state: Mutex::new(state),
        })
    }

    /// A handle on the whole of `name`.
    pub fn input(&self, name: &str) -> Result<RangeInput<'_, S>, RangeError> {
        let file = *self.ids.get(name).ok_or(RangeError::NotFound)?;
        Ok(RangeInput {
            cache: self,
            file,
            start: 0,
            len: self.files[file as usize].1,
        })
    }

    /// Counters now.
    pub fn stats(&self) -> RangeStats {
        self.state
            .lock()
            .map_or_else(|poisoned| poisoned.into_inner().stats, |state| state.stats)
    }

    /// The most bytes the cache ever holds: the slab, the scratch, per-slot
    /// bookkeeping and the trace ring. Fixed at construction.
    #[must_use]
    pub fn memory_bound_bytes(&self) -> usize {
        let slot = size_of::<Option<(u32, u64)>>() + size_of::<u32>() + 1 + 2 * size_of::<u32>();
        self.pages * (self.page + slot) + self.run * (self.page + 1) + self.trace_capacity_bytes()
    }

    /// Bytes reserved for the trace ring.
    #[must_use]
    pub fn trace_capacity_bytes(&self) -> usize {
        self.state
            .lock()
            .map_or(0, |state| state.trace.capacity() * size_of::<Event>())
    }

    /// The kept trace, oldest first.
    pub fn trace(&self) -> Vec<Access> {
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        state
            .trace
            .iter()
            .map(|event| Access {
                kind: event.kind,
                file: self.files[event.file as usize].0.clone(),
                offset: event.offset,
                len: event.len,
            })
            .collect()
    }

    /// Events that did not fit in the trace.
    pub fn trace_dropped(&self) -> u64 {
        self.state.lock().map_or(0, |state| state.trace_dropped)
    }

    fn record(state: &mut State, kind: TraceKind, file: u32, offset: u64, len: u64) {
        if state.trace_cap == 0 {
            return;
        }
        if state.trace.len() < state.trace_cap {
            state.trace.push(Event {
                kind,
                file,
                offset,
                len,
            });
        } else {
            state.trace_dropped += 1;
        }
    }

    fn page_len(&self, file_len: u64, page: u64) -> usize {
        let start = page * self.page as u64;
        usize::try_from((file_len - start).min(self.page as u64)).unwrap_or(self.page)
    }

    /// Take a slot for a new page, evicting by CLOCK when none is free.
    fn take_slot(&self, state: &mut State) -> usize {
        if let Some(slot) = state.free.pop() {
            return slot as usize;
        }
        loop {
            let slot = state.hand;
            state.hand = (state.hand + 1) % self.pages;
            if state.referenced[slot] {
                state.referenced[slot] = false;
                continue;
            }
            if let Some(old) = state.key[slot].take() {
                state.map.remove(&old);
                state.stats.resident_bytes -= u64::from(state.len[slot]);
                state.stats.evictions += 1;
            }
            return slot;
        }
    }

    /// Serve `range` of `file` into `out` (when given), fetching what is missing.
    /// Works in windows of at most `run` pages so memory stays fixed however
    /// large the range is.
    fn fill(
        &self,
        file: u32,
        offset: u64,
        len: u64,
        mut out: Option<&mut [u8]>,
    ) -> Result<(), RangeError> {
        let file_len = self.files[file as usize].1;
        let end = offset.checked_add(len).ok_or(RangeError::OutOfRange)?;
        if end > file_len {
            return Err(RangeError::OutOfRange);
        }
        let page = self.page as u64;
        let mut state = self.state.lock().map_err(|_| RangeError::Poisoned)?;
        let state = &mut *state;
        state.stats.reads += 1;
        Self::record(state, TraceKind::Read, file, offset, len);
        if len == 0 {
            return Ok(());
        }
        let last_page = (end - 1) / page;
        let mut first = offset / page;
        while first <= last_page {
            let window_end = (first + self.run as u64).min(last_page + 1);
            let count = (window_end - first) as usize;
            // Pass 1: copy every hit out now, before a miss can evict it.
            for i in 0..count {
                let index = first + i as u64;
                let slot = state.map.get(&(file, index)).copied().unwrap_or(NO_SLOT);
                state.window_hit[i] = slot != NO_SLOT;
                if slot != NO_SLOT {
                    let slot = slot as usize;
                    state.stats.hits += 1;
                    state.referenced[slot] = true;
                    if let Some(out) = out.as_deref_mut() {
                        let data = &state.slab[slot * self.page..][..state.len[slot] as usize];
                        copy_overlap(data, index * page, offset, end, out);
                    }
                }
            }
            // Pass 2: fetch each run of adjacent misses with one request.
            let mut i = 0;
            while i < count {
                if state.window_hit[i] {
                    i += 1;
                    continue;
                }
                let mut j = i;
                while j < count && !state.window_hit[j] {
                    j += 1;
                }
                self.fetch_run(
                    state,
                    file,
                    file_len,
                    first + i as u64,
                    j - i,
                    offset,
                    end,
                    &mut out,
                )?;
                i = j;
            }
            first = window_end;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn fetch_run(
        &self,
        state: &mut State,
        file: u32,
        file_len: u64,
        first_page: u64,
        pages: usize,
        offset: u64,
        end: u64,
        out: &mut Option<&mut [u8]>,
    ) -> Result<(), RangeError> {
        let page = self.page as u64;
        let from = first_page * page;
        let to = ((first_page + pages as u64) * page).min(file_len);
        let bytes = usize::try_from(to - from).map_err(|_| RangeError::Invalid)?;
        let name = &self.files[file as usize].0;
        if let Err(error) = self.source.read_at(name, from, &mut state.scratch[..bytes]) {
            state.stats.errors += 1;
            Self::record(state, TraceKind::GetFailed, file, from, bytes as u64);
            return Err(error.into());
        }
        state.stats.gets += 1;
        state.stats.get_bytes += bytes as u64;
        state.stats.misses += pages as u64;
        Self::record(state, TraceKind::Get, file, from, bytes as u64);
        for k in 0..pages {
            let index = first_page + k as u64;
            let plen = self.page_len(file_len, index);
            let src = k * self.page;
            if let Some(out) = out.as_deref_mut() {
                copy_overlap(
                    &state.scratch[src..src + plen],
                    index * page,
                    offset,
                    end,
                    out,
                );
            }
            let slot = self.take_slot(state);
            state.slab[slot * self.page..][..plen].copy_from_slice(&state.scratch[src..src + plen]);
            state.key[slot] = Some((file, index));
            state.len[slot] = plen as u32;
            state.referenced[slot] = false;
            state.map.insert((file, index), slot as u32);
            state.stats.resident_bytes += plen as u64;
        }
        Ok(())
    }
}

/// Copy the part of `data` (which starts at file byte `data_at`) that lies in
/// `offset..end` into `out`, which starts at file byte `offset`.
fn copy_overlap(data: &[u8], data_at: u64, offset: u64, end: u64, out: &mut [u8]) {
    let lo = data_at.max(offset);
    let hi = (data_at + data.len() as u64).min(end);
    if lo < hi {
        out[(lo - offset) as usize..(hi - offset) as usize]
            .copy_from_slice(&data[(lo - data_at) as usize..(hi - data_at) as usize]);
    }
}

/// A view of a byte range of one remote file, read by position.
pub struct RangeInput<'a, S: RangeFiles> {
    cache: &'a RangeCache<S>,
    file: u32,
    start: u64,
    len: u64,
}

impl<S: RangeFiles> Clone for RangeInput<'_, S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S: RangeFiles> Copy for RangeInput<'_, S> {}

impl<S: RangeFiles> RangeInput<'_, S> {
    /// Bytes in this view.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// True for an empty view.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Fill all of `buf` from `offset` within this view, or fail without having
    /// claimed success. Bytes of `buf` after an error are unspecified. Costs one
    /// request per run of adjacent pages not already cached.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), RangeError> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(RangeError::OutOfRange)?;
        if end > self.len {
            return Err(RangeError::OutOfRange);
        }
        self.cache
            .fill(self.file, self.start + offset, buf.len() as u64, Some(buf))
    }

    /// Make `offset..offset + len` resident before it is read, so a failed
    /// request is seen here rather than at a later read. Refuses a range that is
    /// more than the cache holds at once.
    pub fn prefetch(&self, offset: u64, len: u64) -> Result<(), RangeError> {
        let end = offset.checked_add(len).ok_or(RangeError::OutOfRange)?;
        if end > self.len {
            return Err(RangeError::OutOfRange);
        }
        let page = self.cache.page as u64;
        let from = self.start + offset;
        let span = if len == 0 {
            0
        } else {
            (from + len - 1) / page - from / page + 1
        };
        if span > self.cache.pages as u64 {
            return Err(RangeError::OutOfRange);
        }
        self.cache.fill(self.file, from, len, None)
    }

    /// A view of `offset..offset + len` of this view.
    pub fn slice(&self, offset: u64, len: u64) -> Result<Self, RangeError> {
        let end = offset.checked_add(len).ok_or(RangeError::OutOfRange)?;
        if end > self.len {
            return Err(RangeError::OutOfRange);
        }
        Ok(Self {
            cache: self.cache,
            file: self.file,
            start: self.start + offset,
            len,
        })
    }
}
