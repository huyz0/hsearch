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
use hsearch_object::MetadataStore;

use crate::log::Log;

/// Most requests packed into one call of the committer.
const MAX_PACKED_REQUESTS: usize = 1024;

enum Job {
    Commit(Vec<Request>, Sender<Vec<Outcome>>),
    External(Box<Request>, External, Sender<Outcome>),
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
        std::thread::spawn(move || run(committer, &inbox, wait, &flag));
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

fn gone(n: usize) -> Vec<Outcome> {
    vec![Outcome::Rejected(Reject::Poisoned); n]
}

impl Log for LaneHandle {
    fn commit(&mut self, requests: Vec<Request>) -> Vec<Outcome> {
        let n = requests.len();
        if n == 0 {
            return Vec::new();
        }
        let (reply, answer) = channel();
        if self.jobs.send(Job::Commit(requests, reply)).is_err() {
            return gone(n);
        }
        answer.recv().unwrap_or_else(|_| gone(n))
    }

    fn commit_external(&mut self, request: Request, external: External) -> Outcome {
        let (reply, answer) = channel();
        if self
            .jobs
            .send(Job::External(Box::new(request), external, reply))
            .is_err()
        {
            return Outcome::Rejected(Reject::Poisoned);
        }
        answer.recv().unwrap_or(Outcome::Rejected(Reject::Poisoned))
    }

    fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
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
            Job::Commit(requests, reply) => (requests, reply),
        };
        // ALLOC: at most MAX_PACKED_REQUESTS requests, each at most one inline record.
        let mut packed = requests;
        let mut replies = vec![(packed.len(), replies)];
        let deadline = Instant::now() + wait;
        while packed.len() < MAX_PACKED_REQUESTS {
            let next = if wait.is_zero() {
                inbox.try_recv().ok()
            } else {
                inbox
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .ok()
            };
            match next {
                Some(Job::Commit(more, reply)) => {
                    replies.push((more.len(), reply));
                    packed.extend(more);
                }
                Some(external @ Job::External(..)) => {
                    held = Some(external);
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
