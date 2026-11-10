//! Independent bounded legal-state-chain search over recorded contract calls.
//!
//! No transport/driver mutation routine or final-state oracle is reused. A
//! fresh empty namespace and externally authenticated capture/plan are inputs,
//! not facts this module can establish. Consistency is not provider readiness.
use crate::model::*;
use std::collections::BTreeSet;
use std::mem::size_of;
use std::time::{Duration, Instant};

/// Finite search configuration, always below the hard component ceilings.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    states: u32,
    duration: Duration,
    working_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            states: 100_000,
            duration: Duration::from_secs(10),
            working_bytes: 64 * 1024 * 1024,
        }
    }
}
impl Limits {
    /// Shorten limits only. Zero limits deliberately produce inconclusive checks.
    pub fn bounded(states: u32, duration: Duration, working_bytes: usize) -> Result<Self, Invalid> {
        if states > 100_000
            || duration > Duration::from_secs(10)
            || working_bytes > 64 * 1024 * 1024
        {
            return Err(Invalid::Bound);
        }
        Ok(Self {
            states,
            duration,
            working_bytes,
        })
    }
}

/// A fail-closed reason; never an alternate form of pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Inconclusive {
    /// Capture/attempt/completion/race records missing.
    Incomplete,
    /// Search state/clock budget exhausted.
    SearchBudget,
    /// Explicit model storage cannot fit the admitted working-byte budget.
    WorkingMemory,
    /// Original request acceptance remains unresolved.
    UnknownOutcome,
    /// Exact selected-byte oracle missing.
    MissingRangeOracle,
    /// Unsupported capability or unavailable resolution was observed.
    CapabilityUnavailable,
    /// Physical retry policy or actual race overlap not established.
    AttemptOrOverlap,
    /// Restart/multipart or auxiliary evidence has no implemented model yet.
    UnmodeledEvidence,
}
/// One recorded invocation ordering, not a provider generation sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Witness {
    /// Exact operation IDs in legal order.
    pub order: Vec<u16>,
    /// Candidate transitions examined within the bounded search.
    pub explored: u32,
}
/// Semantic result. Only Consistent has a fully resolved model witness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// A legal ordering consistent with every modeled observation.
    Consistent(Witness),
    /// All legal candidate orderings refuted within budget.
    Rejected {
        /// Number of examined transitions.
        explored: u32,
    },
    /// No eligible semantic conclusion, even if a partial ordering exists.
    Inconclusive {
        /// Explicit continuing uncertainty.
        reason: Inconclusive,
        /// Candidate transitions examined.
        explored: u32,
    },
}

// Unobserved is a symbolic uncertainty variable bound to an existing request,
// never an invented provider token, returned receipt or numeric generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Token {
    Known(AtomId),
    Unobserved(AtomId),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Image {
    token: Token,
    payload: Payload,
    metadata: AtomId,
}
#[derive(Clone, Copy)]
struct Accepted {
    mutation: Mutation,
    image: Image,
}
struct State {
    heads: Vec<(ObjectId, Image)>,
    versions: Vec<(ObjectId, Image)>,
    receipts: Vec<Accepted>,
    pending: Vec<Mutation>,
    uncertain: Option<Inconclusive>,
}
impl State {
    fn new() -> Self {
        Self {
            heads: Vec::with_capacity(MAX_OPERATIONS),
            versions: Vec::with_capacity(MAX_OPERATIONS),
            receipts: Vec::with_capacity(MAX_OPERATIONS),
            pending: Vec::with_capacity(MAX_OPERATIONS),
            uncertain: None,
        }
    }
    fn fork(&self) -> Self {
        let mut result = Self::new();
        result.heads.extend_from_slice(&self.heads);
        result.versions.extend_from_slice(&self.versions);
        result.receipts.extend_from_slice(&self.receipts);
        result.pending.extend_from_slice(&self.pending);
        result.uncertain = self.uncertain;
        result
    }
    fn reserved_bytes() -> usize {
        size_of::<Self>()
            + MAX_OPERATIONS
                * (2 * size_of::<(ObjectId, Image)>()
                    + size_of::<Accepted>()
                    + size_of::<Mutation>())
    }
    fn head(&self, object: ObjectId) -> Option<Image> {
        self.heads
            .iter()
            .find(|(o, _)| *o == object)
            .map(|(_, v)| *v)
    }
    fn accepted(&self, request: AtomId) -> Option<Accepted> {
        self.receipts
            .iter()
            .find(|a| a.mutation.request == request)
            .copied()
    }
    fn matches(&self, m: Mutation) -> bool {
        match m.precondition {
            Precondition::Absent => self.head(m.object).is_none(),
            Precondition::Matches(t) => self
                .head(m.object)
                .is_some_and(|v| v.token == Token::Known(t)),
        }
    }
    fn pending(&mut self, m: Mutation) -> bool {
        if self.pending.contains(&m) {
            return true;
        }
        if self.pending.len() == MAX_OPERATIONS {
            return false;
        }
        self.pending.push(m);
        true
    }
    fn resolved(&mut self, m: Mutation) {
        self.pending.retain(|p| *p != m);
    }
    fn commit(&mut self, m: Mutation, token: Token) -> bool {
        if !self.matches(m)
            || self.receipts.len() >= MAX_OPERATIONS
            || self.versions.len() >= MAX_OPERATIONS
            || self
                .versions
                .iter()
                .any(|(o, v)| *o == m.object && v.token == token)
        {
            return false;
        }
        let image = Image {
            token,
            payload: m.payload,
            metadata: m.metadata,
        };
        if let Some((_, current)) = self.heads.iter_mut().find(|(o, _)| *o == m.object) {
            *current = image;
        } else {
            self.heads.push((m.object, image));
        }
        self.versions.push((m.object, image));
        self.receipts.push(Accepted { mutation: m, image });
        true
    }
}

fn image(v: Version) -> Image {
    Image {
        token: Token::Known(v.token),
        payload: v.payload,
        metadata: v.metadata,
    }
}

fn transition(
    state: &State,
    op: Operation,
    variant: usize,
    tokens: &[AtomId],
    plan: &Plan,
) -> Option<State> {
    let outcome = op.outcome?;
    let mut next = state.fork();
    if outcome == Outcome::BeforeSendRefusal {
        return (variant == 0).then_some(next);
    }
    if outcome == Outcome::Unsupported {
        next.uncertain = Some(Inconclusive::CapabilityUnavailable);
        return (variant == 0).then_some(next);
    }
    if outcome == Outcome::UnknownOutcome
        && matches!(op.action, Action::Head(_) | Action::ReadRange { .. })
    {
        next.uncertain = Some(Inconclusive::CapabilityUnavailable);
        return (variant == 0).then_some(next);
    }
    match op.action {
        Action::Write(m) => {
            let accepted = state.accepted(m.request);
            if outcome == Outcome::UnknownOutcome {
                if !next.pending(m) {
                    return None;
                }
                if variant == 0 {
                    return Some(next);
                }
                if accepted.is_some() {
                    return None;
                }
                let token = if variant == 1 {
                    Token::Unobserved(m.request)
                } else {
                    Token::Known(*tokens.get(variant - 2)?)
                };
                return next.commit(m, token).then_some(next);
            }
            if variant != 0 {
                return None;
            }
            if let Some(a) = accepted {
                return match outcome {
                    Outcome::Receipt(r)
                        if a.mutation == m
                            && r.request == m.request
                            && image(r.version) == a.image =>
                    {
                        next.resolved(m);
                        Some(next)
                    }
                    Outcome::IdentityConflict if a.mutation != m => {
                        next.resolved(m);
                        Some(next)
                    }
                    _ => None,
                };
            }
            match outcome {
                Outcome::Receipt(r)
                    if r.request == m.request
                        && r.version.payload == m.payload
                        && r.version.metadata == m.metadata =>
                {
                    if !next.commit(m, Token::Known(r.version.token)) {
                        return None;
                    }
                    next.resolved(m);
                    Some(next)
                }
                Outcome::Conflict if !state.matches(m) => Some(next),
                _ => None,
            }
        }
        Action::Resolve(m) => {
            if variant != 0 {
                return None;
            }
            if outcome == Outcome::UnknownOutcome || outcome == Outcome::ResolutionUnavailable {
                if !next.pending(m) {
                    return None;
                }
                next.uncertain = Some(Inconclusive::CapabilityUnavailable);
                return Some(next);
            }
            let a = state.accepted(m.request)?;
            match outcome {
                Outcome::Receipt(r)
                    if a.mutation == m && r.request == m.request && image(r.version) == a.image =>
                {
                    next.resolved(m);
                    Some(next)
                }
                Outcome::IdentityConflict if a.mutation != m => {
                    next.resolved(m);
                    Some(next)
                }
                _ => None,
            }
        }
        Action::Head(object) => {
            if variant != 0 {
                return None;
            }
            match (state.head(object), outcome) {
                (None, Outcome::NotFound) => Some(next),
                (Some(actual), Outcome::Head(v)) if actual == image(v) => Some(next),
                _ => None,
            }
        }
        Action::ReadRange {
            object,
            token,
            offset,
            length,
        } => {
            if variant != 0 {
                return None;
            }
            let end = offset.checked_add(length);
            if length == 0 || end.is_none() {
                return (outcome == Outcome::InvalidRange).then_some(next);
            }
            let saved = state
                .versions
                .iter()
                .find(|(o, v)| *o == object && v.token == Token::Known(token))
                .map(|(_, v)| *v);
            let Some(saved) = saved else {
                return (outcome == Outcome::NotFound).then_some(next);
            };
            if end? > saved.payload.length {
                return (outcome == Outcome::InvalidRange).then_some(next);
            }
            let Outcome::Range {
                token: observed,
                length: received,
                digest,
            } = outcome
            else {
                return None;
            };
            if token != observed || received != length {
                return None;
            }
            let oracle = plan
                .ranges
                .iter()
                .find(|r| r.payload == saved.payload && r.offset == offset && r.length == length);
            if let Some(oracle) = oracle {
                (oracle.digest == digest).then_some(next)
            } else {
                next.uncertain = Some(Inconclusive::MissingRangeOracle);
                Some(next)
            }
        }
    }
}

fn evidence(history: &History, plan: &Plan) -> Result<Option<Inconclusive>, Invalid> {
    history.validate()?;
    if history.binding != plan.binding
        || history.scenario != plan.scenario
        || history.atoms != plan.atoms
    {
        return Err(Invalid::Binding);
    }
    if usize::from(plan.operations) > MAX_OPERATIONS
        || plan.invocations.len() > MAX_OPERATIONS
        || plan.atoms.len() > MAX_ATOMS
        || plan.ranges.len() > MAX_OPERATIONS
        || plan.races.len() > 16
    {
        return Err(Invalid::Bound);
    }
    if plan.invocations.len() != usize::from(plan.operations) {
        return Err(Invalid::Shape);
    }
    let mut invocation_ids = BTreeSet::new();
    for expected in &plan.invocations {
        if expected.id == 0 || expected.id > plan.operations || !invocation_ids.insert(expected.id)
        {
            return Err(Invalid::Shape);
        }
        if let Some(observed) = history.operations.iter().find(|o| o.id == expected.id)
            && (observed.client != expected.client
                || observed.generation != expected.generation
                || observed.race != expected.race
                || observed.continuation_of != expected.continuation_of
                || observed.action != expected.action)
        {
            return Err(Invalid::Shape);
        }
    }
    for (index, r) in plan.ranges.iter().enumerate() {
        if r.payload.length > 16 * 1024 * 1024
            || r.length == 0
            || r.offset
                .checked_add(r.length)
                .is_none_or(|end| end > r.payload.length)
        {
            return Err(Invalid::Bound);
        }
        if plan.ranges[..index]
            .iter()
            .any(|p| p.payload == r.payload && p.offset == r.offset && p.length == r.length)
        {
            return Err(Invalid::Shape);
        }
    }
    if plan.operations == 0
        || !history.complete
        || history.declared_operations != plan.operations
        || usize::from(history.declared_operations) != history.operations.len()
        || usize::from(history.declared_attempts) != history.attempts.len()
        || history
            .operations
            .iter()
            .any(|o| o.span.end.is_none() || o.outcome.is_none())
        || history.attempts.iter().any(|a| a.span.end.is_none())
    {
        return Ok(Some(Inconclusive::Incomplete));
    }
    for id in 1..=history.declared_operations {
        if !history.operations.iter().any(|o| o.id == id) {
            return Ok(Some(Inconclusive::Incomplete));
        }
    }
    for id in 1..=history.declared_attempts {
        if !history.attempts.iter().any(|a| a.id == id) {
            return Ok(Some(Inconclusive::Incomplete));
        }
    }
    if matches!(history.scenario, Scenario::Restart | Scenario::Multipart)
        || !history.scenario_receipt.is_empty()
    {
        return Ok(Some(Inconclusive::UnmodeledEvidence));
    }
    let mut race_ids = BTreeSet::new();
    for race in &plan.races {
        if race.id == 0 || race.participants.count_ones() < 2 || !race_ids.insert(race.id) {
            return Err(Invalid::Shape);
        }
    }
    if history.races.len() != plan.races.len() {
        return Ok(Some(Inconclusive::Incomplete));
    }
    for o in &history.operations {
        let mut attempts: Vec<_> = history
            .attempts
            .iter()
            .filter(|a| a.operation == o.id)
            .collect();
        if attempts.is_empty() {
            return Ok(Some(Inconclusive::Incomplete));
        }
        let sent: Vec<_> = attempts
            .iter()
            .copied()
            .filter(|a| a.boundary != Boundary::NotTransmitted)
            .collect();
        if sent.len() > 1 {
            return Ok(Some(Inconclusive::AttemptOrOverlap));
        }
        attempts.sort_by_key(|a| a.span.start.order);
        if attempts.windows(2).any(|pair| {
            pair[0].span.end.is_none_or(|end| {
                end.order >= pair[1].span.start.order || end.micros > pair[1].span.start.micros
            })
        }) {
            return Err(Invalid::Order);
        }
        let last = attempts.last().ok_or(Invalid::Shape)?;
        if sent.first().is_some_and(|a| a.id != last.id) {
            return Ok(Some(Inconclusive::AttemptOrOverlap));
        }
        let out = o.outcome.ok_or(Invalid::Shape)?;
        if out == Outcome::BeforeSendRefusal {
            if !sent.is_empty() {
                return Err(Invalid::Shape);
            }
        } else if sent.is_empty() {
            return Ok(Some(if out == Outcome::Unsupported {
                Inconclusive::CapabilityUnavailable
            } else {
                Inconclusive::Incomplete
            }));
        }
        if last.boundary == Boundary::ReplySuppressed && out != Outcome::UnknownOutcome {
            return Err(Invalid::Shape);
        }
        if matches!(
            last.boundary,
            Boundary::AfterTransmitBeforeReply | Boundary::DuringTransmit
        ) && out != Outcome::UnknownOutcome
        {
            return Err(Invalid::Shape);
        }
        if let Some(status) = last.status {
            let compatible = match out {
                Outcome::Receipt(_) | Outcome::Head(_) | Outcome::Range { .. } => {
                    (200..300).contains(&status)
                }
                Outcome::Conflict => matches!(status, 409 | 412),
                Outcome::NotFound => status == 404,
                Outcome::InvalidRange => status == 416,
                Outcome::IdentityConflict => status == 409,
                Outcome::UnknownOutcome | Outcome::ResolutionUnavailable | Outcome::Unsupported => {
                    true
                }
                Outcome::BeforeSendRefusal => false,
            };
            if !compatible {
                return Err(Invalid::Shape);
            }
        }
        if o.race.is_some_and(|id| !race_ids.contains(&id)) {
            return Err(Invalid::Shape);
        }
    }
    for expected in &plan.races {
        let Some(race) = history.races.iter().find(|r| r.id == expected.id) else {
            return Ok(Some(Inconclusive::Incomplete));
        };
        let mut participants = 0u16;
        let mut latest_start = 0;
        let mut earliest_end = u32::MAX;
        for o in history
            .operations
            .iter()
            .filter(|o| o.race == Some(expected.id))
        {
            if !matches!(o.action, Action::Write(_)) || participants & (1 << o.client) != 0 {
                return Err(Invalid::Shape);
            }
            participants |= 1 << o.client;
            let Some(a) = history
                .attempts
                .iter()
                .find(|a| a.operation == o.id && a.boundary != Boundary::NotTransmitted)
            else {
                return Ok(Some(Inconclusive::AttemptOrOverlap));
            };
            if o.span.start.order > race.released.order
                || a.span.start.order < race.released.order
                || a.span.start.micros < race.released.micros
            {
                return Err(Invalid::Order);
            }
            latest_start = latest_start.max(a.span.start.order);
            earliest_end = earliest_end.min(a.span.end.ok_or(Invalid::Shape)?.order);
        }
        if participants != expected.participants || latest_start >= earliest_end {
            return Ok(Some(Inconclusive::AttemptOrOverlap));
        }
    }
    Ok(None)
}

type Bits = [u64; 4];
fn contains(bits: &Bits, index: usize) -> bool {
    bits[index / 64] & (1u64 << (index % 64)) != 0
}
fn insert(bits: &mut Bits, index: usize) {
    bits[index / 64] |= 1u64 << (index % 64);
}
struct Frame {
    model: State,
    done: Bits,
    depth: usize,
    next_op: usize,
    next_variant: usize,
}

/// Search all permitted linearizations until a resolved witness, refutation or
/// finite budget. Unknown effects can use an existing observed token or a purely
/// symbolic unobserved identity; no provider token is fabricated in evidence.
pub fn check(history: &History, plan: &Plan, limits: Limits) -> Result<Verdict, Invalid> {
    let started = Instant::now();
    if let Some(reason) = evidence(history, plan)? {
        return Ok(Verdict::Inconclusive {
            reason,
            explored: 0,
        });
    }
    let count = history.operations.len();
    let required = (count + 1)
        .checked_mul(State::reserved_bytes() + size_of::<Frame>())
        .and_then(|n| {
            n.checked_add(
                MAX_ATOMS * size_of::<AtomId>() + count * (size_of::<Bits>() + size_of::<u16>()),
            )
        })
        .ok_or(Invalid::Bound)?;
    if required > limits.working_bytes {
        return Ok(Verdict::Inconclusive {
            reason: Inconclusive::WorkingMemory,
            explored: 0,
        });
    }
    let tokens: Vec<_> = history
        .atoms
        .iter()
        .enumerate()
        .filter(|(_, a)| a.kind == AtomKind::Token)
        .map(|(i, _)| i as AtomId)
        .collect();
    let mut predecessors = vec![[0u64; 4]; count];
    for (i, op) in history.operations.iter().enumerate() {
        for (j, prior) in history.operations.iter().enumerate() {
            if prior
                .span
                .end
                .is_some_and(|end| end.order < op.span.start.order)
            {
                insert(&mut predecessors[i], j);
            }
        }
    }
    let mut stack = Vec::with_capacity(count + 1);
    stack.push(Frame {
        model: State::new(),
        done: [0; 4],
        depth: 0,
        next_op: 0,
        next_variant: 0,
    });
    let mut order = Vec::with_capacity(count);
    let mut explored = 0;
    let mut uncertain = None;
    while !stack.is_empty() {
        if started.elapsed() >= limits.duration || explored >= limits.states {
            return Ok(Verdict::Inconclusive {
                reason: Inconclusive::SearchBudget,
                explored,
            });
        }
        let top = stack.last_mut().ok_or(Invalid::Shape)?;
        if top.depth == count {
            let reason = if !top.model.pending.is_empty() {
                Some(Inconclusive::UnknownOutcome)
            } else {
                top.model.uncertain
            };
            if let Some(reason) = reason {
                uncertain = Some(reason);
            } else {
                return Ok(Verdict::Consistent(Witness { order, explored }));
            }
            stack.pop();
            order.pop();
            continue;
        }
        if top.next_op == count {
            stack.pop();
            order.pop();
            continue;
        }
        let index = top.next_op;
        let op = history.operations[index];
        let variant = top.next_variant;
        let variants = if matches!(op.action, Action::Write(_))
            && op.outcome == Some(Outcome::UnknownOutcome)
        {
            tokens.len() + 2
        } else {
            1
        };
        top.next_variant += 1;
        if top.next_variant == variants {
            top.next_op += 1;
            top.next_variant = 0;
        }
        if contains(&top.done, index)
            || predecessors[index]
                .iter()
                .zip(top.done)
                .any(|(need, done)| need & !done != 0)
        {
            continue;
        }
        explored += 1;
        let Some(model) = transition(&top.model, op, variant, &tokens, plan) else {
            continue;
        };
        let mut done = top.done;
        insert(&mut done, index);
        let depth = top.depth + 1;
        order.push(op.id);
        stack.push(Frame {
            model,
            done,
            depth,
            next_op: 0,
            next_variant: 0,
        });
    }
    Ok(if let Some(reason) = uncertain {
        Verdict::Inconclusive { reason, explored }
    } else {
        Verdict::Rejected { explored }
    })
}
