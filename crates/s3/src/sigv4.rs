//! AWS Signature Version 4 for the S3 subset this adapter sends.
//!
//! Pure functions over strings and bytes; no clock and no network. The caller
//! supplies the timestamp, so the signer is deterministic and testable against
//! published AWS vectors.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Lowercase hex of a SHA-256 digest.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Lowercase hex encoding.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    out
}

/// Decode lowercase or uppercase hex; `None` on odd length or a bad digit.
pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    bytes
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

fn mac(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC accepts keys of any length, so construction cannot fail.
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Credentials and scope for signing. Secrets never appear in `Debug`.
#[derive(Clone)]
pub(crate) struct Signer {
    access_key: String,
    secret_key: String,
    region: String,
    service: String,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("region", &self.region)
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl Signer {
    pub(crate) fn new(access_key: &str, secret_key: &str, region: &str, service: &str) -> Self {
        Self {
            access_key: access_key.to_owned(),
            secret_key: secret_key.to_owned(),
            region: region.to_owned(),
            service: service.to_owned(),
        }
    }

    /// Return the `Authorization` header value.
    ///
    /// `headers` are lowercase names with trimmed values and must include
    /// `host` and `x-amz-date`. `canonical_query` must already be sorted and
    /// encoded. `amz_date` is `YYYYMMDDTHHMMSSZ`.
    pub(crate) fn authorization(
        &self,
        method: &str,
        canonical_uri: &str,
        canonical_query: &str,
        headers: &[(String, String)],
        payload_hash: &str,
        amz_date: &str,
    ) -> String {
        let mut sorted: Vec<&(String, String)> = headers.iter().collect();
        sorted.sort();
        let canonical_headers: String = sorted.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let signed: Vec<&str> = sorted.iter().map(|(k, _)| k.as_str()).collect();
        let signed = signed.join(";");
        let canonical = format!(
            "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed}\n{payload_hash}"
        );
        let day = &amz_date[..8];
        let scope = format!("{day}/{}/{}/aws4_request", self.region, self.service);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical.as_bytes())
        );
        let k_date = mac(
            format!("AWS4{}", self.secret_key).as_bytes(),
            day.as_bytes(),
        );
        let k_region = mac(&k_date, self.region.as_bytes());
        let k_service = mac(&k_region, self.service.as_bytes());
        let k_signing = mac(&k_service, b"aws4_request");
        let signature = hex(&mac(&k_signing, to_sign.as_bytes()));
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            self.access_key
        )
    }
}

/// Percent-encode every byte except unreserved characters (and `/` when asked).
pub(crate) fn encode(bytes: &[u8], keep_slash: bool) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            b'/' if keep_slash => out.push('/'),
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}

/// `YYYYMMDDTHHMMSSZ` for a Unix timestamp in seconds (proleptic Gregorian, UTC).
pub(crate) fn amz_date(unix_seconds: u64) -> String {
    let days = unix_seconds / 86_400;
    let rem = unix_seconds % 86_400;
    // Civil-from-days (Howard Hinnant), valid for the supported range.
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_published_aws_get_vanilla_vector() {
        let signer = Signer::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "service",
        );
        let headers = vec![
            ("host".to_owned(), "example.amazonaws.com".to_owned()),
            ("x-amz-date".to_owned(), "20150830T123600Z".to_owned()),
        ];
        let auth = signer.authorization(
            "GET",
            "/",
            "",
            &headers,
            &sha256_hex(b""),
            "20150830T123600Z",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn debug_hides_secrets() {
        let signer = Signer::new("access-id", "very-secret", "r", "s3");
        let text = format!("{signer:?}");
        assert!(!text.contains("very-secret") && !text.contains("access-id"));
    }

    #[test]
    fn hex_round_trips_and_rejects_garbage() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert_eq!(unhex("000FfF"), Some(vec![0, 15, 255]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }

    #[test]
    fn encode_escapes_everything_but_unreserved() {
        assert_eq!(encode(b"a b/c+d~", true), "a%20b/c%2Bd~");
        assert_eq!(encode(b"a/b", false), "a%2Fb");
        assert_eq!(encode(&[0xff], true), "%FF");
    }

    #[test]
    fn dates_are_formatted_in_utc() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(amz_date(1_709_164_799), "20240228T235959Z");
        assert_eq!(amz_date(1_709_164_800), "20240229T000000Z");
    }
}
