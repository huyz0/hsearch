//! Resolved-operation preparation (M04.03).
//!
//! A client request that depends on current state (`_update` with a partial
//! document, an upsert, `detect_noop`, `if_seq_no` and `if_primary_term`, a
//! create) is evaluated **once**, here, against committed state. What is
//! logged is the resolved whole-document [`Op`], so replay applies logged
//! operations and never calls the resolver: nothing in this module reads a clock,
//! a random source or the network, and there is no script path (A11).
//!
//! Speculative state never leaks: preparation takes `&impl CommittedState` and
//! mutates nothing. Items of one batch see the earlier items of the same batch
//! (the batch is one atomic slot). A batch that fails to commit is dropped, and
//! the next preparation sees committed state only. A preparation remembers the
//! committed head it was resolved against; [`Prepared::ensure_current`] refuses
//! it once any slot has committed since, so two batches prepared in parallel
//! cannot both be logged from the same stale view.
//!
//! Cost: no object-store request. The committed document is read through
//! [`CommittedState::get`] only for ids a request names that need it (a partial
//! update, a condition, a create, a delete); the resolver holds one overlay entry
//! per distinct id in the batch, at most [`MAX_SLOT_OPS`] documents and, because a
//! request that would push the batch's logged text past the slot cap is refused
//! with [`Reject::Limit`], at most that cap of text. An update copies the
//! document up to three times (read, merge, log); it suits ordinary documents,
//! and a very large field belongs on the whole-document index path.
use crate::format::{Field, MAX_SLOT_OPS, Op, slot_text_cap, text_fits};
use hsearch_engine::Kind;

/// Longest document identity a slot decodes (`format` reads ids with this cap).
pub const MAX_ID_BYTES: usize = 256;
use std::collections::{HashMap, VecDeque};

/// A committed document: where it sits in the log and what it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Committed {
    /// Log version the committing operation was assigned.
    pub seq_no: u64,
    /// Writer incarnation that committed it.
    pub primary_term: u64,
    /// Field values in schema order.
    pub fields: Vec<Field>,
}

/// Read access to committed state. Implemented by the writer over its
/// real-time view; nothing here can change it.
pub trait CommittedState {
    /// Slots committed so far. A preparation is only valid at this value.
    fn head(&self) -> u64;
    /// The live committed document, or `None` if absent or deleted.
    fn get(&self, id: &str) -> Option<Committed>;
}

/// An optimistic-concurrency condition. Both parts or neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cond {
    if_seq_no: Option<u64>,
    if_primary_term: Option<u64>,
}

impl Cond {
    /// No condition.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            if_seq_no: None,
            if_primary_term: None,
        }
    }
    /// A condition as the client sent it; a lone half is refused at preparation.
    #[must_use]
    pub const fn new(if_seq_no: Option<u64>, if_primary_term: Option<u64>) -> Self {
        Self {
            if_seq_no,
            if_primary_term,
        }
    }
    fn state(self) -> CondState {
        match (self.if_seq_no, self.if_primary_term) {
            (None, None) => CondState::None,
            (Some(n), Some(t)) => CondState::Check(n, t),
            _ => CondState::Malformed,
        }
    }
}

enum CondState {
    None,
    Check(u64, u64),
    Malformed,
}

/// A client request that needs resolving.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// Create or replace a whole document.
    Index {
        /// Document identity.
        id: String,
        /// Every field, in schema order.
        fields: Vec<Field>,
        /// Optional concurrency condition.
        cond: Cond,
    },
    /// Create only when no live document exists.
    Create {
        /// Document identity.
        id: String,
        /// Every field, in schema order.
        fields: Vec<Field>,
    },
    /// Remove a document.
    Delete {
        /// Document identity.
        id: String,
        /// Optional concurrency condition.
        cond: Cond,
    },
    /// Merge a partial document into the committed one.
    Update {
        /// Document identity.
        id: String,
        /// Named fields to replace.
        doc: Vec<(String, Field)>,
        /// The whole document to create when none exists.
        upsert: Option<Vec<Field>>,
        /// Use `doc` as the whole document when none exists.
        doc_as_upsert: bool,
        /// Log nothing when the merge changes nothing.
        detect_noop: bool,
        /// Optional concurrency condition (not with an upsert).
        cond: Cond,
    },
}

/// Why a request was refused. Nothing was logged for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// The condition does not match the committed document, or it is absent.
    Conflict,
    /// An update names a document that does not exist and has no upsert.
    Missing,
    /// A create names a document that exists.
    Exists,
    /// The request is malformed: wrong shape, unknown or repeated field,
    /// mistyped value or a half condition.
    Invalid,
    /// The resulting document is over the byte bound.
    Limit,
}

/// What a request did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A new document.
    Created,
    /// An existing document replaced.
    Updated,
    /// An existing document removed.
    Deleted,
    /// A delete of a document that was not there.
    NotFound,
    /// An update that changed nothing.
    NoOp,
    /// Refused.
    Rejected(Reject),
}

/// The per-request result, in submission order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// Document identity.
    pub id: String,
    /// The version assigned to a logged operation, or the committed `seq_no`
    /// (0 if none) for one that logged nothing.
    pub seq_no: u64,
    /// The writer incarnation of that version.
    pub primary_term: u64,
    /// What happened.
    pub outcome: Outcome,
}

/// A batch of resolved operations ready to become one slot.
#[derive(Debug)]
pub struct Prepared {
    base_head: u64,
    first_version: i64,
    ops: Vec<Op>,
    items: Vec<Item>,
}

/// The committed head moved after this batch was prepared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stale;

impl Prepared {
    /// The operations to log, in order. Version `first_version + i` for op `i`.
    #[must_use]
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }
    /// One result per request, in submission order.
    #[must_use]
    pub fn items(&self) -> &[Item] {
        &self.items
    }
    /// The version of the first logged operation.
    #[must_use]
    pub fn first_version(&self) -> i64 {
        self.first_version
    }
    /// `Ok` only while no slot has committed since this was prepared.
    ///
    /// # Errors
    /// [`Stale`] when the head moved: prepare again from committed state.
    pub fn ensure_current(&self, committed_head: u64) -> Result<(), Stale> {
        if committed_head == self.base_head {
            Ok(())
        } else {
            Err(Stale)
        }
    }
}

/// Resolves requests for one index and writer incarnation.
#[derive(Clone, Debug)]
pub struct Resolver {
    primary_term: u64,
    names: Vec<String>,
    kinds: Vec<Kind>,
    max_document_bytes: usize,
}

type Live = Option<(u64, u64, Vec<Field>)>;

fn bytes(fields: &[Field]) -> usize {
    fields
        .iter()
        .map(|f| match f {
            Field::Str(s) => s.len(),
            Field::Vector(v) => v.len() * 4,
            _ => 8,
        })
        .sum()
}

impl Resolver {
    /// `fields` are the schema's field names and kinds in order;
    /// `max_document_bytes` bounds a merged document.
    #[must_use]
    pub fn new(primary_term: u64, fields: &[(&str, Kind)], max_document_bytes: usize) -> Self {
        Self {
            primary_term,
            names: fields.iter().map(|(n, _)| (*n).to_owned()).collect(),
            kinds: fields.iter().map(|(_, k)| *k).collect(),
            max_document_bytes,
        }
    }

    fn kind_matches(&self, i: usize, field: &Field) -> bool {
        matches!(
            (self.kinds[i], field),
            (Kind::Keyword | Kind::Text, Field::Str(_))
                | (Kind::Integer, Field::Int(_))
                | (Kind::Boolean, Field::Bool(_))
                | (Kind::Date, Field::Date(_))
        ) || matches!((self.kinds[i], field), (Kind::Vector(spec), Field::Vector(v))
            if v.len() == usize::from(spec.dimension) && v.iter().all(|c| c.is_finite()))
    }

    /// Resolve `requests` against `state`. Item `i` that logs an operation takes
    /// the next version from `first_version`; refused and no-op items take none.
    ///
    /// # Errors
    /// [`crate::Error::Invalid`] when the batch is empty or over
    /// [`MAX_SLOT_OPS`] requests; nothing is prepared.
    pub fn prepare(
        &self,
        state: &impl CommittedState,
        first_version: i64,
        requests: &[Request],
    ) -> crate::Result<Prepared> {
        if requests.is_empty()
            || requests.len() > MAX_SLOT_OPS
            || first_version < 1
            || first_version.checked_add(MAX_SLOT_OPS as i64).is_none()
        {
            return Err(crate::Error::Invalid);
        }
        // Read the head before any document: a slot that commits mid-batch then
        // makes this preparation stale instead of being hidden by a later read.
        let base_head = state.head();
        let mut overlay: HashMap<String, Live> = HashMap::new();
        let mut ops = Vec::new();
        let mut items = Vec::with_capacity(requests.len());
        // The batch becomes one slot: the text it logs obeys the slot's own cap.
        let cap = slot_text_cap(self.max_document_bytes);
        let mut logged = 0usize;
        for request in requests {
            let id = match request {
                Request::Index { id, .. }
                | Request::Create { id, .. }
                | Request::Delete { id, .. }
                | Request::Update { id, .. } => id,
            };
            if id.is_empty() || id.len() > MAX_ID_BYTES {
                items.push(Item {
                    id: id.clone(),
                    seq_no: 0,
                    primary_term: 0,
                    outcome: Outcome::Rejected(Reject::Invalid),
                });
                continue;
            }
            let current: Live = match overlay.get(id) {
                Some(entry) => entry.clone(),
                None => state.get(id).map(|c| (c.seq_no, c.primary_term, c.fields)),
            };
            let next = first_version + ops.len() as i64;
            let mut verdict = self.resolve(request, current.as_ref());
            if let Verdict::Put(_, fields) = &verdict {
                if text_fits(logged, bytes(fields), cap) {
                    logged += bytes(fields);
                } else {
                    verdict = Verdict::Skip(Outcome::Rejected(Reject::Limit));
                }
            }
            let (seq_no, primary_term) = current.as_ref().map_or((0, 0), |c| (c.0, c.1));
            let item = |outcome| Item {
                id: id.clone(),
                seq_no,
                primary_term,
                outcome,
            };
            match verdict {
                Verdict::Skip(outcome) => items.push(item(outcome)),
                Verdict::Put(outcome, fields) => {
                    let seq = next as u64;
                    overlay.insert(id.clone(), Some((seq, self.primary_term, fields.clone())));
                    ops.push(Op::Put {
                        id: id.clone(),
                        fields,
                    });
                    items.push(Item {
                        seq_no: seq,
                        primary_term: self.primary_term,
                        ..item(outcome)
                    });
                }
                Verdict::Delete => {
                    overlay.insert(id.clone(), None);
                    ops.push(Op::Delete { id: id.clone() });
                    items.push(Item {
                        seq_no: next as u64,
                        primary_term: self.primary_term,
                        ..item(Outcome::Deleted)
                    });
                }
            }
        }
        Ok(Prepared {
            base_head,
            first_version,
            ops,
            items,
        })
    }

    fn shaped(&self, fields: &[Field]) -> Result<(), Reject> {
        if fields.len() != self.names.len()
            || fields
                .iter()
                .enumerate()
                .any(|(i, f)| !self.kind_matches(i, f))
        {
            Err(Reject::Invalid)
        } else if bytes(fields) > self.max_document_bytes {
            Err(Reject::Limit)
        } else {
            Ok(())
        }
    }

    fn resolve(&self, request: &Request, current: Option<&(u64, u64, Vec<Field>)>) -> Verdict {
        let refuse = Verdict::Skip;
        match request {
            Request::Index { fields, cond, .. } => {
                if let Err(r) = self.shaped(fields) {
                    return refuse(Outcome::Rejected(r));
                }
                if let Err(r) = check(*cond, current) {
                    return refuse(Outcome::Rejected(r));
                }
                let outcome = if current.is_some() {
                    Outcome::Updated
                } else {
                    Outcome::Created
                };
                Verdict::Put(outcome, fields.clone())
            }
            Request::Create { fields, .. } => {
                if let Err(r) = self.shaped(fields) {
                    return refuse(Outcome::Rejected(r));
                }
                if current.is_some() {
                    return refuse(Outcome::Rejected(Reject::Exists));
                }
                Verdict::Put(Outcome::Created, fields.clone())
            }
            Request::Delete { cond, .. } => {
                if let Err(r) = check(*cond, current) {
                    return refuse(Outcome::Rejected(r));
                }
                if current.is_some() {
                    Verdict::Delete
                } else {
                    refuse(Outcome::NotFound)
                }
            }
            Request::Update {
                doc,
                upsert,
                doc_as_upsert,
                detect_noop,
                cond,
                ..
            } => self.update(
                doc,
                upsert.as_deref(),
                *doc_as_upsert,
                *detect_noop,
                *cond,
                current,
            ),
        }
    }

    fn update(
        &self,
        doc: &[(String, Field)],
        upsert: Option<&[Field]>,
        doc_as_upsert: bool,
        detect_noop: bool,
        cond: Cond,
        current: Option<&(u64, u64, Vec<Field>)>,
    ) -> Verdict {
        let bad = |r| Verdict::Skip(Outcome::Rejected(r));
        let upserting = upsert.is_some() || doc_as_upsert;
        if upserting && !matches!(cond.state(), CondState::None) {
            return bad(Reject::Invalid);
        }
        // Every named field must exist once, whatever the document's state.
        let mut slots = Vec::with_capacity(doc.len());
        for (name, value) in doc {
            match self.names.iter().position(|n| n == name) {
                Some(i) if !slots.iter().any(|(j, _)| *j == i) => slots.push((i, value)),
                _ => return bad(Reject::Invalid),
            }
        }
        let Some(existing) = current else {
            if let Some(whole) = upsert {
                return match self.shaped(whole) {
                    Ok(()) => Verdict::Put(Outcome::Created, whole.to_vec()),
                    Err(r) => bad(r),
                };
            }
            if doc_as_upsert {
                if slots.len() != self.names.len() {
                    return bad(Reject::Invalid);
                }
                let mut whole = vec![None; self.names.len()];
                for (i, v) in &slots {
                    whole[*i] = Some((*v).clone());
                }
                let whole: Vec<Field> = whole.into_iter().flatten().collect();
                return match self.shaped(&whole) {
                    Ok(()) => Verdict::Put(Outcome::Created, whole),
                    Err(r) => bad(r),
                };
            }
            return match cond.state() {
                CondState::Malformed => bad(Reject::Invalid),
                CondState::Check(..) => bad(Reject::Conflict),
                CondState::None => bad(Reject::Missing),
            };
        };
        if let Err(r) = check(cond, current) {
            return bad(r);
        }
        if slots.is_empty() {
            return bad(Reject::Invalid);
        }
        let mut merged = existing.2.clone();
        for (i, value) in slots {
            if !self.kind_matches(i, value) {
                return bad(Reject::Invalid);
            }
            merged[i] = value.clone();
        }
        if bytes(&merged) > self.max_document_bytes {
            return bad(Reject::Limit);
        }
        if detect_noop && merged == existing.2 {
            return Verdict::Skip(Outcome::NoOp);
        }
        Verdict::Put(Outcome::Updated, merged)
    }
}

enum Verdict {
    Skip(Outcome),
    Put(Outcome, Vec<Field>),
    Delete,
}

fn check(cond: Cond, current: Option<&(u64, u64, Vec<Field>)>) -> Result<(), Reject> {
    match cond.state() {
        CondState::None => Ok(()),
        CondState::Malformed => Err(Reject::Invalid),
        CondState::Check(n, t) => match current {
            Some((seq, term, _)) if *seq == n && *term == t => Ok(()),
            _ => Err(Reject::Conflict),
        },
    }
}

/// What [`OutcomeStore::get`] found for a request id.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// Remembered with the same fingerprint: answer with these.
    Replay(&'a [Item]),
    /// The id is remembered for a different request body: refuse the reuse.
    Mismatch,
    /// Not remembered (new, or evicted).
    Unknown,
}

/// Outcomes of recently committed requests, keyed by the client's request id and
/// guarded by a fingerprint of the request body, so a retry gets the original
/// answer instead of being resolved again against state its first attempt
/// changed, and an id reused for a different body is caught. Bounded by entry
/// count and by estimated bytes; the oldest goes first. Record only after the
/// slot is durable.
#[derive(Debug)]
pub struct OutcomeStore {
    max: usize,
    max_bytes: usize,
    bytes: usize,
    order: VecDeque<String>,
    map: HashMap<String, (u64, usize, Vec<Item>)>,
}

fn cost(request: &str, items: &[Item]) -> usize {
    request.len() + 48 + items.iter().map(|i| i.id.len() + 48).sum::<usize>()
}

impl OutcomeStore {
    /// A store holding at most `max` requests and about `max_bytes` of results.
    #[must_use]
    pub fn new(max: usize, max_bytes: usize) -> Self {
        Self {
            max: max.max(1),
            max_bytes,
            bytes: 0,
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }
    /// Remember a committed request's results. An entry larger than the byte
    /// bound is not kept (a retry of it is resolved again, never replayed).
    pub fn record(&mut self, request: &str, fingerprint: u64, items: Vec<Item>) {
        let size = cost(request, &items);
        if size > self.max_bytes {
            return;
        }
        match self
            .map
            .insert(request.to_owned(), (fingerprint, size, items))
        {
            Some((_, old, _)) => self.bytes -= old,
            None => self.order.push_back(request.to_owned()),
        }
        self.bytes += size;
        while self.order.len() > self.max || self.bytes > self.max_bytes {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some((_, size, _)) = self.map.remove(&old) {
                self.bytes -= size;
            }
        }
    }
    /// The recorded results for this request id and body fingerprint.
    #[must_use]
    pub fn get(&self, request: &str, fingerprint: u64) -> Lookup<'_> {
        match self.map.get(request) {
            None => Lookup::Unknown,
            Some((f, _, items)) if *f == fingerprint => Lookup::Replay(items),
            Some(_) => Lookup::Mismatch,
        }
    }
    /// Requests remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }
    /// Whether nothing is remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}
