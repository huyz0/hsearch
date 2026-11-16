//! Who is calling. A [`VerifiedPrincipal`] can only come out of an [`IdentityProvider`]: its fields are
//! private and its constructor is crate-private, so no code outside this crate can say "trust me".

use std::collections::BTreeMap;
use std::fmt;
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::ids::{ClusterId, TrustDomain};

/// The separate kinds of authority. They never imply one another: an admin is not a peer, a peer is not a
/// client, and a replication consumer is neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    /// An end user or application using the public API.
    Client,
    /// An operator using the administration API.
    Admin,
    /// A node that accepts writes and appends to the journal.
    IndexNode,
    /// A node that serves queries.
    SearchNode,
    /// A node that routes requests and coordinates.
    Coordinator,
    /// A consumer of replicated data, possibly from another cluster.
    Replication,
}

impl Role {
    /// Every role.
    pub const ALL: [Role; 6] = [
        Role::Client,
        Role::Admin,
        Role::IndexNode,
        Role::SearchNode,
        Role::Coordinator,
        Role::Replication,
    ];

    /// The name used in identities and policy files.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Client => "client",
            Role::Admin => "admin",
            Role::IndexNode => "index",
            Role::SearchNode => "search",
            Role::Coordinator => "coordinator",
            Role::Replication => "replication",
        }
    }

    /// Parses a role name.
    pub fn parse(text: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.as_str() == text)
    }

    /// Whether this is a role a node takes in the cluster (the peer port speaks only to these).
    pub fn is_peer(self) -> bool {
        matches!(self, Role::IndexNode | Role::SearchNode | Role::Coordinator)
    }
}

/// How the principal proved itself. A peer RPC is never satisfiable by a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// A validated certificate chain on a mutually authenticated connection.
    Mtls,
    /// A bearer token. Public: it can be replayed by whoever holds it.
    Token,
}

/// A caller whose identity was verified by an [`IdentityProvider`].
#[derive(Clone, PartialEq, Eq)]
pub struct VerifiedPrincipal {
    name: String,
    cluster: ClusterId,
    trust_domain: Option<TrustDomain>,
    role: Role,
    channel: Channel,
}

impl VerifiedPrincipal {
    pub(crate) fn new(
        name: String,
        cluster: ClusterId,
        trust_domain: Option<TrustDomain>,
        role: Role,
        channel: Channel,
    ) -> Self {
        Self {
            name,
            cluster,
            trust_domain,
            role,
            channel,
        }
    }

    /// A stable name, used as the key in a policy's grants (a SPIFFE ID, a certificate name or a token label).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The cluster this principal belongs to.
    pub fn cluster(&self) -> &ClusterId {
        &self.cluster
    }

    /// The trust domain, for certificate identities.
    pub fn trust_domain(&self) -> Option<&TrustDomain> {
        self.trust_domain.as_ref()
    }

    /// The one role this principal holds.
    pub fn role(&self) -> Role {
        self.role
    }

    /// How it proved itself.
    pub fn channel(&self) -> Channel {
        self.channel
    }
}

impl fmt::Debug for VerifiedPrincipal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "VerifiedPrincipal({} role={} via {:?})",
            self.name,
            self.role.as_str(),
            self.channel
        )
    }
}

/// What a certificate is being used for. The leaf must carry the matching extended key usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertUse {
    /// The caller presents it as a client certificate.
    Client,
    /// The callee presents it as a server certificate.
    Server,
}

/// What the other side presented.
pub enum Presented<'a> {
    /// A DER certificate chain, leaf first.
    Certificate {
        /// The chain.
        chain: &'a [Vec<u8>],
        /// What the leaf is being used for.
        usage: CertUse,
    },
    /// A bearer token.
    Bearer(&'a str),
}

/// Why a caller was not identified. None of these carry certificate or token contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityError {
    /// The provider does not handle this kind of credential.
    WrongCredentialKind,
    /// The chain is empty, too long, or a certificate is malformed.
    Malformed(&'static str),
    /// The chain does not lead to a trusted CA.
    UntrustedIssuer,
    /// The certificate is past its end date, beyond the permitted skew.
    Expired,
    /// The certificate's start date is in the future, beyond the permitted skew.
    NotYetValid,
    /// The certificate cannot be used for this purpose (key usage or CA constraints).
    WrongUsage,
    /// The certificate verified but carries no identity this provider accepts.
    NoAcceptedIdentity(&'static str),
    /// The identity names a trust domain this deployment does not accept.
    WrongTrustDomain,
    /// The identity names a cluster this deployment does not accept.
    WrongCluster,
    /// The identity names a role that is not permitted for that cluster, or is not a known role.
    RoleNotPermitted,
    /// The token is unknown.
    UnknownToken,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdentityError::WrongCredentialKind => f.write_str("credential kind not accepted here"),
            IdentityError::Malformed(why) => write!(f, "malformed credential: {why}"),
            IdentityError::UntrustedIssuer => {
                f.write_str("certificate does not chain to a trusted CA")
            }
            IdentityError::Expired => f.write_str("certificate expired"),
            IdentityError::NotYetValid => f.write_str("certificate not yet valid"),
            IdentityError::WrongUsage => f.write_str("certificate not valid for this use"),
            IdentityError::NoAcceptedIdentity(why) => {
                write!(f, "no accepted identity in certificate: {why}")
            }
            IdentityError::WrongTrustDomain => f.write_str("trust domain not accepted"),
            IdentityError::WrongCluster => f.write_str("cluster not accepted"),
            IdentityError::RoleNotPermitted => f.write_str("role not permitted"),
            IdentityError::UnknownToken => f.write_str("unknown token"),
        }
    }
}

impl std::error::Error for IdentityError {}

/// Turns what a peer presented into a verified principal, or refuses.
pub trait IdentityProvider: Send + Sync {
    /// Verifies `presented` at `now`.
    fn verify(
        &self,
        presented: &Presented<'_>,
        now: SystemTime,
    ) -> Result<VerifiedPrincipal, IdentityError>;
}

/// Bearer-token identities for the public API: only [`Role::Client`] and [`Role::Admin`], and never a peer
/// role, whatever the table says. Tokens are held as SHA-256 digests and compared in constant time over
/// the digest, so the table does not hold a usable secret.
pub struct TokenProvider {
    cluster: ClusterId,
    by_digest: BTreeMap<[u8; 32], (String, Role)>,
}

/// Why a token could not be registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenError {
    /// A token may identify a client or an admin only.
    PeerRoleRefused,
    /// The token is too short to resist guessing.
    TooShort,
    /// Two labels would share one token.
    Duplicate,
    /// The label is empty, over 64 bytes or outside `[A-Za-z0-9._-]`: it becomes the principal's name in
    /// policies and logs, so it cannot carry separators or control characters.
    BadLabel,
}

impl TokenProvider {
    /// The shortest token accepted.
    pub const MIN_TOKEN_LEN: usize = 24;

    /// An empty table for a cluster.
    pub fn new(cluster: ClusterId) -> Self {
        Self {
            cluster,
            by_digest: BTreeMap::new(),
        }
    }

    /// Registers `token` as `label` holding `role`.
    pub fn add(&mut self, label: &str, token: &str, role: Role) -> Result<(), TokenError> {
        let label_ok = !label.is_empty()
            && label.len() <= 64
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !label_ok {
            return Err(TokenError::BadLabel);
        }
        if !matches!(role, Role::Client | Role::Admin) {
            return Err(TokenError::PeerRoleRefused);
        }
        if token.len() < Self::MIN_TOKEN_LEN {
            return Err(TokenError::TooShort);
        }
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        // Refuse before inserting: a second registration of one secret must not replace the first's role.
        if self.by_digest.contains_key(&digest) {
            return Err(TokenError::Duplicate);
        }
        self.by_digest
            .insert(digest, (format!("token:{label}"), role));
        Ok(())
    }
}

impl IdentityProvider for TokenProvider {
    fn verify(
        &self,
        presented: &Presented<'_>,
        _now: SystemTime,
    ) -> Result<VerifiedPrincipal, IdentityError> {
        let Presented::Bearer(token) = presented else {
            return Err(IdentityError::WrongCredentialKind);
        };
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        // A BTreeMap lookup compares digests, not secrets: timing reveals nothing about the token itself.
        let (label, role) = self
            .by_digest
            .get(&digest)
            .ok_or(IdentityError::UnknownToken)?;
        Ok(VerifiedPrincipal::new(
            label.clone(),
            self.cluster.clone(),
            None,
            *role,
            Channel::Token,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip_and_only_node_roles_are_peers() {
        for role in Role::ALL {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("root"), None);
        let peers: Vec<_> = Role::ALL.into_iter().filter(|r| r.is_peer()).collect();
        assert_eq!(
            peers,
            [Role::IndexNode, Role::SearchNode, Role::Coordinator]
        );
    }

    #[test]
    fn errors_describe_themselves_without_credential_contents() {
        for e in [
            IdentityError::WrongCredentialKind,
            IdentityError::Malformed("x"),
            IdentityError::UntrustedIssuer,
            IdentityError::Expired,
            IdentityError::NotYetValid,
            IdentityError::WrongUsage,
            IdentityError::NoAcceptedIdentity("x"),
            IdentityError::WrongTrustDomain,
            IdentityError::WrongCluster,
            IdentityError::RoleNotPermitted,
            IdentityError::UnknownToken,
        ] {
            assert!(!e.to_string().is_empty());
        }
        let who = VerifiedPrincipal::new(
            "token:a".into(),
            ClusterId::new("prod").unwrap(),
            None,
            Role::Client,
            Channel::Token,
        );
        assert_eq!(
            format!("{who:?}"),
            "VerifiedPrincipal(token:a role=client via Token)"
        );
    }
}
