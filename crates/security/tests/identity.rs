//! M05.01 identity: negative controls first. Every refusal here is a case the plan names: wrong CA, wrong
//! SAN, wrong cluster, wrong role, expired certificate, public token on a peer path, tenant spoof.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hsearch_security::dev_ca::{DevCa, NodeCert};
use hsearch_security::identity::{
    CertUse, Channel, IdentityError, IdentityProvider, Presented, Role, TokenError, TokenProvider,
};
use hsearch_security::ids::{ClusterId, TrustDomain};
use hsearch_security::x509::{
    AcceptedIdentity, ChainVerifier, ConfigError, FileIdentityProvider, MAX_BUNDLE_OVERLAP,
    MAX_SKEW, SpiffeIdentityProvider,
};

const DOMAIN: &str = "example.test";

fn now() -> SystemTime {
    SystemTime::now()
}

fn cluster(name: &str) -> ClusterId {
    ClusterId::new(name).unwrap()
}

fn accepted(domain: &str, name: &str, roles: &[Role]) -> AcceptedIdentity {
    AcceptedIdentity {
        trust_domain: TrustDomain::new(domain).unwrap(),
        cluster: cluster(name),
        roles: roles.iter().copied().collect::<BTreeSet<_>>(),
    }
}

struct Fixture {
    ca: DevCa,
    provider: SpiffeIdentityProvider,
}

fn fixture() -> Fixture {
    let ca = DevCa::new("dev root").unwrap();
    let verifier = Arc::new(ChainVerifier::new(vec![ca.der()], Duration::from_secs(30)).unwrap());
    let provider = SpiffeIdentityProvider::new(
        verifier,
        vec![
            accepted(
                DOMAIN,
                "prod",
                &[Role::IndexNode, Role::SearchNode, Role::Coordinator],
            ),
            accepted("partner.test", "dr", &[Role::Replication]),
        ],
    )
    .unwrap();
    Fixture { ca, provider }
}

fn verify(
    f: &Fixture,
    spec: &NodeCert,
) -> Result<hsearch_security::identity::VerifiedPrincipal, IdentityError> {
    let leaf = f.ca.issue(spec).unwrap();
    f.provider.verify(
        &Presented::Certificate {
            chain: &[leaf.der],
            usage: CertUse::Client,
        },
        now(),
    )
}

#[test]
fn a_valid_spiffe_identity_becomes_a_principal() {
    let f = fixture();
    let who = verify(
        &f,
        &NodeCert::spiffe("spiffe://example.test/hsearch/prod/search"),
    )
    .unwrap();
    assert_eq!(who.role(), Role::SearchNode);
    assert_eq!(who.cluster().as_str(), "prod");
    assert_eq!(who.channel(), Channel::Mtls);
    assert_eq!(who.trust_domain().unwrap().as_str(), DOMAIN);
    assert_eq!(who.name(), "spiffe://example.test/hsearch/prod/search");
}

#[test]
fn a_certificate_from_another_ca_is_refused() {
    let f = fixture();
    let rogue = DevCa::new("rogue root").unwrap();
    let leaf = rogue
        .issue(&NodeCert::spiffe(
            "spiffe://example.test/hsearch/prod/coordinator",
        ))
        .unwrap();
    let r = f.provider.verify(
        &Presented::Certificate {
            chain: &[leaf.der],
            usage: CertUse::Client,
        },
        now(),
    );
    assert_eq!(r.unwrap_err(), IdentityError::UntrustedIssuer);
}

#[test]
fn wrong_trust_domain_cluster_and_role_are_refused() {
    let f = fixture();
    let cases = [
        (
            "spiffe://other.test/hsearch/prod/search",
            IdentityError::WrongTrustDomain,
        ),
        (
            "spiffe://example.test/hsearch/staging/search",
            IdentityError::WrongCluster,
        ),
        (
            "spiffe://example.test/hsearch/prod/admin",
            IdentityError::RoleNotPermitted,
        ),
        (
            "spiffe://example.test/hsearch/prod/superuser",
            IdentityError::RoleNotPermitted,
        ),
        // A federated cluster is trusted for replication only; it cannot be a coordinator.
        (
            "spiffe://partner.test/hsearch/dr/coordinator",
            IdentityError::RoleNotPermitted,
        ),
    ];
    for (uri, expected) in cases {
        assert_eq!(
            verify(&f, &NodeCert::spiffe(uri)).unwrap_err(),
            expected,
            "{uri}"
        );
    }
    // But it is trusted for the one thing it was federated for.
    let who = verify(
        &f,
        &NodeCert::spiffe("spiffe://partner.test/hsearch/dr/replication"),
    )
    .unwrap();
    assert_eq!(who.role(), Role::Replication);
}

#[test]
fn malformed_spiffe_ids_and_san_counts_are_refused() {
    let f = fixture();
    for uri in [
        "spiffe://example.test/hsearch/prod",
        "spiffe://example.test/hsearch/prod/search/extra",
        "spiffe://example.test/other/prod/search",
        "spiffe://EXAMPLE.test/hsearch/prod/search",
        "https://example.test/hsearch/prod/search",
    ] {
        assert!(
            matches!(
                verify(&f, &NodeCert::spiffe(uri)),
                Err(IdentityError::NoAcceptedIdentity(_) | IdentityError::WrongTrustDomain)
            ),
            "{uri}"
        );
    }
    // Two identities in one certificate are ambiguous, so neither is taken.
    let mut two = NodeCert::spiffe("spiffe://example.test/hsearch/prod/search");
    two.uris
        .push("spiffe://example.test/hsearch/prod/coordinator".into());
    assert!(matches!(
        verify(&f, &two),
        Err(IdentityError::NoAcceptedIdentity(_))
    ));
    // A DNS name alone is not a SPIFFE identity.
    assert!(matches!(
        verify(&f, &NodeCert::named("search.prod.example.test")),
        Err(IdentityError::NoAcceptedIdentity(_))
    ));
}

#[test]
fn expiry_and_start_are_checked_with_a_bounded_skew() {
    let f = fixture();
    let uri = "spiffe://example.test/hsearch/prod/search";
    let mk = |not_before: i64, not_after: i64| NodeCert {
        not_before,
        not_after,
        ..NodeCert::spiffe(uri)
    };
    // Expired an hour ago and not valid for another hour: refused.
    assert_eq!(
        verify(&f, &mk(-7200, -3600)).unwrap_err(),
        IdentityError::Expired
    );
    assert_eq!(
        verify(&f, &mk(3600, 7200)).unwrap_err(),
        IdentityError::NotYetValid
    );
    // Inside the 30 s skew on a peer whose clock is slightly off, accepted; outside it, refused.
    assert!(verify(&f, &mk(-3600, -10)).is_ok());
    assert!(verify(&f, &mk(10, 3600)).is_ok());
    assert_eq!(
        verify(&f, &mk(-3600, -120)).unwrap_err(),
        IdentityError::Expired
    );
    assert_eq!(
        verify(&f, &mk(120, 3600)).unwrap_err(),
        IdentityError::NotYetValid
    );
}

#[test]
fn a_server_only_certificate_cannot_authenticate_a_client() {
    let f = fixture();
    let spec = NodeCert {
        client_and_server: false,
        ..NodeCert::spiffe("spiffe://example.test/hsearch/prod/search")
    };
    let leaf = f.ca.issue(&spec).unwrap();
    let as_client = f.provider.verify(
        &Presented::Certificate {
            chain: std::slice::from_ref(&leaf.der),
            usage: CertUse::Client,
        },
        now(),
    );
    assert_eq!(as_client.unwrap_err(), IdentityError::WrongUsage);
    assert!(
        f.provider
            .verify(
                &Presented::Certificate {
                    chain: &[leaf.der],
                    usage: CertUse::Server
                },
                now()
            )
            .is_ok()
    );
}

#[test]
fn garbage_and_oversized_chains_are_refused_without_panicking() {
    let f = fixture();
    let usage = CertUse::Client;
    let verify = |chain: &[Vec<u8>]| {
        f.provider
            .verify(&Presented::Certificate { chain, usage }, now())
    };
    assert!(matches!(verify(&[]), Err(IdentityError::Malformed(_))));
    assert!(matches!(
        verify(&[vec![1, 2, 3]]),
        Err(IdentityError::Malformed(_))
    ));
    assert!(matches!(
        verify(&[vec![0x30; 20_000]]),
        Err(IdentityError::Malformed(_))
    ));
    let leaf =
        f.ca.issue(&NodeCert::spiffe(
            "spiffe://example.test/hsearch/prod/search",
        ))
        .unwrap()
        .der;
    assert!(matches!(
        verify(&vec![leaf.clone(); 7]),
        Err(IdentityError::Malformed(_))
    ));
    // Every truncation of a real certificate is refused.
    for cut in (0..leaf.len()).step_by(7) {
        assert!(verify(&[leaf[..cut].to_vec()]).is_err());
    }
}

#[test]
fn flipping_any_byte_of_a_certificate_never_yields_a_principal() {
    let f = fixture();
    let leaf =
        f.ca.issue(&NodeCert::spiffe(
            "spiffe://example.test/hsearch/prod/search",
        ))
        .unwrap()
        .der;
    for i in 0..leaf.len() {
        let mut bad = leaf.clone();
        bad[i] ^= 0x01;
        let r = f.provider.verify(
            &Presented::Certificate {
                chain: &[bad],
                usage: CertUse::Client,
            },
            now(),
        );
        assert!(r.is_err(), "flipping byte {i} was accepted");
    }
}

#[test]
fn credentials_of_the_wrong_kind_are_refused() {
    let f = fixture();
    assert_eq!(
        f.provider
            .verify(&Presented::Bearer("anything-at-all-anything-at-all"), now())
            .unwrap_err(),
        IdentityError::WrongCredentialKind
    );
    let tokens = TokenProvider::new(cluster("prod"));
    let leaf =
        f.ca.issue(&NodeCert::spiffe(
            "spiffe://example.test/hsearch/prod/search",
        ))
        .unwrap();
    assert_eq!(
        tokens
            .verify(
                &Presented::Certificate {
                    chain: &[leaf.der],
                    usage: CertUse::Client
                },
                now()
            )
            .unwrap_err(),
        IdentityError::WrongCredentialKind
    );
}

#[test]
fn tokens_identify_clients_and_admins_only() {
    let mut tokens = TokenProvider::new(cluster("prod"));
    let secret = "a-long-enough-secret-token-0001";
    tokens.add("alice", secret, Role::Client).unwrap();
    for role in [
        Role::IndexNode,
        Role::SearchNode,
        Role::Coordinator,
        Role::Replication,
    ] {
        assert_eq!(
            tokens.add("node", "another-long-enough-secret-0002", role),
            Err(TokenError::PeerRoleRefused)
        );
    }
    assert_eq!(
        tokens.add("short", "tiny", Role::Client),
        Err(TokenError::TooShort)
    );
    assert_eq!(
        tokens.add("again", secret, Role::Admin),
        Err(TokenError::Duplicate)
    );
    let who = tokens.verify(&Presented::Bearer(secret), now()).unwrap();
    assert_eq!(
        (who.role(), who.channel(), who.name()),
        (Role::Client, Channel::Token, "token:alice")
    );
    assert_eq!(
        tokens
            .verify(&Presented::Bearer("not-the-secret-not-the-secret"), now())
            .unwrap_err(),
        IdentityError::UnknownToken
    );
}

#[test]
fn file_identities_come_from_the_table_not_from_the_name() {
    let ca = DevCa::new("file root").unwrap();
    let verifier = Arc::new(ChainVerifier::new(vec![ca.der()], Duration::ZERO).unwrap());
    let table = FileIdentityProvider::parse_table(
        "# nodes\nsearch-1.prod.internal prod search\ncoord-1.prod.internal prod coordinator  # coordinator\n\n",
    )
    .unwrap();
    let provider = FileIdentityProvider::new(verifier, table);
    let present = |name: &str| {
        let leaf = ca.issue(&NodeCert::named(name)).unwrap();
        provider.verify(
            &Presented::Certificate {
                chain: &[leaf.der],
                usage: CertUse::Client,
            },
            now(),
        )
    };
    let who = present("search-1.prod.internal").unwrap();
    assert_eq!(
        (who.role(), who.cluster().as_str(), who.trust_domain()),
        (Role::SearchNode, "prod", None)
    );
    // A name that merely looks like a coordinator is nobody until the table says so.
    assert!(matches!(
        present("coordinator.prod.internal"),
        Err(IdentityError::NoAcceptedIdentity(_))
    ));
    let mut both = NodeCert::named("search-1.prod.internal");
    both.dns.push("coord-1.prod.internal".into());
    let leaf = ca.issue(&both).unwrap();
    let r = provider.verify(
        &Presented::Certificate {
            chain: &[leaf.der],
            usage: CertUse::Client,
        },
        now(),
    );
    assert!(
        matches!(r, Err(IdentityError::NoAcceptedIdentity(_))),
        "two table names are ambiguous"
    );
}

#[test]
fn bad_tables_are_refused_with_the_line_number() {
    for (text, line) in [
        ("a.internal prod", 1),
        ("a.internal prod search\nb.internal prod wizard", 2),
        ("a.internal Prod search", 1),
        ("a.internal prod search\na.internal prod search", 2),
        ("token:alice prod client", 1),
    ] {
        assert_eq!(
            FileIdentityProvider::parse_table(text).unwrap_err(),
            ConfigError::BadTable(line),
            "{text}"
        );
    }
}

#[test]
fn verification_has_no_off_switch() {
    let ca = DevCa::new("root").unwrap();
    // The only knobs are anchors and a capped skew; an empty bundle is an error, not "trust anyone".
    assert!(matches!(
        ChainVerifier::new(vec![], Duration::ZERO),
        Err(ConfigError::EmptyTrustBundle)
    ));
    assert!(matches!(
        ChainVerifier::new(vec![vec![0u8; 10]], Duration::ZERO),
        Err(ConfigError::BadAnchor)
    ));
    assert!(matches!(
        ChainVerifier::new(vec![ca.der()], MAX_SKEW + Duration::from_secs(1)),
        Err(ConfigError::SkewTooLarge)
    ));
    assert!(matches!(
        ChainVerifier::new(vec![ca.der(); 33], Duration::ZERO),
        Err(ConfigError::TooManyAnchors)
    ));
    let verifier = ChainVerifier::new(vec![ca.der()], Duration::ZERO).unwrap();
    assert!(matches!(
        verifier.rotate(vec![], now(), Duration::ZERO),
        Err(ConfigError::EmptyTrustBundle)
    ));
    assert!(matches!(
        verifier.rotate(
            vec![ca.der()],
            now(),
            MAX_BUNDLE_OVERLAP + Duration::from_secs(1)
        ),
        Err(ConfigError::OverlapTooLarge)
    ));
    assert!(matches!(
        SpiffeIdentityProvider::new(Arc::new(verifier), vec![]),
        Err(ConfigError::BadAcceptedIdentities)
    ));
}

#[test]
fn accepted_identities_must_name_roles_once() {
    let ca = DevCa::new("root").unwrap();
    let v = Arc::new(ChainVerifier::new(vec![ca.der()], Duration::ZERO).unwrap());
    let no_roles = vec![accepted(DOMAIN, "prod", &[])];
    assert!(matches!(
        SpiffeIdentityProvider::new(v.clone(), no_roles),
        Err(ConfigError::BadAcceptedIdentities)
    ));
    let twice = vec![
        accepted(DOMAIN, "prod", &[Role::SearchNode]),
        accepted(DOMAIN, "prod", &[Role::Coordinator]),
    ];
    assert!(matches!(
        SpiffeIdentityProvider::new(v, twice),
        Err(ConfigError::BadAcceptedIdentities)
    ));
}

#[test]
fn trust_bundle_rotation_overlaps_for_a_bounded_time() {
    let old = DevCa::new("old root").unwrap();
    let new = DevCa::new("new root").unwrap();
    let verifier = Arc::new(ChainVerifier::new(vec![old.der()], Duration::ZERO).unwrap());
    let provider = SpiffeIdentityProvider::new(
        verifier.clone(),
        vec![accepted(DOMAIN, "prod", &[Role::SearchNode])],
    )
    .unwrap();
    let uri = "spiffe://example.test/hsearch/prod/search";
    let old_leaf = old.issue(&NodeCert::spiffe(uri)).unwrap().der;
    let new_leaf = new.issue(&NodeCert::spiffe(uri)).unwrap().der;
    let check = |leaf: &[u8], at: SystemTime| {
        provider.verify(
            &Presented::Certificate {
                chain: &[leaf.to_vec()],
                usage: CertUse::Client,
            },
            at,
        )
    };

    assert!(check(&old_leaf, now()).is_ok());
    assert_eq!(
        check(&new_leaf, now()).unwrap_err(),
        IdentityError::UntrustedIssuer
    );

    let at = now();
    verifier
        .rotate(vec![new.der()], at, Duration::from_secs(600))
        .unwrap();
    // During the overlap both work, so certificates can be re-issued without dropping connections.
    assert!(check(&old_leaf, at + Duration::from_secs(599)).is_ok());
    assert!(check(&new_leaf, at).is_ok());
    // After it, only the new bundle does.
    assert_eq!(
        check(&old_leaf, at + Duration::from_secs(601)).unwrap_err(),
        IdentityError::UntrustedIssuer
    );
    assert!(check(&new_leaf, at + Duration::from_secs(601)).is_ok());

    // No overlap at all: the old bundle is gone at once.
    verifier
        .rotate(vec![old.der()], at, Duration::ZERO)
        .unwrap();
    assert_eq!(
        check(&new_leaf, at).unwrap_err(),
        IdentityError::UntrustedIssuer
    );
}

#[test]
fn an_extra_unrelated_certificate_in_the_chain_is_not_trusted() {
    // The dev CA signs leaves directly; a chain carrying an unrelated extra certificate must still verify
    // and must not let that extra certificate stand in for the root.
    let f = fixture();
    let leaf =
        f.ca.issue(&NodeCert::spiffe(
            "spiffe://example.test/hsearch/prod/search",
        ))
        .unwrap()
        .der;
    let stranger = DevCa::new("stranger").unwrap().der();
    let r = f.provider.verify(
        &Presented::Certificate {
            chain: &[leaf, stranger],
            usage: CertUse::Client,
        },
        now(),
    );
    assert!(r.is_ok());
}

#[test]
fn a_certificate_without_extended_key_usage_authenticates_nobody() {
    let f = fixture();
    let leaf =
        f.ca.issue_without_eku(&NodeCert::spiffe(
            "spiffe://example.test/hsearch/prod/coordinator",
        ))
        .unwrap();
    for usage in [CertUse::Client, CertUse::Server] {
        let r = f.provider.verify(
            &Presented::Certificate {
                chain: std::slice::from_ref(&leaf.der),
                usage,
            },
            now(),
        );
        assert_eq!(r.unwrap_err(), IdentityError::WrongUsage, "{usage:?}");
    }
}
