//! Certificate identities: chain verification and the two ways a verified certificate becomes a principal.
//!
//! Chain verification is `rustls-webpki`'s, not ours: path building, signature checks, validity, CA
//! constraints and extended key usage. This module adds what it does not know about: which names mean
//! which role, a bounded clock skew, and a trust bundle that can be rotated with a bounded overlap.
//!
//! There is no way to build a [`ChainVerifier`] that skips verification: it has no "insecure" mode and no
//! flag, it refuses an empty trust bundle, and the skew and overlap it accepts are capped constants.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls_pki_types::{CertificateDer, SignatureVerificationAlgorithm, UnixTime};
use webpki::{EndEntityCert, KeyUsage, anchor_from_trusted_cert};
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::identity::{
    CertUse, Channel, IdentityError, IdentityProvider, Presented, Role, VerifiedPrincipal,
};
use crate::ids::{ClusterId, TrustDomain};

/// The most clock skew any configuration may allow.
pub const MAX_SKEW: Duration = Duration::from_secs(5 * 60);
/// The longest a retired trust bundle keeps being accepted after a rotation.
pub const MAX_BUNDLE_OVERLAP: Duration = Duration::from_secs(60 * 60);
/// The most certificates in one presented chain, leaf included.
pub const MAX_CHAIN: usize = 6;
/// The largest DER certificate accepted.
pub const MAX_CERT_BYTES: usize = 16 * 1024;
/// The most trust anchors in a bundle.
pub const MAX_ANCHORS: usize = 32;
/// The signature algorithms accepted: ECDSA P-256/P-384, Ed25519 and RSA PKCS#1 v1.5 (2048-8192 bit).
/// The legacy-key RSA-PSS variants are left out on purpose.
static SIGNATURE_ALGS: &[&dyn SignatureVerificationAlgorithm] = &[
    webpki::ring::ECDSA_P256_SHA256,
    webpki::ring::ECDSA_P256_SHA384,
    webpki::ring::ECDSA_P384_SHA256,
    webpki::ring::ECDSA_P384_SHA384,
    webpki::ring::ED25519,
    webpki::ring::RSA_PKCS1_2048_8192_SHA256,
    webpki::ring::RSA_PKCS1_2048_8192_SHA384,
    webpki::ring::RSA_PKCS1_2048_8192_SHA512,
];
/// The most SAN entries examined in one certificate.
const MAX_SANS: usize = 32;

/// A configuration the verifier refuses to start with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// A trust bundle with no anchors would trust nobody; it is refused rather than treated as "off".
    EmptyTrustBundle,
    /// More than [`MAX_ANCHORS`].
    TooManyAnchors,
    /// An anchor is not a certificate.
    BadAnchor,
    /// The skew exceeds [`MAX_SKEW`].
    SkewTooLarge,
    /// The overlap exceeds [`MAX_BUNDLE_OVERLAP`].
    OverlapTooLarge,
    /// An accepted identity lists no roles, or the same domain and cluster twice.
    BadAcceptedIdentities,
    /// The file table has a malformed line.
    BadTable(usize),
}

type Anchors = Arc<Vec<Vec<u8>>>;

struct Bundles {
    current: Anchors,
    previous: Option<(Anchors, SystemTime)>,
}

/// Verifies a certificate chain against a rotating trust bundle.
pub struct ChainVerifier {
    bundles: RwLock<Bundles>,
    skew: Duration,
}

/// What a verified leaf says about itself.
#[derive(Debug)]
pub struct Leaf {
    /// URI subject alternative names.
    pub uris: Vec<String>,
    /// DNS subject alternative names.
    pub dns: Vec<String>,
}

fn check_anchors(anchors: &[Vec<u8>]) -> Result<(), ConfigError> {
    if anchors.is_empty() {
        return Err(ConfigError::EmptyTrustBundle);
    }
    if anchors.len() > MAX_ANCHORS {
        return Err(ConfigError::TooManyAnchors);
    }
    for der in anchors {
        if der.len() > MAX_CERT_BYTES
            || anchor_from_trusted_cert(&CertificateDer::from(der.as_slice())).is_err()
        {
            return Err(ConfigError::BadAnchor);
        }
    }
    Ok(())
}

fn unix(time: SystemTime) -> UnixTime {
    UnixTime::since_unix_epoch(time.duration_since(UNIX_EPOCH).unwrap_or_default())
}

impl ChainVerifier {
    /// A verifier for `anchors` (DER CA certificates). `skew` is how far a certificate's validity window may
    /// be stretched at either end, at most [`MAX_SKEW`].
    pub fn new(anchors: Vec<Vec<u8>>, skew: Duration) -> Result<Self, ConfigError> {
        check_anchors(&anchors)?;
        if skew > MAX_SKEW {
            return Err(ConfigError::SkewTooLarge);
        }
        Ok(Self {
            bundles: RwLock::new(Bundles {
                current: Arc::new(anchors),
                previous: None,
            }),
            skew,
        })
    }

    /// Replaces the trust bundle. The old bundle stays acceptable for `overlap` (at most
    /// [`MAX_BUNDLE_OVERLAP`]) so certificates issued under it keep working while they are re-issued.
    pub fn rotate(
        &self,
        anchors: Vec<Vec<u8>>,
        now: SystemTime,
        overlap: Duration,
    ) -> Result<(), ConfigError> {
        check_anchors(&anchors)?;
        if overlap > MAX_BUNDLE_OVERLAP {
            return Err(ConfigError::OverlapTooLarge);
        }
        let mut bundles = self
            .bundles
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::mem::replace(&mut bundles.current, Arc::new(anchors));
        bundles.previous = (!overlap.is_zero()).then(|| (old, now + overlap));
        Ok(())
    }

    /// Verifies the chain (leaf first) at `now` for `usage` and returns the leaf's names.
    pub fn verify_leaf(
        &self,
        chain: &[Vec<u8>],
        usage: CertUse,
        now: SystemTime,
    ) -> Result<Leaf, IdentityError> {
        if chain.is_empty() {
            return Err(IdentityError::Malformed("empty chain"));
        }
        if chain.len() > MAX_CHAIN || chain.iter().any(|c| c.len() > MAX_CERT_BYTES) {
            return Err(IdentityError::Malformed(
                "chain too long or certificate too large",
            ));
        }
        let (current, previous) = {
            let bundles = self
                .bundles
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = bundles
                .previous
                .as_ref()
                .filter(|(_, until)| now < *until)
                .map(|(a, _)| a.clone());
            (bundles.current.clone(), previous)
        };
        let mut outcome = self.verify_against(&current, chain, usage, now);
        if matches!(outcome, Err(IdentityError::UntrustedIssuer))
            && let Some(previous) = previous
        {
            outcome = self.verify_against(&previous, chain, usage, now);
        }
        outcome?;
        leaf_names(&chain[0])
    }

    fn verify_against(
        &self,
        anchors: &[Vec<u8>],
        chain: &[Vec<u8>],
        usage: CertUse,
        now: SystemTime,
    ) -> Result<(), IdentityError> {
        let leaf_der = CertificateDer::from(chain[0].as_slice());
        let leaf = EndEntityCert::try_from(&leaf_der)
            .map_err(|_| IdentityError::Malformed("leaf certificate"))?;
        let intermediates: Vec<CertificateDer<'_>> = chain[1..]
            .iter()
            .map(|c| CertificateDer::from(c.as_slice()))
            .collect();
        let owned: Vec<CertificateDer<'_>> = anchors
            .iter()
            .map(|a| CertificateDer::from(a.as_slice()))
            .collect();
        let trust: Vec<_> = owned
            .iter()
            .filter_map(|der| anchor_from_trusted_cert(der).ok())
            .collect();
        let try_at = |at: SystemTime| {
            let eku = match usage {
                CertUse::Client => KeyUsage::client_auth(),
                CertUse::Server => KeyUsage::server_auth(),
            };
            leaf.verify_for_usage(
                SIGNATURE_ALGS,
                &trust,
                &intermediates,
                unix(at),
                eku,
                None,
                None,
            )
            .map(|_| ())
        };
        let mut result = try_at(now);
        // Skew stretches the window at both ends and no further: a certificate that expired a moment ago on
        // a peer whose clock runs behind ours still verifies, one that expired an hour ago does not.
        match &result {
            Err(webpki::Error::CertNotValidYet { .. }) => result = try_at(now + self.skew),
            Err(webpki::Error::CertExpired { .. }) => {
                result = try_at(now.checked_sub(self.skew).unwrap_or(UNIX_EPOCH))
            }
            _ => {}
        }
        result.map_err(map_error)
    }
}

fn map_error(error: webpki::Error) -> IdentityError {
    use webpki::Error as E;
    match error {
        E::CertExpired { .. } => IdentityError::Expired,
        E::CertNotValidYet { .. } => IdentityError::NotYetValid,
        E::UnknownIssuer
        | E::InvalidSignatureForPublicKey
        | E::CertDistrusted
        | E::UnsupportedSignatureAlgorithmContext(_) => IdentityError::UntrustedIssuer,
        E::RequiredEkuNotFoundContext(_)
        | E::CaUsedAsEndEntity
        | E::EndEntityUsedAsCa
        | E::PathLenConstraintViolated
        | E::NameConstraintViolation => IdentityError::WrongUsage,
        _ => IdentityError::Malformed("certificate"),
    }
}

fn leaf_names(der: &[u8]) -> Result<Leaf, IdentityError> {
    let (rest, cert) =
        X509Certificate::from_der(der).map_err(|_| IdentityError::Malformed("leaf certificate"))?;
    if !rest.is_empty() {
        return Err(IdentityError::Malformed("trailing bytes after certificate"));
    }
    let mut leaf = Leaf {
        uris: Vec::new(),
        dns: Vec::new(),
    };
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in san.value.general_names.iter().take(MAX_SANS) {
            match name {
                GeneralName::URI(uri) => leaf.uris.push((*uri).to_string()),
                GeneralName::DNSName(dns) => leaf.dns.push((*dns).to_string()),
                _ => {}
            }
        }
    }
    Ok(leaf)
}

/// What a deployment accepts from one trust domain and cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedIdentity {
    /// The trust domain.
    pub trust_domain: TrustDomain,
    /// The cluster.
    pub cluster: ClusterId,
    /// The roles identities from there may hold. A federated cluster lists only what it is trusted for
    /// (usually [`Role::Replication`]); trust is never transitive.
    pub roles: BTreeSet<Role>,
}

fn check_accepted(accepted: &[AcceptedIdentity]) -> Result<(), ConfigError> {
    let mut seen = BTreeSet::new();
    for a in accepted {
        if a.roles.is_empty() || !seen.insert((a.trust_domain.clone(), a.cluster.clone())) {
            return Err(ConfigError::BadAcceptedIdentities);
        }
    }
    if accepted.is_empty() {
        return Err(ConfigError::BadAcceptedIdentities);
    }
    Ok(())
}

/// SPIFFE X.509 identities: exactly one URI SAN, `spiffe://<trust-domain>/hsearch/<cluster>/<role>`.
pub struct SpiffeIdentityProvider {
    verifier: Arc<ChainVerifier>,
    accepted: Vec<AcceptedIdentity>,
}

impl SpiffeIdentityProvider {
    /// A provider accepting `accepted` identities whose certificates `verifier` accepts.
    pub fn new(
        verifier: Arc<ChainVerifier>,
        accepted: Vec<AcceptedIdentity>,
    ) -> Result<Self, ConfigError> {
        check_accepted(&accepted)?;
        Ok(Self { verifier, accepted })
    }
}

impl IdentityProvider for SpiffeIdentityProvider {
    fn verify(
        &self,
        presented: &Presented<'_>,
        now: SystemTime,
    ) -> Result<VerifiedPrincipal, IdentityError> {
        let Presented::Certificate { chain, usage } = presented else {
            return Err(IdentityError::WrongCredentialKind);
        };
        let leaf = self.verifier.verify_leaf(chain, *usage, now)?;
        let spiffe: Vec<&String> = leaf
            .uris
            .iter()
            .filter(|u| u.starts_with("spiffe://"))
            .collect();
        let [id] = spiffe.as_slice() else {
            return Err(IdentityError::NoAcceptedIdentity(
                "a SPIFFE identity needs exactly one spiffe:// URI SAN",
            ));
        };
        let rest = &id["spiffe://".len()..];
        let mut parts = rest.split('/');
        let (domain, ns, cluster, role, extra) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        );
        let (Some(domain), Some("hsearch"), Some(cluster), Some(role), None) =
            (domain, ns, cluster, role, extra)
        else {
            return Err(IdentityError::NoAcceptedIdentity(
                "SPIFFE ID is not spiffe://<domain>/hsearch/<cluster>/<role>",
            ));
        };
        let domain = TrustDomain::new(domain).map_err(|_| IdentityError::WrongTrustDomain)?;
        let cluster = ClusterId::new(cluster).map_err(|_| IdentityError::WrongCluster)?;
        let role = Role::parse(role).ok_or(IdentityError::RoleNotPermitted)?;
        let entry = match self
            .accepted
            .iter()
            .find(|a| a.trust_domain == domain && a.cluster == cluster)
        {
            Some(entry) => entry,
            None if self.accepted.iter().any(|a| a.trust_domain == domain) => {
                return Err(IdentityError::WrongCluster);
            }
            None => return Err(IdentityError::WrongTrustDomain),
        };
        if !entry.roles.contains(&role) {
            return Err(IdentityError::RoleNotPermitted);
        }
        Ok(VerifiedPrincipal::new(
            (*id).clone(),
            cluster,
            Some(domain),
            role,
            Channel::Mtls,
        ))
    }
}

/// Certificates identified by a DNS SAN looked up in an operator-supplied table, for deployments without a
/// SPIFFE issuer. The table is the authority: a certificate is never trusted for a role because of what its
/// name looks like.
pub struct FileIdentityProvider {
    verifier: Arc<ChainVerifier>,
    table: BTreeMap<String, (ClusterId, Role)>,
}

impl FileIdentityProvider {
    /// The most entries in a table.
    pub const MAX_ENTRIES: usize = 10_000;

    /// Parses a table: one `<dns-name> <cluster> <role>` per line, `#` comments and blank lines ignored.
    pub fn parse_table(text: &str) -> Result<BTreeMap<String, (ClusterId, Role)>, ConfigError> {
        let mut table = BTreeMap::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [dns, cluster, role] = fields.as_slice() else {
                return Err(ConfigError::BadTable(n + 1));
            };
            let cluster = ClusterId::new(cluster).map_err(|_| ConfigError::BadTable(n + 1))?;
            let role = Role::parse(role).ok_or(ConfigError::BadTable(n + 1))?;
            // A name with a colon could pose as a token label or a SPIFFE ID in a policy's grants.
            if dns.contains(':')
                || table.len() >= Self::MAX_ENTRIES
                || table.insert((*dns).to_string(), (cluster, role)).is_some()
            {
                return Err(ConfigError::BadTable(n + 1));
            }
        }
        Ok(table)
    }

    /// A provider over `table` whose certificates `verifier` accepts.
    pub fn new(verifier: Arc<ChainVerifier>, table: BTreeMap<String, (ClusterId, Role)>) -> Self {
        Self { verifier, table }
    }
}

impl IdentityProvider for FileIdentityProvider {
    fn verify(
        &self,
        presented: &Presented<'_>,
        now: SystemTime,
    ) -> Result<VerifiedPrincipal, IdentityError> {
        let Presented::Certificate { chain, usage } = presented else {
            return Err(IdentityError::WrongCredentialKind);
        };
        let leaf = self.verifier.verify_leaf(chain, *usage, now)?;
        let mut found = leaf
            .dns
            .iter()
            .filter_map(|name| self.table.get_key_value(name));
        let (Some((name, (cluster, role))), None) = (found.next(), found.next()) else {
            return Err(IdentityError::NoAcceptedIdentity(
                "exactly one DNS SAN must be in the identity table",
            ));
        };
        Ok(VerifiedPrincipal::new(
            name.clone(),
            cluster.clone(),
            None,
            *role,
            Channel::Mtls,
        ))
    }
}
