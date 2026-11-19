//! Fenced commit lanes (M03.08).
//!
//! A *lane* is an ordered log of slots in the object store. A write batch commits with **one
//! conditional create** of the next slot; there is no per-batch head CAS and no sidecar key object.
//! Many small indexes share a lane (M03.09, option B); a hot index is promoted to its own.
//!
//! Modules: [`log`] the fenced ordered-slot writer and replay; [`members`] and [`place`] lane
//! identity, the descriptor and join/leave; [`batch`] the dirty-only batcher; [`hot`] the promotion
//! rule; [`promote`] hot-index promotion; [`headcas`] the payload-plus-head-CAS alternative, kept
//! for the request comparison; [`counting`] request accounting over any store.
//!
//! [`head`] and [`catalog`] are M03.05: the structural head (authority, epoch inventory, cover and
//! tail locators, changed only by conditional transition) and the paged name catalog.
//!
//! [`admission`], [`rendezvous`], [`quota`], [`feedback`] and [`subscribe`] are M13.02: tenant
//! fairness and hot placement for a node's compute, none of which touches who is authoritative.
//!
//! [`sealing`] is M06.09: a [`SlotCipher`] a writer is given seals each slot's inline payloads before
//! the one conditional create, and replay opens them. The lane never sees a key.
//!
//! Not here, by design: engine apply and
//! checkpoints that move the cover (M06). A caller supplies the cover it replays from.
pub mod activation;
pub mod admission;
pub mod batch;
pub mod catalog;
pub mod counting;
mod cover;
mod error;
pub mod feedback;
pub mod head;
pub mod headcas;
pub mod hot;
mod io;
pub mod log;
pub mod members;
pub mod partition;
pub mod place;
pub mod promote;
pub mod quota;
pub mod rendezvous;
pub mod sealing;
pub mod subscribe;

pub use catalog::{
    Catalog, CatalogCursor, RepairClaim, Repaired, Resolution, VerifyReport, create_catalog,
    open_catalog,
};
pub use error::{Error, Result};
pub use head::{Advance, Change, advance_head, create_head, read_head};
pub use io::block_on;
pub use log::{Ack, LaneWriter, OpenReport, Reclaimed, WriterStats, read_log};
pub use members::{
    LaneId, create_lane, dedicated_group, join_members, lane_exists, read_descriptor, shared_group,
};
pub use place::{Placement, Placer};
pub use promote::{Promotion, Route, route};
pub use sealing::{Payloads, SealFailure, SlotCipher, read_log_sealed};
