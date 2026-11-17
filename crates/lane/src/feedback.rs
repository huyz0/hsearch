//! Load feedback (M13.02): how loaded each node says it is, turned into a placement weight.
//!
//! Each node reports its utilisation in parts per thousand. A report is smoothed (a quarter of the
//! new value, three quarters of the old) and mapped to a *factor*: 1000 up to the target
//! utilisation, falling linearly to `min_factor` at full. The factor only changes when the mapped
//! value moves by at least `deadband`, so a node hovering around the target does not flap between
//! weights and move tenants back and forth. A node that has not reported for `stale_ticks` counts
//! as neutral (1000): silence is membership's business, not a reason to move compute.
//!
//! Holds one small record per node that ever reported, bounded by `max_nodes`; it is a table of
//! the fleet, never of tenants.
use std::collections::HashMap;

use hsearch_format::Id;

/// Neutral factor: the node counts at its full capacity.
pub const NEUTRAL: u32 = 1000;

/// Tuning for one table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Utilisation (permille) at and below which a node keeps its full weight.
    pub target: u32,
    /// The factor at 1000 permille, between 1 and 1000.
    pub min_factor: u32,
    /// The smallest change in the mapped factor that is applied.
    pub deadband: u32,
    /// A report older than this many ticks is ignored.
    pub stale_ticks: u64,
    /// Most nodes tracked.
    pub max_nodes: usize,
}

/// A report that was not recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// The config is unusable.
    Invalid,
    /// The table is full and the node is new.
    Full,
}

struct Reading {
    smoothed: u32,
    factor: u32,
    tick: u64,
}

/// Placement factors from load reports.
pub struct LoadFeedback {
    config: Config,
    nodes: HashMap<Id, Reading>,
}

impl LoadFeedback {
    /// An empty table. [`Refusal::Invalid`] if the config is unusable.
    pub fn new(config: Config) -> Result<Self, Refusal> {
        if config.target >= 1000
            || config.min_factor == 0
            || config.min_factor > NEUTRAL
            || config.max_nodes == 0
            || config.stale_ticks == 0
        {
            return Err(Refusal::Invalid);
        }
        Ok(Self {
            config,
            nodes: HashMap::new(),
        })
    }

    /// Record `node`'s utilisation (clamped to 1000) at `tick`.
    pub fn report(&mut self, node: Id, utilization: u32, tick: u64) -> Result<(), Refusal> {
        let utilization = utilization.min(1000);
        if !self.nodes.contains_key(&node) && self.nodes.len() >= self.config.max_nodes {
            return Err(Refusal::Full);
        }
        let config = self.config;
        let reading = self.nodes.entry(node).or_insert(Reading {
            smoothed: utilization,
            factor: map(config, utilization),
            tick,
        });
        let stale = tick.saturating_sub(reading.tick) > config.stale_ticks;
        // A first report after silence starts from the new value, not from an old one.
        reading.smoothed = if stale {
            utilization
        } else {
            (reading.smoothed * 3 + utilization) / 4
        };
        let mapped = map(config, reading.smoothed);
        if mapped.abs_diff(reading.factor) >= config.deadband || stale || mapped == NEUTRAL {
            reading.factor = mapped;
        }
        reading.tick = reading.tick.max(tick);
        Ok(())
    }

    /// The factor for `node` at `now`: [`NEUTRAL`] if it never reported or went quiet.
    #[must_use]
    pub fn factor(&self, node: &Id, now: u64) -> u32 {
        match self.nodes.get(node) {
            Some(r) if now.saturating_sub(r.tick) <= self.config.stale_ticks => r.factor,
            _ => NEUTRAL,
        }
    }

    /// Forget a node that left the fleet.
    pub fn remove(&mut self, node: &Id) {
        self.nodes.remove(node);
    }

    /// Nodes tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether no node is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

fn map(config: Config, utilization: u32) -> u32 {
    if utilization <= config.target {
        return NEUTRAL;
    }
    let over = utilization - config.target;
    let span = 1000 - config.target;
    let drop = (NEUTRAL - config.min_factor) * over / span;
    NEUTRAL - drop
}
