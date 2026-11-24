//! One lane thread for the hosts of several indexes: each host commits through a [`LaneHandle`],
//! and the thread packs what is waiting into one call of the committer, so records of different
//! indexes share a lane slot and the lane takes one append for all of them.
//!
//! Every handle sees the same poison flag: a fence lost or a writer poisoned by any host's append
//! breaks every host of the lane, because the lane has one writer and it is gone.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::time::{Duration, Instant};

use hsearch_commit::{Committer, DependencyView, External, Outcome, Reject, Request};
use hsearch_format::Id;
use hsearch_lane::{Error as LaneError, Reclaimed};
use hsearch_object::MetadataStore;

use crate::log::Log;

/// Most requests packed into one call of the committer.
const MAX_PACKED_REQUESTS: usize = 1024;
/// Most body bytes packed into one call: packing stops once this is reached, so a pack holds at
/// most this plus the one job that crossed it.
const MAX_PACKED_BYTES: usize = 8 << 20;

enum Job {
    Commit(Vec<Request>, Sender<Vec<Outcome>>),
    External(Box<Request>, External, Sender<Outcome>),
    Leave(Id, u64, Sender<Result<(), LaneError>>),
    Reclaim(Sender<Result<Reclaimed, LaneError>>),
}

/// The lane thread's owner. The thread ends when this and every handle are dropped.
pub struct SharedLane {
    jobs: SyncSender<Job>,
    poisoned: Arc<AtomicBool>,
}

/// One host's view of a shared lane.
#[derive(Clone)]
pub struct LaneHandle {
    jobs: SyncSender<Job>,
    poisoned: Arc<AtomicBool>,
}

impl SharedLane {
    /// Start the lane thread over `committer`. `wait` is how long it holds a job for others to
    /// share its slot (zero packs only what is already queued); `queue` bounds the jobs waiting.
    #[must_use]
    pub fn start<S, D>(committer: Committer<S, D>, wait: Duration, queue: usize) -> Self
    where
        S: MetadataStore + 'static,
        D: DependencyView + Send + 'static,
    {
        let (jobs, inbox) = sync_channel(queue.max(1));
        let poisoned = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&poisoned);
        std::thread::spawn(move || {
            // A thread that ends any way but its senders closing (a panic) breaks the lane.
            let guard = Broken(&flag);
            run(committer, &inbox, wait, &flag);
            std::mem::forget(guard);
        });
        Self { jobs, poisoned }
    }

    /// A handle for one host. Handles are cheap and may be made at any time.
    #[must_use]
    pub fn handle(&self) -> LaneHandle {
        LaneHandle {
            jobs: self.jobs.clone(),
            poisoned: Arc::clone(&self.poisoned),
        }
    }
}

/// What a lost lane thread leaves a caller: the job may or may not have been appended, so it is
/// unknown (never "refused", which would let the caller reuse the sequence or discard a payload),
/// and the lane is broken for every handle.
const LOST: Outcome = Outcome::Unknown { ordinal: u64::MAX };

fn gone(n: usize, poisoned: &AtomicBool) -> Vec<Outcome> {
    poisoned.store(true, Ordering::Release);
    vec![LOST; n]
}

impl LaneHandle {
    /// The lane thread is gone: every handle is broken.
    fn lost(&self) -> LaneError {
        self.poisoned.store(true, Ordering::Release);
        LaneError::Poisoned
    }
}

impl Log for LaneHandle {
    fn commit(&mut self, requests: Vec<Request>) -> Vec<Outcome> {
        let n = requests.len();
        if n == 0 {
            return Vec::new();
        }
        let (reply, answer) = channel();
        if self.jobs.send(Job::Commit(requests, reply)).is_err() {
            return gone(n, &self.poisoned);
        }
        answer.recv().unwrap_or_else(|_| gone(n, &self.poisoned))
    }

    fn commit_external(&mut self, request: Request, external: External) -> Outcome {
        let (reply, answer) = channel();
        if self
            .jobs
            .send(Job::External(Box::new(request), external, reply))
            .is_err()
        {
            return gone(1, &self.poisoned).remove(0);
        }
        answer
            .recv()
            .unwrap_or_else(|_| gone(1, &self.poisoned).remove(0))
    }

    fn leave(&mut self, index: Id, last_seq: u64) -> Result<(), LaneError> {
        let (reply, answer) = channel();
        if self.jobs.send(Job::Leave(index, last_seq, reply)).is_err() {
            return Err(self.lost());
        }
        answer.recv().unwrap_or_else(|_| Err(self.lost()))
    }

    fn reclaim(&mut self) -> Result<Reclaimed, LaneError> {
        let (reply, answer) = channel();
        if self.jobs.send(Job::Reclaim(reply)).is_err() {
            return Err(self.lost());
        }
        answer.recv().unwrap_or_else(|_| Err(self.lost()))
    }

    fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }
}

/// Raises the poison flag when dropped.
struct Broken<'a>(&'a AtomicBool);

impl Drop for Broken<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn run<S, D>(
    mut committer: Committer<S, D>,
    inbox: &Receiver<Job>,
    wait: Duration,
    poisoned: &AtomicBool,
) where
    S: MetadataStore + 'static,
    D: DependencyView + Send + 'static,
{
    let mut held: Option<Job> = None;
    loop {
        let first = match held.take() {
            Some(job) => job,
            None => match inbox.recv() {
                Ok(job) => job,
                Err(_) => return,
            },
        };
        let (requests, replies) = match first {
            Job::External(request, external, reply) => {
                let outcome = Log::commit_external(&mut committer, *request, external);
                note(&committer, std::slice::from_ref(&outcome), poisoned);
                let _ = reply.send(outcome);
                continue;
            }
            Job::Leave(index, last_seq, reply) => {
                let outcome = Log::leave(&mut committer, index, last_seq);
                note_poison(&committer, poisoned);
                let _ = reply.send(outcome);
                continue;
            }
            Job::Reclaim(reply) => {
                let outcome = Log::reclaim(&mut committer);
                note_poison(&committer, poisoned);
                let _ = reply.send(outcome);
                continue;
            }
            Job::Commit(requests, reply) => (requests, reply),
        };
        // ALLOC: at most MAX_PACKED_REQUESTS requests and MAX_PACKED_BYTES of bodies, plus the one
        // job that crossed either.
        let mut packed = requests;
        let mut bytes: usize = packed.iter().map(|r| r.body.len()).sum();
        let mut replies = vec![(packed.len(), replies)];
        let deadline = Instant::now() + wait;
        while packed.len() < MAX_PACKED_REQUESTS && bytes < MAX_PACKED_BYTES {
            let next = if wait.is_zero() {
                inbox.try_recv().ok()
            } else {
                inbox
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .ok()
            };
            match next {
                Some(Job::Commit(more, reply)) => {
                    bytes += more.iter().map(|r| r.body.len()).sum::<usize>();
                    replies.push((more.len(), reply));
                    packed.extend(more);
                }
                Some(other @ (Job::External(..) | Job::Leave(..) | Job::Reclaim(..))) => {
                    held = Some(other);
                    break;
                }
                None => break,
            }
        }
        let outcomes = Log::commit(&mut committer, packed);
        note(&committer, &outcomes, poisoned);
        let mut outcomes = outcomes.into_iter();
        for (n, reply) in replies {
            let _ = reply.send(outcomes.by_ref().take(n).collect());
        }
    }
}

fn note_poison<S, D>(committer: &Committer<S, D>, poisoned: &AtomicBool)
where
    S: MetadataStore,
    D: DependencyView,
{
    if committer.is_poisoned() {
        poisoned.store(true, Ordering::Release);
    }
}

/// Raise the shared flag when the writer is poisoned or an outcome says the fence is lost.
fn note<S, D>(committer: &Committer<S, D>, outcomes: &[Outcome], poisoned: &AtomicBool)
where
    S: MetadataStore,
    D: DependencyView,
{
    let lost = outcomes
        .iter()
        .any(|o| matches!(o, Outcome::Rejected(Reject::Fenced | Reject::Poisoned)));
    if lost || committer.is_poisoned() {
        poisoned.store(true, Ordering::Release);
    }
}
