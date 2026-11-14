//! Directed traffic attribution and pricing (M03.07).
//!
//! Bytes are recorded per (role, direction, endpoint, path class, payer), each transfer once
//! however many ends report it. A path whose region, zone, payer or evidence is missing is
//! `Unknown` and is priced as unknown, never as free. Everything is bounded: distinct keys,
//! distinct transfers, tenants and string lengths have fixed ceilings with an overflow bucket.

use crate::ledger::{Ledger, Role, Verb};
use std::collections::BTreeMap;

/// Most distinct (role, direction, endpoint, class, payer) keys kept.
pub const MAX_KEYS: usize = 256;
/// Most distinct transfer ids remembered for de-duplication.
pub const MAX_TRANSFERS: usize = 4096;
/// Most tenants with their own drill-down row; the rest share one `other` row.
pub const MAX_TENANTS: usize = 64;
/// Longest endpoint or tenant name accepted.
pub const MAX_NAME: usize = 128;

/// Which way bytes moved relative to this process.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Direction {
    /// Sent by this process.
    Egress,
    /// Received by this process.
    Ingress,
}

/// Who the provider bills for a transfer.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Payer {
    /// This deployment.
    Us,
    /// The peer (for example the object-store provider, or another tenant's account).
    Peer,
}

/// How a path was established.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Evidence {
    /// Only a DNS answer. This proves nothing about the route and fails acceptance.
    DnsOnly,
    /// Stated by configuration, not observed.
    Declared,
    /// A route, endpoint or gateway inspection of the infrastructure.
    RouteInspected,
    /// Seen in transport logs.
    TransportLog,
}

impl Evidence {
    fn accepted(self) -> bool {
        matches!(self, Evidence::RouteInspected | Evidence::TransportLog)
    }
}

/// The middle of a path that changes its price.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Via {
    /// No gateway.
    Direct,
    /// Through a NAT gateway.
    Nat,
    /// Out through a NAT gateway and back into the same zone.
    Hairpin,
}

/// A region and zone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Place {
    /// Region name.
    pub region: String,
    /// Zone name.
    pub zone: String,
}

/// What is known about the path to one endpoint. A `None` is unknown, not default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Path {
    /// Endpoint name.
    pub endpoint: String,
    /// Region of the endpoint.
    pub region: Option<String>,
    /// Zone of the endpoint.
    pub zone: Option<String>,
    /// Who pays for traffic on this path.
    pub payer: Option<Payer>,
    /// Gateway in the middle.
    pub via: Via,
    /// How the path was established.
    pub evidence: Evidence,
}

/// The price class of a path.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Class {
    /// Same region and zone.
    SameZone,
    /// Same region, different zone.
    CrossZone,
    /// Different region.
    CrossRegion,
    /// Through a NAT gateway.
    NatProcessing,
    /// Out through a NAT gateway and back into the same zone.
    NatHairpin,
    /// Something needed to price it is missing or unverified.
    Unknown,
}

impl Path {
    /// Price class from `local`. Unverified evidence or a missing field is `Unknown`.
    #[must_use]
    pub fn class(&self, local: &Place) -> Class {
        if !self.evidence.accepted() {
            return Class::Unknown;
        }
        match self.via {
            Via::Nat => return Class::NatProcessing,
            Via::Hairpin => return Class::NatHairpin,
            Via::Direct => {}
        }
        match (&self.region, &self.zone) {
            (Some(region), _) if *region != local.region => Class::CrossRegion,
            (Some(_), Some(zone)) if *zone != local.zone => Class::CrossZone,
            (Some(_), Some(_)) => Class::SameZone,
            _ => Class::Unknown,
        }
    }
}

/// A price expressed as micro-dollars per a stated number of bytes, so the unit is part of the value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rate {
    micro_usd: u64,
    per_bytes: u64,
}

/// The byte unit a provider quotes a rate in. `GB` is 10^9 bytes and `GiB` is 2^30.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unit {
    /// 10^9 bytes.
    GB,
    /// 2^30 bytes.
    GiB,
}

impl Rate {
    /// `micro_usd` per one `unit`.
    #[must_use]
    pub fn per(micro_usd: u64, unit: Unit) -> Self {
        Rate {
            micro_usd,
            per_bytes: match unit {
                Unit::GB => 1_000_000_000,
                Unit::GiB => 1 << 30,
            },
        }
    }

    /// Cost of `bytes`, rounded up to a whole micro-dollar.
    #[must_use]
    pub fn cost(self, bytes: u64) -> u128 {
        let numerator = u128::from(bytes) * u128::from(self.micro_usd);
        numerator.div_ceil(u128::from(self.per_bytes))
    }
}

/// Rates per path class. A class with no rate is unknown, never free.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rates {
    /// Same zone.
    pub same_zone: Option<Rate>,
    /// Cross zone.
    pub cross_zone: Option<Rate>,
    /// Cross region.
    pub cross_region: Option<Rate>,
    /// NAT processing.
    pub nat_processing: Option<Rate>,
    /// NAT hairpin.
    pub nat_hairpin: Option<Rate>,
}

impl Rates {
    fn of(&self, class: Class) -> Option<Rate> {
        match class {
            Class::SameZone => self.same_zone,
            Class::CrossZone => self.cross_zone,
            Class::CrossRegion => self.cross_region,
            Class::NatProcessing => self.nat_processing,
            Class::NatHairpin => self.nat_hairpin,
            Class::Unknown => None,
        }
    }
}

/// Why a transfer was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrafficError {
    /// An endpoint or tenant name is empty or longer than [`MAX_NAME`].
    BadName,
    /// [`MAX_TRANSFERS`] distinct transfers are already remembered.
    TransferTableFull,
}

/// One transfer as reported by one end.
#[derive(Clone, Copy, Debug)]
pub struct Transfer<'a> {
    /// Identity shared by every end that reports this transfer.
    pub id: u64,
    /// What the bytes were for.
    pub role: Role,
    /// Direction at this process.
    pub direction: Direction,
    /// Path to the endpoint.
    pub path: &'a Path,
    /// Where this process runs.
    pub local: &'a Place,
    /// Tenant the work was done for, when known.
    pub tenant: Option<&'a str>,
    /// Encoded bytes.
    pub bytes: u64,
}

/// A tenant's totals.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Totals {
    /// Transfers attributed.
    pub transfers: u64,
    /// Encoded bytes attributed.
    pub bytes: u64,
}

type Key = (Role, Direction, String, Class, Option<Payer>);

/// Why acceptance fails.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejection {
    /// A path was established by DNS alone.
    DnsOnly,
    /// This many bytes travelled a path that is unknown or unverified.
    UnknownPath(u64),
}

/// The priced result.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Priced {
    /// Micro-dollars for traffic this deployment pays and has a rate for.
    pub known_micro_usd: u128,
    /// Bytes that could not be priced: unknown path, unknown payer, missing rate, or overflow.
    pub unknown_bytes: u64,
    /// Bytes the peer pays for, recorded but not our cost.
    pub peer_paid_bytes: u64,
}

impl Priced {
    /// True only when every byte was priced or belongs to the peer.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.unknown_bytes == 0
    }
}

/// Directed traffic by role, endpoint, class and payer, with bounded tenant drill-down.
#[derive(Clone, Debug, Default)]
pub struct TrafficLedger {
    entries: BTreeMap<Key, u64>,
    seen: BTreeMap<u64, u64>,
    overflow_bytes: u64,
    disagreements: u64,
    dns_only_bytes: u64,
    tenants: BTreeMap<String, Totals>,
    other: Totals,
}

impl TrafficLedger {
    /// Record one transfer. A transfer id seen before is not counted again: the same bytes
    /// reported by both ends are one charge. A repeat with different bytes is counted in
    /// [`Self::disagreements`].
    pub fn record(&mut self, t: Transfer<'_>) -> Result<(), TrafficError> {
        if let Some(first) = self.seen.get(&t.id) {
            self.disagreements += u64::from(*first != t.bytes);
            return Ok(());
        }
        if self.seen.len() >= MAX_TRANSFERS {
            self.refuse(t.bytes);
            return Err(TrafficError::TransferTableFull);
        }
        self.add(t)?;
        self.seen.insert(t.id, t.bytes);
        Ok(())
    }

    /// Record a transfer known to be reported once, such as one this process observed itself, so
    /// no identity is remembered and the de-duplication table cannot fill. Refused bytes are kept
    /// as unknown traffic, never dropped.
    pub fn record_unique(&mut self, t: Transfer<'_>) -> Result<(), TrafficError> {
        self.add(t)
    }

    fn refuse(&mut self, bytes: u64) {
        self.overflow_bytes = self.overflow_bytes.saturating_add(bytes);
    }

    fn add(&mut self, t: Transfer<'_>) -> Result<(), TrafficError> {
        let named = |s: &str| !s.is_empty() && s.len() <= MAX_NAME;
        if !named(&t.path.endpoint) || t.tenant.is_some_and(|n| !named(n)) {
            self.refuse(t.bytes);
            return Err(TrafficError::BadName);
        }
        if t.path.evidence == Evidence::DnsOnly {
            self.dns_only_bytes = self.dns_only_bytes.saturating_add(t.bytes);
        }
        let key = (
            t.role,
            t.direction,
            t.path.endpoint.clone(),
            t.path.class(t.local),
            t.path.payer,
        );
        if self.entries.contains_key(&key) || self.entries.len() < MAX_KEYS {
            let slot = self.entries.entry(key).or_default();
            *slot = slot.saturating_add(t.bytes);
        } else {
            self.overflow_bytes = self.overflow_bytes.saturating_add(t.bytes);
        }
        if let Some(name) = t.tenant {
            let row = if self.tenants.contains_key(name) || self.tenants.len() < MAX_TENANTS {
                self.tenants.entry(name.to_owned()).or_default()
            } else {
                &mut self.other
            };
            row.transfers += 1;
            row.bytes = row.bytes.saturating_add(t.bytes);
        }
        Ok(())
    }

    /// Transfers reported again with different bytes.
    #[must_use]
    pub fn disagreements(&self) -> u64 {
        self.disagreements
    }

    /// Bytes in a (role, class) cell, summed over endpoints, directions and payers.
    #[must_use]
    pub fn bytes(&self, role: Role, class: Class) -> u64 {
        self.entries
            .iter()
            .filter(|((r, _, _, c, _), _)| *r == role && *c == class)
            .map(|(_, b)| *b)
            .sum()
    }

    /// Bytes beyond the key ceiling or refused for a bad name or a full table, kept only as an
    /// unpriceable total.
    #[must_use]
    pub fn overflow_bytes(&self) -> u64 {
        self.overflow_bytes
    }

    /// One tenant's totals. The caller names the tenant it is scoped to; no other tenant's row is
    /// reachable through this ledger.
    #[must_use]
    pub fn tenant(&self, scope: &str) -> Option<Totals> {
        self.tenants.get(scope).copied()
    }

    /// Totals of tenants beyond [`MAX_TENANTS`].
    #[must_use]
    pub fn other_tenants(&self) -> Totals {
        self.other
    }

    /// Price the ledger. Only traffic this deployment pays is priced; the rest is counted apart.
    #[must_use]
    pub fn price(&self, rates: &Rates) -> Priced {
        let mut out = Priced {
            unknown_bytes: self.overflow_bytes,
            ..Priced::default()
        };
        for ((_, _, _, class, payer), bytes) in &self.entries {
            match (payer, rates.of(*class)) {
                (Some(Payer::Peer), _) => {
                    out.peer_paid_bytes = out.peer_paid_bytes.saturating_add(*bytes);
                }
                (Some(Payer::Us), Some(rate)) => {
                    out.known_micro_usd = out.known_micro_usd.saturating_add(rate.cost(*bytes));
                }
                _ => out.unknown_bytes = out.unknown_bytes.saturating_add(*bytes),
            }
        }
        out
    }

    /// Acceptance of the recorded paths: a DNS-only path or any unknown path fails.
    pub fn acceptance(&self) -> Result<(), Rejection> {
        if self.dns_only_bytes > 0 {
            return Err(Rejection::DnsOnly);
        }
        let unknown: u64 = self
            .entries
            .iter()
            .filter(|((_, _, _, class, _), _)| *class == Class::Unknown)
            .map(|(_, b)| *b)
            .fold(self.overflow_bytes, u64::saturating_add);
        match unknown {
            0 => Ok(()),
            bytes => Err(Rejection::UnknownPath(bytes)),
        }
    }
}

/// A verb whose adapter count and independently observed count differ.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mismatch {
    /// The verb.
    pub verb: Verb,
    /// Attempts the adapter's ledger recorded.
    pub ledger: u64,
    /// Requests an independent source (transport log, provider usage) saw.
    pub observed: u64,
}

/// Compare the ledger's attempts with an independently observed list of requests. An empty result
/// means every verb agrees exactly.
#[must_use]
pub fn reconcile(ledger: &Ledger, observed: &[Verb]) -> Vec<Mismatch> {
    [
        Verb::Get,
        Verb::Put,
        Verb::Head,
        Verb::List,
        Verb::Delete,
        Verb::Post,
    ]
    .into_iter()
    .filter_map(|verb| {
        let seen = observed.iter().filter(|v| **v == verb).count() as u64;
        let recorded = ledger.attempts_by_verb(verb);
        (seen != recorded).then_some(Mismatch {
            verb,
            ledger: recorded,
            observed: seen,
        })
    })
    .collect()
}

/// Encoded size of an HTTP/1.1 request: the request line, every header line and the body.
#[must_use]
pub fn encoded_request_bytes(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: u64,
) -> u64 {
    let line = method.len() + 1 + target.len() + " HTTP/1.1\r\n".len();
    let head: usize = headers.iter().map(|(n, v)| n.len() + 2 + v.len() + 2).sum();
    (line + head + 2) as u64 + body
}

/// Encoded size of an HTTP/1.1 response whose reason phrase is unknown (counted as empty): the status
/// line, every header line and the body. A lower bound on the wire size, computed not read.
#[must_use]
pub fn encoded_response_bytes(headers: &[(String, String)], body: u64) -> u64 {
    let head: usize = headers.iter().map(|(n, v)| n.len() + 2 + v.len() + 2).sum();
    ("HTTP/1.1 200 \r\n".len() + head + 2) as u64 + body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Outcome;

    fn home() -> Place {
        Place {
            region: "r1".into(),
            zone: "a".into(),
        }
    }

    fn path(
        endpoint: &str,
        region: Option<&str>,
        zone: Option<&str>,
        via: Via,
        ev: Evidence,
    ) -> Path {
        Path {
            endpoint: endpoint.into(),
            region: region.map(Into::into),
            zone: zone.map(Into::into),
            payer: Some(Payer::Us),
            via,
            evidence: ev,
        }
    }

    fn transfer<'a>(id: u64, p: &'a Path, local: &'a Place, bytes: u64) -> Transfer<'a> {
        Transfer {
            id,
            role: Role::Payload,
            direction: Direction::Egress,
            path: p,
            local,
            tenant: None,
            bytes,
        }
    }

    #[test]
    fn path_classes_follow_region_zone_gateway_and_evidence() {
        let l = home();
        let seen = Evidence::TransportLog;
        let cases = [
            (
                path("e", Some("r1"), Some("a"), Via::Direct, seen),
                Class::SameZone,
            ),
            (
                path("e", Some("r1"), Some("b"), Via::Direct, seen),
                Class::CrossZone,
            ),
            (
                path("e", Some("r2"), Some("a"), Via::Direct, seen),
                Class::CrossRegion,
            ),
            (
                path(
                    "e",
                    Some("r1"),
                    Some("a"),
                    Via::Nat,
                    Evidence::RouteInspected,
                ),
                Class::NatProcessing,
            ),
            (
                path("e", Some("r1"), Some("a"), Via::Hairpin, seen),
                Class::NatHairpin,
            ),
            (
                path("e", Some("r1"), None, Via::Direct, seen),
                Class::Unknown,
            ),
            (
                path("e", None, Some("a"), Via::Direct, seen),
                Class::Unknown,
            ),
            (
                path("e", Some("r1"), Some("a"), Via::Direct, Evidence::DnsOnly),
                Class::Unknown,
            ),
            (
                path("e", Some("r1"), Some("a"), Via::Direct, Evidence::Declared),
                Class::Unknown,
            ),
            (
                path("e", Some("r1"), Some("a"), Via::Nat, Evidence::DnsOnly),
                Class::Unknown,
            ),
        ];
        for (p, want) in cases {
            assert_eq!(p.class(&l), want, "{p:?}");
        }
    }

    #[test]
    fn a_transfer_reported_by_both_ends_is_one_charge() {
        let l = home();
        let p = path(
            "store",
            Some("r1"),
            Some("b"),
            Via::Direct,
            Evidence::TransportLog,
        );
        let mut ledger = TrafficLedger::default();
        ledger.record(transfer(7, &p, &l, 1_000)).unwrap();
        ledger.record(transfer(7, &p, &l, 1_000)).unwrap(); // the other end's report
        assert_eq!(ledger.bytes(Role::Payload, Class::CrossZone), 1_000);
        assert_eq!(ledger.disagreements(), 0);
        ledger.record(transfer(7, &p, &l, 1_200)).unwrap(); // an end that disagrees
        assert_eq!(ledger.bytes(Role::Payload, Class::CrossZone), 1_000);
        assert_eq!(ledger.disagreements(), 1);
    }

    #[test]
    fn rates_carry_their_unit_so_gb_is_not_read_as_gib() {
        // 1 GiB at a "per GB" rate costs more than 1 GB does, by exactly 2^30 / 10^9.
        let per_gb = Rate::per(10_000, Unit::GB);
        let per_gib = Rate::per(10_000, Unit::GiB);
        assert_eq!(per_gb.cost(1_000_000_000), 10_000);
        assert_eq!(per_gib.cost(1 << 30), 10_000);
        assert_eq!(per_gb.cost(1 << 30), 10_738); // 10_737.4 rounded up
        assert_eq!(per_gib.cost(1_000_000_000), 9_314); // 9_313.2 rounded up
        assert_eq!(per_gb.cost(0), 0);
        assert_eq!(per_gb.cost(1), 1); // rounds up: a request is never free
    }

    #[test]
    fn missing_rates_paths_and_payers_are_unknown_never_free() {
        let l = home();
        let mut ledger = TrafficLedger::default();
        let known = path(
            "s",
            Some("r1"),
            Some("b"),
            Via::Direct,
            Evidence::TransportLog,
        );
        let no_zone = path("z", Some("r1"), None, Via::Direct, Evidence::TransportLog);
        let mut no_payer = path(
            "p",
            Some("r1"),
            Some("b"),
            Via::Direct,
            Evidence::TransportLog,
        );
        no_payer.payer = None;
        let mut peer = known.clone();
        peer.payer = Some(Payer::Peer);
        peer.endpoint = "peer".into();
        for (id, p, bytes) in [
            (1, &known, 1u64 << 30),
            (2, &no_zone, 500),
            (3, &no_payer, 700),
            (4, &peer, 900),
        ] {
            ledger.record(transfer(id, p, &l, bytes)).unwrap();
        }
        let rates = Rates {
            cross_zone: Some(Rate::per(10_000, Unit::GiB)),
            ..Rates::default()
        };
        let priced = ledger.price(&rates);
        assert_eq!(priced.known_micro_usd, 10_000);
        assert_eq!(priced.unknown_bytes, 500 + 700);
        assert_eq!(priced.peer_paid_bytes, 900);
        assert!(!priced.complete());
        // Without the cross-zone rate the same bytes are unknown, not zero cost.
        let none = ledger.price(&Rates::default());
        assert_eq!(none.known_micro_usd, 0);
        assert_eq!(none.unknown_bytes, (1 << 30) + 500 + 700);
    }

    #[test]
    fn dns_only_paths_and_unknown_paths_fail_acceptance() {
        let l = home();
        let good = path(
            "s",
            Some("r1"),
            Some("a"),
            Via::Direct,
            Evidence::RouteInspected,
        );
        let mut ledger = TrafficLedger::default();
        ledger.record(transfer(1, &good, &l, 10)).unwrap();
        assert_eq!(ledger.acceptance(), Ok(()));
        let dns = path("d", Some("r1"), Some("a"), Via::Direct, Evidence::DnsOnly);
        ledger.record(transfer(2, &dns, &l, 10)).unwrap();
        assert_eq!(ledger.acceptance(), Err(Rejection::DnsOnly));
        let mut declared_only = TrafficLedger::default();
        let declared = path("c", Some("r1"), Some("a"), Via::Direct, Evidence::Declared);
        declared_only
            .record(transfer(1, &declared, &l, 25))
            .unwrap();
        assert_eq!(declared_only.acceptance(), Err(Rejection::UnknownPath(25)));
    }

    #[test]
    fn nat_and_hairpin_paths_are_priced_apart_from_direct() {
        let l = home();
        let nat = path(
            "n",
            Some("r1"),
            Some("a"),
            Via::Nat,
            Evidence::RouteInspected,
        );
        let hairpin = path(
            "h",
            Some("r1"),
            Some("a"),
            Via::Hairpin,
            Evidence::RouteInspected,
        );
        let mut ledger = TrafficLedger::default();
        ledger.record(transfer(1, &nat, &l, 1_000_000_000)).unwrap();
        ledger
            .record(transfer(2, &hairpin, &l, 1_000_000_000))
            .unwrap();
        let rates = Rates {
            nat_processing: Some(Rate::per(45_000, Unit::GB)),
            nat_hairpin: None,
            ..Rates::default()
        };
        let priced = ledger.price(&rates);
        assert_eq!(priced.known_micro_usd, 45_000);
        assert_eq!(priced.unknown_bytes, 1_000_000_000);
    }

    #[test]
    fn keys_transfers_names_and_tenants_are_bounded() {
        let l = home();
        let mut ledger = TrafficLedger::default();
        for i in 0..(MAX_KEYS as u64 + 10) {
            let p = path(
                &format!("e{i}"),
                Some("r1"),
                Some("a"),
                Via::Direct,
                Evidence::TransportLog,
            );
            ledger.record(transfer(i, &p, &l, 1)).unwrap();
        }
        assert_eq!(ledger.overflow_bytes(), 10);
        assert_eq!(
            ledger.price(&Rates::default()).unknown_bytes,
            10 + MAX_KEYS as u64
        );
        assert_eq!(ledger.acceptance(), Err(Rejection::UnknownPath(10)));
        let long = path(
            &"x".repeat(MAX_NAME + 1),
            Some("r1"),
            Some("a"),
            Via::Direct,
            Evidence::TransportLog,
        );
        assert_eq!(
            ledger.record(transfer(9_999, &long, &l, 1)),
            Err(TrafficError::BadName)
        );
        let empty = path(
            "",
            Some("r1"),
            Some("a"),
            Via::Direct,
            Evidence::TransportLog,
        );
        assert_eq!(
            ledger.record(transfer(9_998, &empty, &l, 1)),
            Err(TrafficError::BadName)
        );
        let p = path(
            "e0",
            Some("r1"),
            Some("a"),
            Via::Direct,
            Evidence::TransportLog,
        );
        let mut t = transfer(9_997, &p, &l, 1);
        t.tenant = Some("");
        assert_eq!(ledger.record(t), Err(TrafficError::BadName));

        let mut tenants = TrafficLedger::default();
        for i in 0..(MAX_TENANTS as u64 + 5) {
            let mut t = transfer(i, &p, &l, 10);
            let name = format!("t{i}");
            t.tenant = Some(&name);
            tenants.record(t).unwrap();
        }
        assert_eq!(
            tenants.tenant("t0"),
            Some(Totals {
                transfers: 1,
                bytes: 10
            })
        );
        assert_eq!(tenants.tenant(&format!("t{}", MAX_TENANTS)), None);
        assert_eq!(
            tenants.other_tenants(),
            Totals {
                transfers: 5,
                bytes: 50
            }
        );
        // An existing tenant keeps accumulating after the ceiling is reached.
        let mut t = transfer(10_000, &p, &l, 5);
        t.tenant = Some("t0");
        tenants.record(t).unwrap();
        assert_eq!(
            tenants.tenant("t0"),
            Some(Totals {
                transfers: 2,
                bytes: 15
            })
        );

        let mut full = TrafficLedger::default();
        for i in 0..MAX_TRANSFERS as u64 {
            full.record(transfer(i, &p, &l, 1)).unwrap();
        }
        assert_eq!(
            full.record(transfer(u64::MAX, &p, &l, 1)),
            Err(TrafficError::TransferTableFull)
        );
        // A transfer already remembered is still accepted when the table is full.
        assert_eq!(full.record(transfer(0, &p, &l, 1)), Ok(()));
    }

    #[test]
    fn refused_transfers_stay_in_the_ledger_as_unknown_bytes() {
        let l = home();
        let ok = path(
            "e",
            Some("r1"),
            Some("a"),
            Via::Direct,
            Evidence::TransportLog,
        );
        let mut ledger = TrafficLedger::default();
        let empty = path(
            "",
            Some("r1"),
            Some("a"),
            Via::Direct,
            Evidence::TransportLog,
        );
        assert_eq!(
            ledger.record(transfer(1, &empty, &l, 40)),
            Err(TrafficError::BadName)
        );
        assert_eq!(
            ledger.record_unique(transfer(2, &empty, &l, 2)),
            Err(TrafficError::BadName)
        );
        assert_eq!(ledger.overflow_bytes(), 42);
        assert_eq!(ledger.acceptance(), Err(Rejection::UnknownPath(42)));
        assert!(!ledger.price(&Rates::default()).complete());
        // A full de-duplication table refuses `record` but not `record_unique`, and keeps the bytes.
        let mut full = TrafficLedger::default();
        for i in 0..MAX_TRANSFERS as u64 {
            full.record(transfer(i, &ok, &l, 1)).unwrap();
        }
        assert_eq!(
            full.record(transfer(u64::MAX, &ok, &l, 7)),
            Err(TrafficError::TransferTableFull)
        );
        assert_eq!(full.overflow_bytes(), 7);
        full.record_unique(transfer(u64::MAX, &ok, &l, 5)).unwrap();
        assert_eq!(
            full.bytes(Role::Payload, Class::SameZone),
            MAX_TRANSFERS as u64 + 5
        );
    }

    #[test]
    fn peer_paid_bytes_on_an_unknown_path_are_peer_cost_but_still_fail_acceptance() {
        let l = home();
        let mut peer = path("p", Some("r1"), None, Via::Direct, Evidence::TransportLog);
        peer.payer = Some(Payer::Peer);
        let mut ledger = TrafficLedger::default();
        ledger.record(transfer(1, &peer, &l, 30)).unwrap();
        let priced = ledger.price(&Rates::default());
        assert_eq!((priced.peer_paid_bytes, priced.unknown_bytes), (30, 0));
        assert_eq!(ledger.acceptance(), Err(Rejection::UnknownPath(30)));
    }

    #[test]
    fn reconciliation_reports_every_verb_that_disagrees() {
        let mut ledger = Ledger::default();
        ledger.attempt(Role::Payload, Verb::Put, Outcome::Ok, 1, 1, false);
        ledger.attempt(Role::Payload, Verb::Put, Outcome::Throttled, 1, 1, true);
        ledger.attempt(Role::Metadata, Verb::List, Outcome::Ok, 1, 1, false);
        assert!(reconcile(&ledger, &[Verb::Put, Verb::Put, Verb::List]).is_empty());
        // The transport log saw one fewer PUT (a retry the adapter counted twice) and an unlogged HEAD.
        let mismatches = reconcile(&ledger, &[Verb::Put, Verb::List, Verb::Head]);
        assert_eq!(
            mismatches,
            vec![
                Mismatch {
                    verb: Verb::Put,
                    ledger: 2,
                    observed: 1
                },
                Mismatch {
                    verb: Verb::Head,
                    ledger: 0,
                    observed: 1
                },
            ]
        );
    }

    #[test]
    fn encoded_sizes_count_every_line_and_the_body() {
        let headers = vec![
            ("host".to_owned(), "h".to_owned()),
            ("x".to_owned(), "yy".to_owned()),
        ];
        // "GET /k HTTP/1.1\r\n" 17 + "host: h\r\n" 9 + "x: yy\r\n" 7 + "\r\n" 2 + body 5
        assert_eq!(
            encoded_request_bytes("GET", "/k", &headers, 5),
            17 + 9 + 7 + 2 + 5
        );
        // "HTTP/1.1 200 \r\n" 15 + 9 + 7 + 2 + body 3
        assert_eq!(encoded_response_bytes(&headers, 3), 15 + 9 + 7 + 2 + 3);
    }
}
