//! A development certificate authority: mints a CA and node certificates for local clusters and tests.
//! Behind the `dev-ca` feature; no production crate enables it.

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use time::OffsetDateTime;

/// A throw-away CA.
pub struct DevCa {
    cert: rcgen::Certificate,
    params: CertificateParams,
    key: KeyPair,
}

/// What to put in a node certificate.
pub struct NodeCert {
    /// URI SANs (`spiffe://...`).
    pub uris: Vec<String>,
    /// DNS SANs.
    pub dns: Vec<String>,
    /// Seconds from now to the start of validity (negative: already started).
    pub not_before: i64,
    /// Seconds from now to the end of validity.
    pub not_after: i64,
    /// Whether to carry both client and server auth usages.
    pub client_and_server: bool,
}

impl NodeCert {
    /// A valid certificate with a SPIFFE URI and both usages.
    pub fn spiffe(uri: &str) -> Self {
        Self {
            uris: vec![uri.to_string()],
            dns: vec![],
            not_before: -60,
            not_after: 3600,
            client_and_server: true,
        }
    }

    /// A valid certificate with one DNS name and both usages.
    pub fn named(dns: &str) -> Self {
        Self {
            uris: vec![],
            dns: vec![dns.to_string()],
            not_before: -60,
            not_after: 3600,
            client_and_server: true,
        }
    }
}

/// A minted certificate chain entry.
pub struct Minted {
    /// DER certificate.
    pub der: Vec<u8>,
    /// The leaf's private key, PKCS#8 DER.
    pub key_pkcs8: Vec<u8>,
}

impl DevCa {
    /// Creates a CA named `name`.
    pub fn new(name: &str) -> Result<Self, rcgen::Error> {
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = OffsetDateTime::now_utc() - time::Duration::hours(1);
        params.not_after = OffsetDateTime::now_utc() + time::Duration::days(1);
        let key = KeyPair::generate()?;
        let cert = params.self_signed(&key)?;
        Ok(Self { cert, params, key })
    }

    /// The CA certificate, DER.
    pub fn der(&self) -> Vec<u8> {
        self.cert.der().to_vec()
    }

    /// Issues a leaf with no extended key usage extension at all, as a general-purpose CA might.
    pub fn issue_without_eku(&self, spec: &NodeCert) -> Result<Minted, rcgen::Error> {
        self.mint(spec, false)
    }

    /// Issues a leaf.
    pub fn issue(&self, spec: &NodeCert) -> Result<Minted, rcgen::Error> {
        self.mint(spec, true)
    }

    fn mint(&self, spec: &NodeCert, with_eku: bool) -> Result<Minted, rcgen::Error> {
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(DnType::CommonName, "dev-node");
        for uri in &spec.uris {
            params
                .subject_alt_names
                .push(SanType::URI(uri.clone().try_into()?));
        }
        for dns in &spec.dns {
            params
                .subject_alt_names
                .push(SanType::DnsName(dns.clone().try_into()?));
        }
        params.not_before = OffsetDateTime::now_utc() + time::Duration::seconds(spec.not_before);
        params.not_after = OffsetDateTime::now_utc() + time::Duration::seconds(spec.not_after);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = if !with_eku {
            vec![]
        } else if spec.client_and_server {
            vec![
                ExtendedKeyUsagePurpose::ClientAuth,
                ExtendedKeyUsagePurpose::ServerAuth,
            ]
        } else {
            vec![ExtendedKeyUsagePurpose::ServerAuth]
        };
        let key = KeyPair::generate()?;
        let issuer = rcgen::Issuer::from_params(&self.params, &self.key);
        let cert = params.signed_by(&key, &issuer)?;
        Ok(Minted {
            der: cert.der().to_vec(),
            key_pkcs8: key.serialize_der(),
        })
    }
}
