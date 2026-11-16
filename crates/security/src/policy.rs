//! Authorization: which verified principal may do what, to which index.
//!
//! Tenant = index, so a policy object is one index's: its grants, its quota and its key reference. There is
//! no policy that spans indexes and no wildcard grant. A policy carries a generation that only grows; a
//! policy that cannot be shown to be fresh is not used (fail closed).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use crate::identity::{Channel, Role, VerifiedPrincipal};
use crate::ids::{ClusterId, IndexId};
use crate::keys::KeyRef;

/// What a principal may do to one index through the public or admin API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Permission {
    /// Run queries.
    Search,
    /// Index, update and delete documents.
    Write,
    /// Change settings and mappings, delete the index.
    Manage,
    /// Read the index for replication or copy. Granted per index, per source.
    Replicate,
    /// Read diagnostics that expose internals.
    Debug,
}

impl Permission {
    /// The roles that may hold this permission at all. A grant to a role outside this list never takes effect.
    pub fn allowed_roles(self) -> &'static [Role] {
        match self {
            Permission::Search | Permission::Write => &[Role::Client],
            Permission::Manage | Permission::Debug => &[Role::Admin],
            Permission::Replicate => &[Role::Replication],
        }
    }
}

/// Every RPC on the peer port. Adding one is a compile error until [`PeerRpc::allowed_callers`] says who
/// may call it, and a test walks [`PeerRpc::ALL`] to keep the list complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeerRpc {
    /// Query one shard.
    ShardSearch,
    /// Apply a write to one index's journal lane.
    ShardWrite,
    /// Append to an index's journal.
    JournalAppend,
    /// Read a range of an index's journal.
    JournalFetch,
    /// Fetch a segment range to fill a node's cache.
    SegmentFetch,
    /// Exchange membership.
    Membership,
    /// Liveness probe.
    Heartbeat,
    /// Drain a node.
    Drain,
    /// Pull an index's data for replication.
    ReplicationPull,
}

impl PeerRpc {
    /// Every RPC.
    pub const ALL: [PeerRpc; 9] = [
        PeerRpc::ShardSearch,
        PeerRpc::ShardWrite,
        PeerRpc::JournalAppend,
        PeerRpc::JournalFetch,
        PeerRpc::SegmentFetch,
        PeerRpc::Membership,
        PeerRpc::Heartbeat,
        PeerRpc::Drain,
        PeerRpc::ReplicationPull,
    ];

    /// The roles that may call this RPC.
    pub fn allowed_callers(self) -> &'static [Role] {
        match self {
            PeerRpc::ShardSearch | PeerRpc::ShardWrite => &[Role::Coordinator],
            PeerRpc::JournalAppend | PeerRpc::JournalFetch => &[Role::IndexNode],
            PeerRpc::SegmentFetch => &[Role::IndexNode, Role::SearchNode],
            PeerRpc::Membership | PeerRpc::Heartbeat => {
                &[Role::IndexNode, Role::SearchNode, Role::Coordinator]
            }
            PeerRpc::Drain => &[Role::Coordinator],
            PeerRpc::ReplicationPull => &[Role::Replication],
        }
    }

    /// Whether the RPC names an index and so needs that index's policy.
    pub fn is_index_scoped(self) -> bool {
        !matches!(
            self,
            PeerRpc::Membership | PeerRpc::Heartbeat | PeerRpc::Drain
        )
    }
}

/// Per-index limits. Data only here; enforcement belongs to the write and query paths.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quota {
    /// Most bytes the index may store.
    pub max_stored_bytes: Option<u64>,
    /// Most bytes a second the index may ingest.
    pub max_write_bytes_per_sec: Option<u64>,
    /// Most queries a second.
    pub max_queries_per_sec: Option<u64>,
}

/// Why a policy was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyError {
    /// More grants than [`IndexPolicy::MAX_GRANTS`].
    TooManyGrants,
    /// The permission cannot be held by the principal's role, so the grant would silently do nothing.
    RoleCannotHold,
    /// The generation is not newer than the installed one.
    NotNewer {
        /// The generation already installed.
        installed: u64,
    },
    /// The store holds [`PolicyStore::MAX_INDEXES`] policies.
    StoreFull,
    /// The policy to refresh is not installed at that generation.
    NotInstalled,
}

/// One index's authorization policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexPolicy {
    index: IndexId,
    generation: u64,
    suspended: bool,
    grants: BTreeMap<String, BTreeSet<Permission>>,
    quota: Quota,
    key: Option<KeyRef>,
}

impl IndexPolicy {
    /// The most principals one index's policy names.
    pub const MAX_GRANTS: usize = 4096;

    /// An empty policy at `generation`.
    pub fn new(index: IndexId, generation: u64) -> Self {
        Self {
            index,
            generation,
            suspended: false,
            grants: BTreeMap::new(),
            quota: Quota::default(),
            key: None,
        }
    }

    /// Grants `permission` to the principal called `principal`.
    pub fn grant(mut self, principal: &str, permission: Permission) -> Result<Self, PolicyError> {
        if !self.grants.contains_key(principal) && self.grants.len() >= Self::MAX_GRANTS {
            return Err(PolicyError::TooManyGrants);
        }
        self.grants
            .entry(principal.to_string())
            .or_default()
            .insert(permission);
        Ok(self)
    }

    /// Sets the quota.
    pub fn with_quota(mut self, quota: Quota) -> Self {
        self.quota = quota;
        self
    }

    /// Sets the index's own key reference; without one the deployment default applies.
    pub fn with_key(mut self, key: KeyRef) -> Self {
        self.key = Some(key);
        self
    }

    /// Marks the index suspended: every operation is refused while it is, whoever asks.
    pub fn suspended(mut self, suspended: bool) -> Self {
        self.suspended = suspended;
        self
    }

    /// The index this policy governs.
    pub fn index(&self) -> &IndexId {
        &self.index
    }

    /// The generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The quota.
    pub fn quota(&self) -> Quota {
        self.quota
    }

    /// The index's own key reference, if it has one.
    pub fn key(&self) -> Option<&KeyRef> {
        self.key.as_ref()
    }

    fn allows(&self, principal: &str, permission: Permission) -> bool {
        self.grants
            .get(principal)
            .is_some_and(|p| p.contains(&permission))
    }
}

struct Entry {
    policy: IndexPolicy,
    fresh_at: SystemTime,
}

/// The policies a node currently holds, one per index, bounded.
pub struct PolicyStore {
    entries: RwLock<HashMap<IndexId, Arc<Entry>>>,
}

impl Default for PolicyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyStore {
    /// The most indexes whose policy one node holds at once. Beyond this the caller evicts an idle one.
    pub const MAX_INDEXES: usize = 200_000;

    /// An empty store.
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Installs `policy`, which `now` shows to be current. Generations only grow.
    pub fn install(&self, policy: IndexPolicy, now: SystemTime) -> Result<(), PolicyError> {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        match entries.get(policy.index()) {
            Some(old) if old.policy.generation >= policy.generation => {
                return Err(PolicyError::NotNewer {
                    installed: old.policy.generation,
                });
            }
            None if entries.len() >= Self::MAX_INDEXES => return Err(PolicyError::StoreFull),
            _ => {}
        }
        entries.insert(
            policy.index().clone(),
            Arc::new(Entry {
                policy,
                fresh_at: now,
            }),
        );
        Ok(())
    }

    /// Records that the policy at `generation` was re-checked against its source at `now`.
    pub fn refresh(
        &self,
        index: &IndexId,
        generation: u64,
        now: SystemTime,
    ) -> Result<(), PolicyError> {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        match entries.get(index) {
            Some(e) if e.policy.generation == generation => {
                let policy = e.policy.clone();
                entries.insert(
                    index.clone(),
                    Arc::new(Entry {
                        policy,
                        fresh_at: now,
                    }),
                );
                Ok(())
            }
            _ => Err(PolicyError::NotInstalled),
        }
    }

    /// Drops an index's policy (the index is gone, or the node is making room).
    pub fn evict(&self, index: &IndexId) {
        self.entries
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(index);
    }

    /// The installed generation of an index's policy.
    pub fn generation(&self, index: &IndexId) -> Option<u64> {
        self.entries
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(index)
            .map(|e| e.policy.generation)
    }

    /// A copy of an index's policy.
    pub fn get(&self, index: &IndexId) -> Option<IndexPolicy> {
        self.entries
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(index)
            .map(|e| e.policy.clone())
    }

    fn entry(&self, index: &IndexId) -> Option<Arc<Entry>> {
        self.entries
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(index)
            .cloned()
    }
}

/// Why an operation was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthzError {
    /// The principal's role may not do this.
    WrongRole,
    /// A peer RPC arrived over something other than mutual TLS (a token, however valid, is public).
    NotMutualTls,
    /// The principal belongs to another cluster and is not a replication consumer.
    ForeignCluster,
    /// An index-scoped operation without an index.
    IndexRequired,
    /// No policy for the index is held. Fail closed.
    NoPolicy,
    /// The policy is older than the freshness bound. Fail closed.
    PolicyStale,
    /// The index is suspended.
    IndexSuspended,
    /// The principal has no grant for this on this index.
    NotGranted,
}

impl fmt::Display for AuthzError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AuthzError::WrongRole => "role may not perform this operation",
            AuthzError::NotMutualTls => "peer operations require mutual TLS",
            AuthzError::ForeignCluster => "principal belongs to another cluster",
            AuthzError::IndexRequired => "operation needs an index",
            AuthzError::NoPolicy => "no policy held for the index",
            AuthzError::PolicyStale => "policy is past its freshness bound",
            AuthzError::IndexSuspended => "index is suspended",
            AuthzError::NotGranted => "not granted on this index",
        })
    }
}

impl std::error::Error for AuthzError {}

/// An allowed operation and the policy generation that allowed it (for audit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    /// The generation used, `None` for cluster-scoped operations.
    pub generation: Option<u64>,
}

/// Decides, per request, from a verified principal and the held policies.
pub struct Authorizer {
    local: ClusterId,
    store: Arc<PolicyStore>,
    max_age: Duration,
}

impl Authorizer {
    /// An authorizer for `local`, refusing any policy last shown fresh more than `max_age` ago.
    pub fn new(local: ClusterId, store: Arc<PolicyStore>, max_age: Duration) -> Self {
        Self {
            local,
            store,
            max_age,
        }
    }

    fn policy(&self, index: &IndexId, now: SystemTime) -> Result<Arc<Entry>, AuthzError> {
        let entry = self.store.entry(index).ok_or(AuthzError::NoPolicy)?;
        // A fresh time in the future (clock stepped back) is not trusted either: it must be within bound.
        let age = now
            .duration_since(entry.fresh_at)
            .unwrap_or_else(|e| e.duration());
        if age > self.max_age {
            return Err(AuthzError::PolicyStale);
        }
        if entry.policy.suspended {
            return Err(AuthzError::IndexSuspended);
        }
        Ok(entry)
    }

    /// Authorizes a call on the peer port.
    pub fn authorize_peer(
        &self,
        who: &VerifiedPrincipal,
        rpc: PeerRpc,
        index: Option<&IndexId>,
        now: SystemTime,
    ) -> Result<Decision, AuthzError> {
        if who.channel() != Channel::Mtls {
            return Err(AuthzError::NotMutualTls);
        }
        if !rpc.allowed_callers().contains(&who.role()) {
            return Err(AuthzError::WrongRole);
        }
        if who.cluster() != &self.local && who.role() != Role::Replication {
            return Err(AuthzError::ForeignCluster);
        }
        if !rpc.is_index_scoped() {
            return Ok(Decision { generation: None });
        }
        let index = index.ok_or(AuthzError::IndexRequired)?;
        let entry = self.policy(index, now)?;
        // Cluster members serve every index in the cluster; a replication consumer, which may be foreign,
        // is allowed only the indexes whose policy names it.
        if who.role() == Role::Replication
            && !entry.policy.allows(who.name(), Permission::Replicate)
        {
            return Err(AuthzError::NotGranted);
        }
        Ok(Decision {
            generation: Some(entry.policy.generation),
        })
    }

    /// Authorizes a public or admin API call against one index.
    pub fn authorize_api(
        &self,
        who: &VerifiedPrincipal,
        permission: Permission,
        index: &IndexId,
        now: SystemTime,
    ) -> Result<Decision, AuthzError> {
        if !permission.allowed_roles().contains(&who.role()) {
            return Err(AuthzError::WrongRole);
        }
        if who.cluster() != &self.local {
            return Err(AuthzError::ForeignCluster);
        }
        let entry = self.policy(index, now)?;
        if !entry.policy.allows(who.name(), permission) {
            return Err(AuthzError::NotGranted);
        }
        Ok(Decision {
            generation: Some(entry.policy.generation),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::from_secs(1_800_000_000);

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + T0 + Duration::from_secs(secs)
    }

    fn idx(name: &str) -> IndexId {
        IndexId::new(name).unwrap()
    }

    fn who(name: &str, cluster: &str, role: Role, channel: Channel) -> VerifiedPrincipal {
        VerifiedPrincipal::new(
            name.into(),
            ClusterId::new(cluster).unwrap(),
            None,
            role,
            channel,
        )
    }

    fn setup() -> (Authorizer, Arc<PolicyStore>) {
        let store = Arc::new(PolicyStore::new());
        let authz = Authorizer::new(
            ClusterId::new("prod").unwrap(),
            store.clone(),
            Duration::from_secs(60),
        );
        (authz, store)
    }

    #[test]
    fn every_peer_rpc_names_callers_and_only_peer_or_replication_roles() {
        let mut seen = BTreeSet::new();
        for rpc in PeerRpc::ALL {
            assert!(seen.insert(format!("{rpc:?}")), "duplicate in ALL");
            let callers = rpc.allowed_callers();
            assert!(!callers.is_empty(), "{rpc:?} has no caller");
            for role in callers {
                assert!(
                    role.is_peer() || *role == Role::Replication,
                    "{rpc:?} allows {role:?}"
                );
            }
        }
        // The exhaustive match below stops compiling when a variant is added without being listed in ALL.
        for rpc in PeerRpc::ALL {
            match rpc {
                PeerRpc::ShardSearch
                | PeerRpc::ShardWrite
                | PeerRpc::JournalAppend
                | PeerRpc::JournalFetch
                | PeerRpc::SegmentFetch
                | PeerRpc::Membership
                | PeerRpc::Heartbeat
                | PeerRpc::Drain
                | PeerRpc::ReplicationPull => {}
            }
        }
        assert_eq!(PeerRpc::ALL.len(), 9);
    }

    #[test]
    fn each_peer_rpc_refuses_every_role_not_listed_for_it() {
        let (authz, store) = setup();
        store
            .install(IndexPolicy::new(idx("orders"), 1), at(0))
            .unwrap();
        for rpc in PeerRpc::ALL {
            for role in Role::ALL {
                let p = who("spiffe://d/hsearch/prod/x", "prod", role, Channel::Mtls);
                let r = authz.authorize_peer(&p, rpc, Some(&idx("orders")), at(1));
                if rpc.allowed_callers().contains(&role) && role != Role::Replication {
                    assert!(r.is_ok(), "{rpc:?} should allow {role:?}: {r:?}");
                } else if rpc.allowed_callers().contains(&role) {
                    assert_eq!(
                        r,
                        Err(AuthzError::NotGranted),
                        "{rpc:?} replication needs a grant"
                    );
                } else {
                    assert_eq!(
                        r,
                        Err(AuthzError::WrongRole),
                        "{rpc:?} must refuse {role:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_public_token_never_reaches_the_peer_port() {
        let (authz, store) = setup();
        store
            .install(IndexPolicy::new(idx("orders"), 1), at(0))
            .unwrap();
        // Even a token labelled with a peer's name, holding a peer role, is not mutual TLS.
        for role in Role::ALL {
            let p = who("token:stolen", "prod", role, Channel::Token);
            for rpc in PeerRpc::ALL {
                assert_eq!(
                    authz.authorize_peer(&p, rpc, Some(&idx("orders")), at(1)),
                    Err(AuthzError::NotMutualTls)
                );
            }
        }
    }

    #[test]
    fn a_node_of_another_cluster_is_refused_unless_it_replicates() {
        let (authz, store) = setup();
        let policy = IndexPolicy::new(idx("orders"), 1)
            .grant(
                "spiffe://partner/hsearch/dr/replication",
                Permission::Replicate,
            )
            .unwrap();
        store.install(policy, at(0)).unwrap();
        let foreign = who(
            "spiffe://partner/hsearch/dr/coordinator",
            "dr",
            Role::Coordinator,
            Channel::Mtls,
        );
        assert_eq!(
            authz.authorize_peer(&foreign, PeerRpc::ShardSearch, Some(&idx("orders")), at(1)),
            Err(AuthzError::ForeignCluster)
        );
        let replica = who(
            "spiffe://partner/hsearch/dr/replication",
            "dr",
            Role::Replication,
            Channel::Mtls,
        );
        assert_eq!(
            authz.authorize_peer(
                &replica,
                PeerRpc::ReplicationPull,
                Some(&idx("orders")),
                at(1)
            ),
            Ok(Decision {
                generation: Some(1)
            })
        );
    }

    #[test]
    fn replication_is_scoped_to_the_index_that_grants_it() {
        let (authz, store) = setup();
        let who_dr = "spiffe://partner/hsearch/dr/replication";
        store
            .install(
                IndexPolicy::new(idx("orders"), 1)
                    .grant(who_dr, Permission::Replicate)
                    .unwrap(),
                at(0),
            )
            .unwrap();
        store
            .install(IndexPolicy::new(idx("payroll"), 1), at(0))
            .unwrap();
        let p = who(who_dr, "dr", Role::Replication, Channel::Mtls);
        assert!(
            authz
                .authorize_peer(&p, PeerRpc::ReplicationPull, Some(&idx("orders")), at(1))
                .is_ok()
        );
        // Tenant spoof: the grant on `orders` does not carry over to `payroll`, nor to an index with no policy.
        assert_eq!(
            authz.authorize_peer(&p, PeerRpc::ReplicationPull, Some(&idx("payroll")), at(1)),
            Err(AuthzError::NotGranted)
        );
        assert_eq!(
            authz.authorize_peer(&p, PeerRpc::ReplicationPull, Some(&idx("unknown")), at(1)),
            Err(AuthzError::NoPolicy)
        );
        // And an index-scoped RPC without an index is not allowed to fall back to "any".
        assert_eq!(
            authz.authorize_peer(&p, PeerRpc::ReplicationPull, None, at(1)),
            Err(AuthzError::IndexRequired)
        );
    }

    #[test]
    fn cluster_scoped_rpcs_need_no_index_or_policy() {
        let (authz, _store) = setup();
        let p = who(
            "spiffe://d/hsearch/prod/c",
            "prod",
            Role::Coordinator,
            Channel::Mtls,
        );
        assert_eq!(
            authz.authorize_peer(&p, PeerRpc::Drain, None, at(1)),
            Ok(Decision { generation: None })
        );
    }

    #[test]
    fn api_permissions_are_per_index_and_per_role() {
        let (authz, store) = setup();
        let alice = "token:alice";
        store
            .install(
                IndexPolicy::new(idx("orders"), 1)
                    .grant(alice, Permission::Search)
                    .unwrap()
                    .grant(alice, Permission::Manage)
                    .unwrap(),
                at(0),
            )
            .unwrap();
        store
            .install(IndexPolicy::new(idx("payroll"), 1), at(0))
            .unwrap();
        let client = who(alice, "prod", Role::Client, Channel::Token);
        assert!(
            authz
                .authorize_api(&client, Permission::Search, &idx("orders"), at(1))
                .is_ok()
        );
        // Granted Manage in the policy, but a client role cannot hold it: the grant does nothing.
        assert_eq!(
            authz.authorize_api(&client, Permission::Manage, &idx("orders"), at(1)),
            Err(AuthzError::WrongRole)
        );
        // Not granted Write, and not granted anything on another index.
        assert_eq!(
            authz.authorize_api(&client, Permission::Write, &idx("orders"), at(1)),
            Err(AuthzError::NotGranted)
        );
        assert_eq!(
            authz.authorize_api(&client, Permission::Search, &idx("payroll"), at(1)),
            Err(AuthzError::NotGranted)
        );
        // A principal of the same name in another cluster is a different principal.
        let alien = who(alice, "dr", Role::Client, Channel::Token);
        assert_eq!(
            authz.authorize_api(&alien, Permission::Search, &idx("orders"), at(1)),
            Err(AuthzError::ForeignCluster)
        );
    }

    #[test]
    fn admin_and_debug_need_the_admin_role_and_an_explicit_grant() {
        let (authz, store) = setup();
        store
            .install(
                IndexPolicy::new(idx("orders"), 1)
                    .grant("token:ops", Permission::Debug)
                    .unwrap(),
                at(0),
            )
            .unwrap();
        let ops = who("token:ops", "prod", Role::Admin, Channel::Token);
        assert!(
            authz
                .authorize_api(&ops, Permission::Debug, &idx("orders"), at(1))
                .is_ok()
        );
        assert_eq!(
            authz.authorize_api(&ops, Permission::Manage, &idx("orders"), at(1)),
            Err(AuthzError::NotGranted)
        );
        assert_eq!(
            authz.authorize_api(&ops, Permission::Search, &idx("orders"), at(1)),
            Err(AuthzError::WrongRole)
        );
    }

    #[test]
    fn a_stale_or_missing_policy_fails_closed() {
        let (authz, store) = setup();
        store
            .install(
                IndexPolicy::new(idx("orders"), 1)
                    .grant("token:a", Permission::Search)
                    .unwrap(),
                at(0),
            )
            .unwrap();
        let p = who("token:a", "prod", Role::Client, Channel::Token);
        assert!(
            authz
                .authorize_api(&p, Permission::Search, &idx("orders"), at(60))
                .is_ok()
        );
        assert_eq!(
            authz.authorize_api(&p, Permission::Search, &idx("orders"), at(61)),
            Err(AuthzError::PolicyStale)
        );
        // Re-validated against its source, it is usable again, at the same generation.
        store.refresh(&idx("orders"), 1, at(61)).unwrap();
        assert!(
            authz
                .authorize_api(&p, Permission::Search, &idx("orders"), at(62))
                .is_ok()
        );
        assert_eq!(
            authz.authorize_api(&p, Permission::Search, &idx("gone"), at(62)),
            Err(AuthzError::NoPolicy)
        );
        // A freshness stamp from the future (clock stepped back) is bounded the same way.
        assert_eq!(
            authz.authorize_api(&p, Permission::Search, &idx("orders"), at(0)),
            Err(AuthzError::PolicyStale)
        );
    }

    #[test]
    fn generations_only_grow_and_a_suspended_index_refuses_everyone() {
        let (authz, store) = setup();
        let grant = |g| {
            IndexPolicy::new(idx("orders"), g)
                .grant("token:a", Permission::Search)
                .unwrap()
        };
        store.install(grant(5), at(0)).unwrap();
        assert_eq!(
            store.install(grant(5), at(1)),
            Err(PolicyError::NotNewer { installed: 5 })
        );
        assert_eq!(
            store.install(grant(4), at(1)),
            Err(PolicyError::NotNewer { installed: 5 })
        );
        assert_eq!(store.generation(&idx("orders")), Some(5));
        let p = who("token:a", "prod", Role::Client, Channel::Token);
        assert_eq!(
            authz
                .authorize_api(&p, Permission::Search, &idx("orders"), at(1))
                .unwrap()
                .generation,
            Some(5)
        );
        // Revoking the grant is a new generation, effective at once on this node.
        store
            .install(IndexPolicy::new(idx("orders"), 6), at(2))
            .unwrap();
        assert_eq!(
            authz.authorize_api(&p, Permission::Search, &idx("orders"), at(3)),
            Err(AuthzError::NotGranted)
        );
        store.install(grant(7).suspended(true), at(4)).unwrap();
        assert_eq!(
            authz.authorize_api(&p, Permission::Search, &idx("orders"), at(5)),
            Err(AuthzError::IndexSuspended)
        );
        let node = who(
            "spiffe://d/hsearch/prod/s",
            "prod",
            Role::SearchNode,
            Channel::Mtls,
        );
        let coord = who(
            "spiffe://d/hsearch/prod/c",
            "prod",
            Role::Coordinator,
            Channel::Mtls,
        );
        assert_eq!(
            authz.authorize_peer(&coord, PeerRpc::ShardSearch, Some(&idx("orders")), at(5)),
            Err(AuthzError::IndexSuspended)
        );
        assert_eq!(
            authz.authorize_peer(&node, PeerRpc::SegmentFetch, Some(&idx("orders")), at(5)),
            Err(AuthzError::IndexSuspended)
        );
    }

    #[test]
    fn store_and_policy_are_bounded_and_refresh_needs_the_installed_generation() {
        let store = PolicyStore::new();
        assert_eq!(
            store.refresh(&idx("orders"), 1, at(0)),
            Err(PolicyError::NotInstalled)
        );
        store
            .install(IndexPolicy::new(idx("orders"), 2), at(0))
            .unwrap();
        assert_eq!(
            store.refresh(&idx("orders"), 1, at(1)),
            Err(PolicyError::NotInstalled)
        );
        store.evict(&idx("orders"));
        assert!(store.get(&idx("orders")).is_none());
        let mut p = IndexPolicy::new(idx("orders"), 1);
        for n in 0..IndexPolicy::MAX_GRANTS {
            p = p.grant(&format!("token:{n}"), Permission::Search).unwrap();
        }
        // Another permission for a principal already named costs no new slot; a new principal does.
        p = p.grant("token:0", Permission::Write).unwrap();
        assert_eq!(
            p.clone()
                .grant("token:extra", Permission::Search)
                .unwrap_err(),
            PolicyError::TooManyGrants
        );
        // Principal, quota and key scope are the one index's.
        let key = KeyRef::new("test/acct/k").unwrap();
        let p = IndexPolicy::new(idx("orders"), 1)
            .with_quota(Quota {
                max_stored_bytes: Some(10),
                ..Quota::default()
            })
            .with_key(key.clone());
        assert_eq!(
            (p.index().as_str(), p.quota().max_stored_bytes, p.key()),
            ("orders", Some(10), Some(&key))
        );
    }
}
