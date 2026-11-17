//! Mutual TLS 1.3 whose certificates and trust bundle rotate while connections stay open (M05.06).
//!
//! Nothing here builds a new `ServerConfig` or `ClientConfig` on rotation. Each config is built once with a
//! resolver and a verifier that read the current state at the moment of every handshake, so:
//!
//! * a rotation takes effect for the next handshake and never touches a connection that already finished
//!   its handshake: its keys came from the certificate it authenticated with, and a stream in flight keeps
//!   going;
//! * a rotation to something unusable (expired, issued by an untrusted CA, or a key that does not match) is
//!   refused before it is installed, so a bad file cannot take a node off the network;
//! * no session is resumed: a resumed TLS session skips certificate verification, so it would outlive a
//!   rotated bundle or an expired certificate;
//! * a long-lived channel is re-authenticated by policy, not by luck: [`PeerSession::check`] re-verifies the
//!   peer's certificate against the trust bundle in force *now* and ends the session when the certificate
//!   expired, its issuer left the bundle after the overlap, or the channel is older than its maximum age.
//!
//! The protocol is TLS 1.3 only and the client must present a certificate. There is no way to build either
//! config without verification, no insecure mode, and no plaintext fallback. Chain validation is
//! [`crate::x509::ChainVerifier`]'s through an [`IdentityProvider`], so role, trust-domain, cluster and
//! federation checks apply during the handshake exactly as they do for any other credential. Server names
//! are not matched: a SPIFFE identity is not a DNS name, and what the peer is allowed to be is the
//! provider's decision.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::client::ResolvesClientCert;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, Error, ServerConfig, SignatureScheme,
};
use zeroize::Zeroizing;

use crate::identity::{CertUse, IdentityError, IdentityProvider, Presented, VerifiedPrincipal};

/// The longest chain this module installs or accepts.
pub const MAX_CHAIN: usize = crate::x509::MAX_CHAIN;

/// The default longest a channel lives before it must be re-authenticated.
pub const DEFAULT_MAX_CHANNEL_AGE: Duration = Duration::from_secs(60 * 60);

/// Why a rotation was refused. The current certificate stays in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RotateError {
    /// The chain is empty or too long.
    BadChain,
    /// The private key is not usable, or does not belong to the leaf.
    BadKey,
    /// The new certificate would not be accepted by this node's own verifier at `now`.
    NotAcceptable(IdentityError),
}

impl std::fmt::Display for RotateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RotateError::BadChain => f.write_str("certificate chain empty or too long"),
            RotateError::BadKey => f.write_str("private key unusable or not the leaf's"),
            RotateError::NotAcceptable(why) => write!(f, "certificate not acceptable: {why}"),
        }
    }
}

impl std::error::Error for RotateError {}

fn unix(now: UnixTime) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(now.as_secs())
}

/// This node's own certificate and key, replaceable at any time. Cloning shares the slot.
#[derive(Clone)]
pub struct RotatingCert {
    slot: Arc<RwLock<Arc<CertifiedKey>>>,
    generation: Arc<AtomicU64>,
    provider: Arc<CryptoProvider>,
    usage: CertUse,
    identity: Arc<dyn IdentityProvider>,
}

impl std::fmt::Debug for RotatingCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RotatingCert(generation {})", self.generation())
    }
}

impl RotatingCert {
    /// Installs the first certificate. `usage` is how this node presents it; `identity` is what judges
    /// certificates (this node's own must pass it too, which catches a certificate the peers would refuse).
    pub fn new(
        chain: Vec<Vec<u8>>,
        key: Zeroizing<Vec<u8>>,
        usage: CertUse,
        identity: Arc<dyn IdentityProvider>,
        now: SystemTime,
    ) -> Result<Self, RotateError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let certified = build(&chain, &key, &provider, usage, identity.as_ref(), now)?;
        Ok(Self {
            slot: Arc::new(RwLock::new(Arc::new(certified))),
            generation: Arc::new(AtomicU64::new(1)),
            provider,
            usage,
            identity,
        })
    }

    /// Replaces the certificate. Verified first; on refusal nothing changes. Returns the new generation.
    pub fn rotate(
        &self,
        chain: Vec<Vec<u8>>,
        key: Zeroizing<Vec<u8>>,
        now: SystemTime,
    ) -> Result<u64, RotateError> {
        let certified = build(
            &chain,
            &key,
            &self.provider,
            self.usage,
            self.identity.as_ref(),
            now,
        )?;
        *self.slot.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(certified);
        Ok(self.generation.fetch_add(1, Ordering::AcqRel) + 1)
    }

    /// How many certificates have been installed (the first is generation 1).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The leaf DER currently presented.
    pub fn current_leaf(&self) -> Vec<u8> {
        self.current().cert[0].as_ref().to_vec()
    }

    fn current(&self) -> Arc<CertifiedKey> {
        self.slot.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

fn build(
    chain: &[Vec<u8>],
    key: &[u8],
    provider: &CryptoProvider,
    usage: CertUse,
    identity: &dyn IdentityProvider,
    now: SystemTime,
) -> Result<CertifiedKey, RotateError> {
    if chain.is_empty() || chain.len() > MAX_CHAIN {
        return Err(RotateError::BadChain);
    }
    identity
        .verify(&Presented::Certificate { chain, usage }, now)
        .map_err(RotateError::NotAcceptable)?;
    let der = chain
        .iter()
        .map(|c| CertificateDer::from(c.clone()))
        .collect();
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(key.to_vec()));
    CertifiedKey::from_der(der, key, provider).map_err(|_| RotateError::BadKey)
}

impl ResolvesServerCert for RotatingCert {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
}

impl ResolvesClientCert for RotatingCert {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// Judges the peer's chain through the identity provider at handshake time, so a rotated trust bundle
/// applies to the next handshake.
struct PeerVerifier {
    identity: Arc<dyn IdentityProvider>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl PeerVerifier {
    fn judge(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        usage: CertUse,
        now: UnixTime,
    ) -> Result<(), Error> {
        if intermediates.len() + 1 > MAX_CHAIN {
            return Err(Error::General("certificate chain too long".into()));
        }
        let chain: Vec<Vec<u8>> = std::iter::once(end_entity)
            .chain(intermediates)
            .map(|c| c.as_ref().to_vec())
            .collect();
        self.identity
            .verify(
                &Presented::Certificate {
                    chain: &chain,
                    usage,
                },
                unix(now),
            )
            .map(|_| ())
            .map_err(|e| Error::General(e.to_string()))
    }
}

impl std::fmt::Debug for PeerVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PeerVerifier")
    }
}

impl ClientCertVerifier for PeerVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.judge(end_entity, intermediates, CertUse::Client, now)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

impl ServerCertVerifier for PeerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.judge(end_entity, intermediates, CertUse::Server, now)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// The two configs for one node, sharing the rotating certificate and the verifier.
pub struct PeerTls {
    server: Arc<ServerConfig>,
    client: Arc<ClientConfig>,
    cert: RotatingCert,
    identity: Arc<dyn IdentityProvider>,
}

impl PeerTls {
    /// Builds a server config (client certificates mandatory) and a client config, both TLS 1.3 only.
    /// `cert` must be presentable as both (a node dials peers and accepts them), so build it with
    /// [`CertUse::Server`] and a certificate that also carries client authentication.
    pub fn new(cert: RotatingCert, identity: Arc<dyn IdentityProvider>) -> Result<Self, Error> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(PeerVerifier {
            identity: identity.clone(),
            algorithms: provider.signature_verification_algorithms,
        });
        let mut server = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(verifier.clone())
            .with_cert_resolver(Arc::new(cert.clone()));
        // A resumed session skips certificate verification, so it would outlive a rotated trust bundle or
        // an expired certificate. Every connection does a full handshake instead: peers hold long-lived
        // connections, so the cost is one handshake per connection, not per request.
        server.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        server.send_tls13_tickets = 0;
        let mut client = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_cert_resolver(Arc::new(cert.clone()));
        client.resumption = rustls::client::Resumption::disabled();
        Ok(Self {
            server: Arc::new(server),
            client: Arc::new(client),
            cert,
            identity,
        })
    }

    /// The config for accepting connections.
    pub fn server_config(&self) -> Arc<ServerConfig> {
        self.server.clone()
    }

    /// The config for dialling.
    pub fn client_config(&self) -> Arc<ClientConfig> {
        self.client.clone()
    }

    /// The rotating certificate.
    pub fn cert(&self) -> &RotatingCert {
        &self.cert
    }

    /// Starts tracking an established connection whose peer presented `chain`. Fails when the chain does
    /// not verify now (it should have, at the handshake).
    pub fn session(
        &self,
        chain: &[CertificateDer<'_>],
        usage: CertUse,
        now: SystemTime,
        max_age: Duration,
    ) -> Result<PeerSession, IdentityError> {
        let chain: Vec<Vec<u8>> = chain.iter().map(|c| c.as_ref().to_vec()).collect();
        let principal = self.identity.verify(
            &Presented::Certificate {
                chain: &chain,
                usage,
            },
            now,
        )?;
        Ok(PeerSession {
            chain,
            usage,
            established: now,
            max_age,
            principal,
            identity: self.identity.clone(),
        })
    }
}

/// Why a session must end and be re-established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reauthenticate {
    /// The channel outlived its maximum age.
    TooOld,
    /// The peer's certificate no longer verifies (expired, or its issuer left the trust bundle).
    NoLongerTrusted(IdentityError),
    /// The clock moved back past the time the channel was established.
    ClockWentBack,
}

/// One authenticated channel, re-checkable against the trust in force at any later time.
pub struct PeerSession {
    chain: Vec<Vec<u8>>,
    usage: CertUse,
    established: SystemTime,
    max_age: Duration,
    principal: VerifiedPrincipal,
    identity: Arc<dyn IdentityProvider>,
}

impl PeerSession {
    /// Who the peer is.
    pub fn principal(&self) -> &VerifiedPrincipal {
        &self.principal
    }

    /// Whether the session may continue at `now`. A caller checks between messages (or on a timer); a
    /// message already in flight is not interrupted by a rotation.
    pub fn check(&self, now: SystemTime) -> Result<(), Reauthenticate> {
        match now.duration_since(self.established) {
            Err(_) => return Err(Reauthenticate::ClockWentBack),
            Ok(age) if age > self.max_age => return Err(Reauthenticate::TooOld),
            Ok(_) => {}
        }
        self.identity
            .verify(
                &Presented::Certificate {
                    chain: &self.chain,
                    usage: self.usage,
                },
                now,
            )
            .map(|_| ())
            .map_err(Reauthenticate::NoLongerTrusted)
    }
}
