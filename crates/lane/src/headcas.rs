//! The alternative the fenced slot log is compared with: **payload object, then a CAS on a head**.
//! Each batch is written as an immutable payload object, and the batch is committed by replacing
//! one head object that names how much is committed. It is correct (the head CAS fences a stale
//! writer) and costs two PUTs per batch against the lane's one; with several writers it also pays
//! conflict retries on the head. It exists in this crate only so the request comparison is measured
//! on the same store, with the same accounting, rather than assumed.
//!
//! The head is a plain bounded record, not an envelope kind: nothing persists it outside the
//! comparison. Payloads live under `epoch/ordinal`, and the head keeps the `(epoch, first ordinal)`
//! list that tells a reader which epoch wrote which range, so an orphan payload from a fenced
//! writer is never read and never collides with its successor.
use std::sync::Arc;

use hsearch_format::{FormatError, Layout, crc32c};
use hsearch_object::{MetadataStore, Precondition, VersionToken};

use crate::io::{Written, fresh_token, key, mutation, read_object, request_id, write};
use crate::members::LaneId;
use crate::{Error, Result};

const MAGIC: &[u8; 4] = b"HSHC";
/// Epoch starts a head may list; one more open needs a rewrite that this comparison does not model.
const MAX_EPOCHS: usize = 256;
const HEAD_BOUND: u64 = 16 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Head {
    epoch: u64,
    committed: u64,
    token: [u8; 16],
    starts: Vec<(u64, u64)>,
}

impl Head {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(48 + 16 * self.starts.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.committed.to_le_bytes());
        out.extend_from_slice(&self.token);
        out.extend_from_slice(&(self.starts.len() as u16).to_le_bytes());
        for (epoch, first) in &self.starts {
            out.extend_from_slice(&epoch.to_le_bytes());
            out.extend_from_slice(&first.to_le_bytes());
        }
        let crc = crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let bad = || Error::Format(FormatError::Malformed("comparison head"));
        let (body, crc) = bytes.split_at(bytes.len().checked_sub(4).ok_or_else(bad)?);
        if crc32c(body).to_le_bytes() != *crc || body.get(..4) != Some(MAGIC) {
            return Err(Error::Format(FormatError::Checksum));
        }
        let u64_at = |at: usize| -> Result<u64> {
            let raw = body.get(at..at + 8).ok_or_else(bad)?;
            Ok(u64::from_le_bytes(raw.try_into().map_err(|_| bad())?))
        };
        let epoch = u64_at(4)?;
        let committed = u64_at(12)?;
        let token: [u8; 16] = body
            .get(20..36)
            .ok_or_else(bad)?
            .try_into()
            .map_err(|_| bad())?;
        let n = usize::from(u16::from_le_bytes(
            body.get(36..38)
                .ok_or_else(bad)?
                .try_into()
                .map_err(|_| bad())?,
        ));
        if n > MAX_EPOCHS || body.len() != 38 + 16 * n {
            return Err(bad());
        }
        let mut starts = Vec::with_capacity(n);
        for i in 0..n {
            starts.push((u64_at(38 + 16 * i)?, u64_at(46 + 16 * i)?));
        }
        Ok(Self {
            epoch,
            committed,
            token,
            starts,
        })
    }
}

/// A writer of the payload-plus-head-CAS log.
pub struct HeadCasLog<S> {
    store: Arc<S>,
    layout: Layout,
    id: LaneId,
    head: Head,
    version: VersionToken,
    poisoned: bool,
}

impl<S: MetadataStore> HeadCasLog<S> {
    fn head_key(layout: &Layout, id: &LaneId) -> String {
        layout.lane_head(&id.domain, id.incarnation)
    }

    fn payload_key(layout: &Layout, id: &LaneId, epoch: u64, ordinal: u64) -> String {
        let head = Self::head_key(layout, id);
        let base = head.strip_suffix("HEAD").unwrap_or(&head);
        format!("{base}hc/{epoch:020}/{ordinal:020}")
    }

    async fn swap(
        &self,
        next: &Head,
        version: Option<&VersionToken>,
    ) -> Result<Option<VersionToken>> {
        let bytes = next.encode();
        let token = hsearch_format::CommitToken::new(next.token).map_err(Error::from)?;
        let request = mutation(
            &request_id("hc", &self.id.domain, next.committed, &token),
            &Self::head_key(&self.layout, &self.id),
            &bytes,
            version.map_or(Precondition::Absent, |v| Precondition::Matches(v.clone())),
        )?;
        match write(&*self.store, &request, version.is_some()).await {
            Written::Done(receipt) => Ok(Some(receipt.object.version)),
            Written::Occupied => Ok(None),
            Written::Refused(error) => Err(error.into()),
            Written::Unknown => Err(Error::Unknown {
                ordinal: next.committed,
            }),
        }
    }

    /// Become the writer: create the head, or CAS it to a new epoch (which fences the previous
    /// writer, whose next head CAS then fails). One read and one PUT when the head exists.
    pub async fn open(store: Arc<S>, layout: Layout, id: LaneId) -> Result<Self> {
        let k = key(&Self::head_key(&layout, &id))?;
        let found = read_object(&*store, &k, HEAD_BOUND).await?;
        let mut log = Self {
            store,
            layout,
            id,
            head: Head {
                epoch: 1,
                committed: 0,
                token: *fresh_token().as_bytes(),
                starts: vec![(1, 0)],
            },
            version: VersionToken::new(b"-").map_err(Error::from)?,
            poisoned: false,
        };
        match found {
            None => {
                let head = log.head.clone();
                log.version = log.swap(&head, None).await?.ok_or(Error::Exists)?;
            }
            Some((bytes, version)) => {
                let mut head = Head::decode(bytes.as_bytes())?;
                if head.starts.len() >= MAX_EPOCHS {
                    return Err(Error::Capacity);
                }
                head.epoch += 1;
                head.token = *fresh_token().as_bytes();
                head.starts.push((head.epoch, head.committed));
                log.version = log
                    .swap(&head, Some(&version))
                    .await?
                    .ok_or(Error::Contended)?;
                log.head = head;
            }
        }
        Ok(log)
    }

    /// Committed batches so far.
    #[must_use]
    pub fn committed(&self) -> u64 {
        self.head.committed
    }

    /// Whether the writer stopped after a lost CAS or an unresolved reply.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Commit one batch: PUT its payload, then CAS the head over it. Two PUTs, acknowledged only
    /// after the second. A lost CAS means a successor opened: [`Error::Fenced`], poisoned.
    pub async fn append(&mut self, payload: &[u8]) -> Result<u64> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        let ordinal = self.head.committed;
        let token = fresh_token();
        let request = mutation(
            &request_id("hcp", &self.id.domain, ordinal, &token),
            &Self::payload_key(&self.layout, &self.id, self.head.epoch, ordinal),
            payload,
            Precondition::Absent,
        )?;
        match write(&*self.store, &request, false).await {
            Written::Done(_) => {}
            Written::Occupied => {
                self.poisoned = true;
                return Err(Error::Fenced);
            }
            Written::Refused(error) => return Err(error.into()),
            Written::Unknown => {
                self.poisoned = true;
                return Err(Error::Unknown { ordinal });
            }
        }
        let mut next = self.head.clone();
        next.committed += 1;
        next.token = *fresh_token().as_bytes();
        match self.swap(&next, Some(&self.version.clone())).await {
            Ok(Some(version)) => {
                self.head = next;
                self.version = version;
                Ok(ordinal)
            }
            Ok(None) => {
                self.poisoned = true;
                Err(Error::Fenced)
            }
            Err(error @ Error::Unknown { .. }) => {
                self.poisoned = true;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    /// Read every committed payload in order: one head read, then one HEAD and one GET per batch.
    pub async fn read_all(
        store: &S,
        layout: &Layout,
        id: &LaneId,
        mut visit: impl FnMut(u64, &[u8]),
    ) -> Result<u64> {
        let k = key(&Self::head_key(layout, id))?;
        let (bytes, _) = read_object(store, &k, HEAD_BOUND)
            .await?
            .ok_or(Error::NoLane)?;
        let head = Head::decode(bytes.as_bytes())?;
        for ordinal in 0..head.committed {
            let epoch = head
                .starts
                .iter()
                .rev()
                .find(|(_, first)| *first <= ordinal)
                .map(|(epoch, _)| *epoch)
                .ok_or(Error::Format(FormatError::Malformed("comparison head")))?;
            let pk = key(&Self::payload_key(layout, id, epoch, ordinal))?;
            let (payload, _) = read_object(store, &pk, 64 << 20)
                .await?
                .ok_or(Error::Format(FormatError::Malformed("missing payload")))?;
            visit(ordinal, payload.as_bytes());
        }
        Ok(head.committed)
    }
}
