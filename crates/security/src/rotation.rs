//! Rotation and revocation state machines (M05.05): sans-I/O, so every history can be enumerated.
//!
//! * [`Inventory`] tracks, per immutable object, the key envelope readers use and the overlays being moved
//!   through *stage, verify, publish*, and per pinned reader the envelope it captured. A key version is
//!   retired only when **nothing selects it and no live reader holds it**: that new writes use another
//!   version is not a reason. An unpublished overlay is lost in a restart and the inline wrapper reads.
//! * [`PolicyGate`] is policy freshness for one index: a policy older than its allowed age, or one that is a
//!   revocation, refuses every operation. It never extends freshness from a late clock or a lost message.
//!
//! **Retained roots must be pinned.** A point-in-time read, a snapshot or a manifest inside its retention
//! window needs the envelopes it was written with; the inventory only knows about roots that were pinned, so
//! retention is expressed as holding a pin until the retention ends. A historical root nobody pinned is
//! invisible here, and retiring a version it needs breaks it.
//!
//! The caller owns the I/O: writing the overlay object, proving it unwraps and opens the header, and the
//! atomic publication of the selection. This module only says which step may follow which.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::keys::{KeyRef, KeyVersion};

/// An object's identity in the inventory.
pub type Object = [u8; 16];
/// A pinned set of objects (a reader's root).
pub type RootId = u64;

/// A key envelope: which customer key version wraps a data key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Envelope {
    /// The customer key.
    pub key: KeyRef,
    /// The provider's version.
    pub version: KeyVersion,
}

/// Why a step was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RotationError {
    /// No such object.
    UnknownObject,
    /// No such root.
    UnknownRoot,
    /// No overlay is waiting for this step.
    NotStaged,
    /// The overlay was staged but not verified.
    NotVerified,
    /// This many selections still depend on the envelope or version.
    InUse(usize),
    /// The key version was retired.
    Retired,
    /// A reader pins the object.
    Pinned,
    /// The object already exists.
    Exists,
}

struct ObjectState {
    inline: Envelope,
    /// An overlay and whether it was verified. Lost in a restart.
    staged: Option<(Envelope, bool)>,
    /// The published overlay. Survives a restart.
    published: Option<Envelope>,
}

impl ObjectState {
    fn selected(&self) -> &Envelope {
        self.published.as_ref().unwrap_or(&self.inline)
    }
}

/// Envelopes per object, per pinned reader, and the retired key versions.
#[derive(Default)]
pub struct Inventory {
    objects: BTreeMap<Object, ObjectState>,
    roots: BTreeMap<RootId, BTreeMap<Object, Envelope>>,
    retired: BTreeSet<(KeyRef, KeyVersion)>,
    next_root: RootId,
}

impl Inventory {
    /// An empty inventory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an object with the inline envelope it was written with. Refuses an object that exists (which
    /// would drop its published overlay) and an envelope whose version was retired.
    pub fn add_object(&mut self, object: Object, inline: Envelope) -> Result<(), RotationError> {
        if self.objects.contains_key(&object) {
            return Err(RotationError::Exists);
        }
        if self.is_retired_envelope(&inline) {
            return Err(RotationError::Retired);
        }
        self.objects.insert(
            object,
            ObjectState {
                inline,
                staged: None,
                published: None,
            },
        );
        Ok(())
    }

    /// Removes an object (garbage collection). Refused while any reader pins it.
    pub fn remove_object(&mut self, object: Object) -> Result<(), RotationError> {
        if !self.objects.contains_key(&object) {
            return Err(RotationError::UnknownObject);
        }
        if self.roots.values().any(|r| r.contains_key(&object)) {
            return Err(RotationError::Pinned);
        }
        self.objects.remove(&object);
        Ok(())
    }

    fn is_retired_envelope(&self, e: &Envelope) -> bool {
        self.retired.contains(&(e.key.clone(), e.version.clone()))
    }

    /// Stages an overlay envelope for an object. Replaces an earlier staged one and resets verification.
    pub fn stage(&mut self, object: Object, overlay: Envelope) -> Result<(), RotationError> {
        if self.is_retired_envelope(&overlay) {
            return Err(RotationError::Retired);
        }
        let state = self
            .objects
            .get_mut(&object)
            .ok_or(RotationError::UnknownObject)?;
        state.staged = Some((overlay, false));
        Ok(())
    }

    /// Records that the staged overlay was written, unwraps, and opens the object's header.
    pub fn verified(&mut self, object: Object) -> Result<(), RotationError> {
        let state = self
            .objects
            .get_mut(&object)
            .ok_or(RotationError::UnknownObject)?;
        match &mut state.staged {
            Some((_, verified)) if !*verified => {
                *verified = true;
                Ok(())
            }
            _ => Err(RotationError::NotStaged),
        }
    }

    /// Publishes the verified overlay: from now on readers select it. The caller makes the publication
    /// atomic; this only allows it from the verified state.
    pub fn publish(&mut self, object: Object) -> Result<(), RotationError> {
        let retired = self.retired.clone();
        let state = self
            .objects
            .get_mut(&object)
            .ok_or(RotationError::UnknownObject)?;
        match state.staged.take() {
            None => Err(RotationError::NotStaged),
            Some((overlay, false)) => {
                state.staged = Some((overlay, false));
                Err(RotationError::NotVerified)
            }
            Some((overlay, true)) => {
                if retired.contains(&(overlay.key.clone(), overlay.version.clone())) {
                    // Still staged: the owner can stage another version over it.
                    state.staged = Some((overlay, true));
                    return Err(RotationError::Retired);
                }
                state.published = Some(overlay);
                Ok(())
            }
        }
    }

    /// The envelope a new reader uses: the published overlay, else the inline one.
    pub fn selected(&self, object: Object) -> Result<&Envelope, RotationError> {
        self.objects
            .get(&object)
            .map(ObjectState::selected)
            .ok_or(RotationError::UnknownObject)
    }

    /// Pins a reader: captures the current selection of each object, which it keeps until released.
    pub fn pin(&mut self, objects: &[Object]) -> Result<RootId, RotationError> {
        let mut captured = BTreeMap::new();
        for object in objects {
            captured.insert(*object, self.selected(*object)?.clone());
        }
        let id = self.next_root;
        self.next_root += 1;
        self.roots.insert(id, captured);
        Ok(id)
    }

    /// The envelope a pinned reader captured for an object.
    pub fn root_envelope(&self, root: RootId, object: Object) -> Result<&Envelope, RotationError> {
        self.roots
            .get(&root)
            .ok_or(RotationError::UnknownRoot)?
            .get(&object)
            .ok_or(RotationError::UnknownObject)
    }

    /// Releases a pinned reader.
    pub fn release(&mut self, root: RootId) -> Result<(), RotationError> {
        self.roots
            .remove(&root)
            .map(|_| ())
            .ok_or(RotationError::UnknownRoot)
    }

    fn users_of(&self, matches: impl Fn(&Envelope) -> bool) -> usize {
        let current = self
            .objects
            .values()
            .filter(|o| matches(o.selected()))
            .count();
        let pinned = self
            .roots
            .values()
            .flat_map(BTreeMap::values)
            .filter(|e| matches(e))
            .count();
        current + pinned
    }

    /// How many current selections and pinned readers depend on an envelope.
    pub fn users(&self, envelope: &Envelope) -> usize {
        self.users_of(|e| e == envelope)
    }

    /// Retires a key version, so no overlay may be staged under it and the provider may disable it. Refused
    /// while anything selects it or any live reader holds it.
    pub fn retire(&mut self, key: &KeyRef, version: &KeyVersion) -> Result<(), RotationError> {
        if self.is_retired(key, version) {
            return Ok(());
        }
        let busy = self.users_of(|e| &e.key == key && &e.version == version);
        if busy > 0 {
            return Err(RotationError::InUse(busy));
        }
        self.retired.insert((key.clone(), version.clone()));
        Ok(())
    }

    /// Whether a key version was retired.
    pub fn is_retired(&self, key: &KeyRef, version: &KeyVersion) -> bool {
        self.retired.contains(&(key.clone(), version.clone()))
    }

    /// A restart: staged and verified overlays are lost; what was published and retired persists, and so do
    /// pinned roots. A root is a durable record (the pin kind in `formats.md`), not a process: readers on
    /// other nodes survive this node's restart, so a restart must never make a version look unused.
    pub fn crash(&mut self) {
        for state in self.objects.values_mut() {
            state.staged = None;
        }
    }
}

/// Why a policy check refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateError {
    /// No policy was ever accepted: fail closed.
    NeverAccepted,
    /// The policy is older than its allowed age: fail closed.
    Stale,
    /// The accepted policy is a revocation.
    Revoked,
    /// A lower generation than the one held, a changed meaning for the same generation, or a clock that
    /// moved backwards.
    Regressed,
}

struct Held {
    generation: u64,
    revoked: bool,
    refreshed_at: Duration,
}

/// Policy freshness for one index.
pub struct PolicyGate {
    max_age: Duration,
    held: Option<Held>,
}

impl PolicyGate {
    /// A gate that accepts a policy for at most `max_age` after it was last delivered.
    pub fn new(max_age: Duration) -> Self {
        Self {
            max_age,
            held: None,
        }
    }

    /// Accepts a policy `generation` delivered at monotonic time `now`. A higher generation replaces the
    /// held one; the same generation with the same meaning only refreshes it; anything else is refused.
    pub fn accept(
        &mut self,
        generation: u64,
        revoked: bool,
        now: Duration,
    ) -> Result<(), GateError> {
        if let Some(held) = &mut self.held {
            if generation < held.generation {
                return Err(GateError::Regressed);
            }
            if generation == held.generation {
                if revoked != held.revoked {
                    return Err(GateError::Regressed);
                }
                if now < held.refreshed_at {
                    return Err(GateError::Regressed);
                }
                held.refreshed_at = now;
                return Ok(());
            }
        }
        self.held = Some(Held {
            generation,
            revoked,
            refreshed_at: now,
        });
        Ok(())
    }

    /// The generation operations may run under at `now`, or why none may. A clock earlier than the last
    /// delivery is not trusted: it is `Stale`, never fresh, so a backwards step cannot extend freshness.
    /// `now` must come from the node's own monotonic clock, never from a message: the gate trusts its caller
    /// for the time and for the authenticity of the channel a policy arrived on (M05.01's job).
    pub fn check(&self, now: Duration) -> Result<u64, GateError> {
        let held = self.held.as_ref().ok_or(GateError::NeverAccepted)?;
        if held.revoked {
            return Err(GateError::Revoked);
        }
        match now.checked_sub(held.refreshed_at) {
            Some(age) if age <= self.max_age => {}
            _ => return Err(GateError::Stale),
        }
        Ok(held.generation)
    }
}

/// The longest a revoked index can keep being served by a node that never received the revocation:
/// `max_age` when every operation consults the [`PolicyGate`], otherwise the data-key cache lifetime. This
/// is a bound derived from those two settings, not a measurement of a channel.
pub fn revocation_bound(max_age: Duration, cache_ttl: Duration, consults_gate: bool) -> Duration {
    if consults_gate { max_age } else { cache_ttl }
}

/// Whether the bound meets a revocation target such as the plan's 60 seconds.
pub fn meets_target(bound: Duration, target: Duration) -> bool {
    bound <= target
}
