//! Validated identifiers. Every one is bounded and drawn from a small alphabet, so nothing a peer or a
//! client sends can smuggle a separator, a control character or an unbounded string into a key, a log
//! line or a policy lookup.

use std::fmt;
use std::sync::Arc;

/// The longest index, cluster or domain label.
pub const MAX_LABEL: usize = 64;
/// The longest trust domain, the DNS maximum.
pub const MAX_TRUST_DOMAIN: usize = 253;

/// A rejected identifier: which kind and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdError {
    /// What was being parsed.
    pub kind: &'static str,
    /// Why it was refused.
    pub reason: &'static str,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {}", self.kind, self.reason)
    }
}

impl std::error::Error for IdError {}

fn check(
    kind: &'static str,
    text: &str,
    max: usize,
    ok: impl Fn(u8) -> bool,
) -> Result<(), IdError> {
    if text.is_empty() {
        return Err(IdError {
            kind,
            reason: "empty",
        });
    }
    if text.len() > max {
        return Err(IdError {
            kind,
            reason: "too long",
        });
    }
    if !text.bytes().all(ok) {
        return Err(IdError {
            kind,
            reason: "character outside the allowed set",
        });
    }
    Ok(())
}

/// An index name. Tenant = index: this is the unit of principal, policy, quota and key scope.
///
/// The rule is the one the node enforces when it creates an index: 1-64 of `[a-z0-9_-]`, with `v1`
/// reserved. Cloning is a reference-count increment, so a cache key built from one does not allocate.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IndexId(Arc<str>);

impl IndexId {
    /// Parses and validates an index name.
    pub fn new(text: &str) -> Result<Self, IdError> {
        check("index name", text, MAX_LABEL, |b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
        })?;
        if text == "v1" {
            return Err(IdError {
                kind: "index name",
                reason: "v1 is reserved",
            });
        }
        Ok(Self(text.into()))
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for IndexId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IndexId({})", self.0)
    }
}

impl fmt::Display for IndexId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A cluster name: 1-64 of `[a-z0-9-]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClusterId(Arc<str>);

impl ClusterId {
    /// Parses and validates a cluster name.
    pub fn new(text: &str) -> Result<Self, IdError> {
        check("cluster id", text, MAX_LABEL, |b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'
        })?;
        Ok(Self(text.into()))
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A SPIFFE trust domain: lower-case DNS-style, up to 253 bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrustDomain(Arc<str>);

impl TrustDomain {
    /// Parses and validates a trust domain.
    pub fn new(text: &str) -> Result<Self, IdError> {
        check("trust domain", text, MAX_TRUST_DOMAIN, |b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.' || b == b'_'
        })?;
        Ok(Self(text.into()))
    }

    /// The domain.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TrustDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_names_follow_the_node_rule() {
        for ok in ["a", "orders-2026_q1", &"a".repeat(64)] {
            assert_eq!(IndexId::new(ok).unwrap().as_str(), ok);
        }
        for bad in [
            "",
            "Orders",
            "a b",
            "a/b",
            "a.b",
            "ü",
            "v1",
            &"a".repeat(65),
            "a\0",
            "../x",
        ] {
            assert!(IndexId::new(bad).is_err(), "{bad:?}");
        }
        let id = IndexId::new("orders").unwrap();
        assert_eq!(
            (format!("{id}"), format!("{id:?}")),
            ("orders".into(), "IndexId(orders)".into())
        );
        // A clone shares the allocation: building a cache key from one must not copy the string.
        assert!(std::ptr::eq(id.as_str(), id.clone().as_str()));
    }

    #[test]
    fn cluster_and_trust_domain_are_bounded_alphabets() {
        assert!(ClusterId::new("prod-eu-1").is_ok());
        assert!(
            ClusterId::new("Prod").is_err()
                && ClusterId::new("a_b").is_err()
                && ClusterId::new("").is_err()
        );
        assert_eq!(ClusterId::new("prod").unwrap().to_string(), "prod");
        assert!(TrustDomain::new("example.test").is_ok());
        assert!(TrustDomain::new("Example.test").is_err());
        assert!(TrustDomain::new(&"a".repeat(MAX_TRUST_DOMAIN + 1)).is_err());
        assert_eq!(
            TrustDomain::new("example.test").unwrap().to_string(),
            "example.test"
        );
        let e = IndexId::new("").unwrap_err();
        assert_eq!(e.to_string(), "invalid index name: empty");
    }
}
