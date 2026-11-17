//! The AES-GCM bytes are the same on every backend (M05.06).
//!
//! The expected digests come from Python's `cryptography` (OpenSSL), which shares no code with this crate. CI
//! runs this file twice: with the detected hardware paths and with `--cfg aes_backend="soft" --cfg
//! polyval_backend="soft"`, so a hardware path and the software path are held to the same bytes.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};
use hsearch_security::backend::{Impl, report};
use sha2::{Digest, Sha256};

const AAD: &[u8] = b"hsearch backend kat";

fn key() -> [u8; 32] {
    std::array::from_fn(|i| (i * 7 + 3) as u8)
}

fn nonce() -> [u8; 12] {
    std::array::from_fn(|i| i as u8)
}

fn plain(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 + n) as u8).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const EXPECTED: &[(usize, &str)] = &[
    (
        0,
        "aa5c564c13efccde23ab243692bf4bedbc219bee0672dc04648a2cd48fa2de19",
    ),
    (
        1,
        "2458945ac4efa8a11e86a4fa7bc634ffaf2182b5da870918e3f75ef6e43775a1",
    ),
    (
        15,
        "b2faf2dc090232e23695fab51dcdec3014797502979cf4c87c8d8d95cda11330",
    ),
    (
        16,
        "06be97d8436d4b6a74ea8790a4da3aac980a620bdb19b1f9172cdf634a3230cc",
    ),
    (
        17,
        "767b2b906624c93f099e17275a80bda289c6c4e00adfb45b2c1a2ec59fd58ff4",
    ),
    (
        31,
        "3f9626b240f5b05859dacf6afbadc4a233fb49185afc8904b9da16ef7b1d9833",
    ),
    (
        63,
        "0964ba229bcde70037c94af02798ee00869801c745b24ded97443b84baedff80",
    ),
    (
        64,
        "37d1e44e80f16ee3991f4dd6e9a801585111c0a3d2ff231d276fc39925fb38ab",
    ),
    (
        65,
        "94137f20d6e580fb09dac71bb8f4e0f991c0f493808ce14e02c94d3362528224",
    ),
    (
        255,
        "92f38adcd19c8ce25ca6ef15848afd9e0b683efd7aed2f25bf443ad3d848f80d",
    ),
    (
        256,
        "993c246694928f6182efe8fe4b794b3db4257c2309740ded5a449ce06c5a9b68",
    ),
    (
        1023,
        "2baff466a0bac4f18a6cfb386bbe7ddc68d0a76c265dc6a133c8227b7ee0bc11",
    ),
    (
        4096,
        "36f0752cf1b05fdcfd4f49e82e05da0f37b5416094b7b1add912fe56c38db773",
    ),
    (
        65537,
        "3f955e85b31ac9214492d5e6c07bccf4e46bd7ad5bb7431d0a0641f6069579d7",
    ),
];

fn cipher() -> Aes256Gcm {
    Aes256Gcm::new_from_slice(&key()).expect("32-byte key")
}

#[test]
fn ciphertext_matches_the_independent_implementation_at_every_length() {
    println!("backend: {}", report());
    for (n, digest) in EXPECTED {
        let nonce = nonce();
        let ct = cipher()
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: &plain(*n),
                    aad: AAD,
                },
            )
            .expect("encrypt");
        assert_eq!(ct.len(), n + 16);
        assert_eq!(hex(&Sha256::digest(&ct)), *digest, "length {n}");
    }
}

#[test]
fn decrypt_round_trips_and_every_flipped_bit_in_a_small_message_fails() {
    let nonce = nonce();
    let msg = plain(40);
    let ct = cipher()
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: &msg,
                aad: AAD,
            },
        )
        .expect("encrypt");
    let back = cipher()
        .decrypt((&nonce).into(), Payload { msg: &ct, aad: AAD })
        .expect("decrypt");
    assert_eq!(back, msg);
    for byte in 0..ct.len() {
        for bit in 0..8 {
            let mut bad = ct.clone();
            bad[byte] ^= 1 << bit;
            assert!(
                cipher()
                    .decrypt(
                        (&nonce).into(),
                        Payload {
                            msg: &bad,
                            aad: AAD
                        }
                    )
                    .is_err(),
                "flip byte {byte} bit {bit} was accepted"
            );
        }
    }
    let wrong_aad = Payload {
        msg: &ct,
        aad: b"other",
    };
    assert!(cipher().decrypt((&nonce).into(), wrong_aad).is_err());
}

#[test]
fn the_report_is_consistent_with_the_build_and_the_cpu() {
    let r = report();
    assert!(["x86_64", "x86", "aarch64", "other"].contains(&r.arch));
    if r.forced_soft {
        assert_eq!(
            r.aes,
            Impl::Soft,
            "forced software must not run AES in hardware"
        );
        assert_eq!(
            r.ghash,
            Impl::Soft,
            "forced software must not run GHASH in hardware"
        );
        assert!(r.any_soft());
    }
    // Hardware is only ever claimed where the CPU has the instructions.
    if r.aes == Impl::Hardware {
        assert!(r.cpu.aes);
    }
    if r.ghash == Impl::Hardware {
        assert!(r.cpu.carryless_multiply);
    }
    // A CPU without the instructions is software, never an unsupported-instruction path.
    if !r.cpu.aes {
        assert_eq!(r.aes, Impl::Soft);
    }
    assert!(r.to_string().contains(r.aes.as_str()));
}

#[test]
fn a_forced_software_build_is_what_the_report_says_it_is() {
    // The CI step sets HSEARCH_EXPECT_FORCED_SOFT=1 beside the cfg switches; the two must agree.
    if std::env::var("HSEARCH_EXPECT_FORCED_SOFT").as_deref() == Ok("1") {
        let r = report();
        assert!(r.forced_soft);
        assert_eq!((r.aes, r.ghash), (Impl::Soft, Impl::Soft));
    } else if let Ok(v) = std::env::var("HSEARCH_EXPECT_FORCED_SOFT") {
        assert_eq!(v, "0");
        assert!(!report().forced_soft);
    }
}
