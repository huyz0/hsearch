//! Minimal reader for the one XML document this adapter parses: the
//! `ListObjectVersions` response. Delete markers and unrelated elements are skipped.

/// One object version in newest-first listing order.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Listed {
    pub(crate) key: Vec<u8>,
    pub(crate) version_id: String,
    /// The version's ETag as the provider prints it (quotes included), when listed.
    pub(crate) etag: Option<String>,
    /// `LastModified` in Unix milliseconds, when listed and well formed.
    pub(crate) modified_ms: Option<u64>,
}

/// A parsed page of versions.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Page {
    pub(crate) versions: Vec<Listed>,
    pub(crate) truncated: bool,
}

fn tag<'a>(block: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&close)? + start;
    Some(&block[start..end])
}

fn unescape(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.extend_from_slice(&rest.as_bytes()[..at]);
        rest = &rest[at..];
        let Some(semi) = rest.find(';') else { break };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some(b'&'),
            "lt" => Some(b'<'),
            "gt" => Some(b'>'),
            "quot" => Some(b'"'),
            "apos" => Some(b'\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok())),
        };
        match decoded {
            Some(byte) => out.push(byte),
            None => out.extend_from_slice(&rest.as_bytes()[..=semi]),
        }
        rest = &rest[semi + 1..];
    }
    out.extend_from_slice(rest.as_bytes());
    out
}

/// `2026-11-17T22:30:00.000Z` (fraction optional, UTC only) as Unix milliseconds.
fn unix_ms(text: &str) -> Option<u64> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut t = clock.split(':');
    let (h, mi, sec): (u64, u64, u64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    if d.next().is_some()
        || !(1..=12).contains(&m)
        || !(1..=31).contains(&day)
        || h > 23
        || mi > 59
        || sec > 60
    {
        return None;
    }
    let millis: u64 = format!("{fraction:0<3}").get(..3)?.parse().ok()?;
    // Days from 1970-01-01 (Howard Hinnant's civil-from-days, inverted).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = u64::try_from(era * 146_097 + doe - 719_468).ok()?;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + sec * 1000 + millis)
}

/// Parse a `ListObjectVersions` body. `None` when it has no result element.
pub(crate) fn parse_versions(body: &str) -> Option<Page> {
    if !body.contains("<ListVersionsResult") {
        return None;
    }
    let mut versions = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("<Version>") {
        let after = &rest[start + "<Version>".len()..];
        let end = after.find("</Version>")?;
        let block = &after[..end];
        versions.push(Listed {
            key: unescape(tag(block, "Key")?),
            version_id: String::from_utf8(unescape(tag(block, "VersionId")?)).ok()?,
            etag: tag(block, "ETag").and_then(|t| String::from_utf8(unescape(t)).ok()),
            modified_ms: tag(block, "LastModified").and_then(unix_ms),
        });
        rest = &after[end..];
    }
    Some(Page {
        versions,
        truncated: tag(body, "IsTruncated") == Some("true"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "<?xml version=\"1.0\"?><ListVersionsResult><Name>b</Name>\
        <IsTruncated>true</IsTruncated>\
        <Version><Key>a&amp;b</Key><VersionId>v2</VersionId><IsLatest>true</IsLatest></Version>\
        <DeleteMarker><Key>a&amp;b</Key><VersionId>dm</VersionId></DeleteMarker>\
        <Version><Key>a&amp;b</Key><VersionId>v1</VersionId></Version></ListVersionsResult>";

    #[test]
    fn lists_versions_in_order_and_skips_delete_markers() {
        let page = parse_versions(BODY).unwrap();
        assert!(page.truncated);
        let ids: Vec<_> = page
            .versions
            .iter()
            .map(|v| v.version_id.as_str())
            .collect();
        assert_eq!(ids, ["v2", "v1"]);
        assert_eq!(page.versions[0].key, b"a&b");
    }

    #[test]
    fn etag_and_last_modified_are_read_when_listed() {
        let page = parse_versions(
            "<ListVersionsResult><IsTruncated>false</IsTruncated><Version><Key>k</Key>\
             <VersionId>v</VersionId><LastModified>2026-11-17T22:30:00.250Z</LastModified>\
             <ETag>&quot;abc&quot;</ETag></Version></ListVersionsResult>",
        )
        .unwrap();
        assert_eq!(page.versions[0].etag.as_deref(), Some("\"abc\""));
        // 2026-11-17 is day 20,774 since the epoch: 20409 * 86400 + 22.5 h.
        assert_eq!(
            page.versions[0].modified_ms,
            Some(20_774 * 86_400_000 + 81_000_250)
        );
        assert_eq!(unix_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_ms("2000-03-01T00:00:00.5Z"), Some(951_868_800_500));
        assert_eq!(unix_ms("garbage"), None);
        assert_eq!(unix_ms("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn empty_and_foreign_documents() {
        let empty = parse_versions(
            "<ListVersionsResult><IsTruncated>false</IsTruncated></ListVersionsResult>",
        );
        assert_eq!(
            empty,
            Some(Page {
                versions: vec![],
                truncated: false
            })
        );
        assert_eq!(parse_versions("<Error><Code>X</Code></Error>"), None);
    }

    #[test]
    fn malformed_version_blocks_are_refused() {
        assert_eq!(
            parse_versions(
                "<ListVersionsResult><Version><Key>k</Key></Version></ListVersionsResult>"
            ),
            None
        );
        assert_eq!(parse_versions("<ListVersionsResult><Version><Key>k"), None);
    }

    #[test]
    fn entities_decode_or_pass_through() {
        assert_eq!(unescape("&lt;&gt;&quot;&apos;&#65;&#x42;"), b"<>\"'AB");
        assert_eq!(unescape("a&unknown;b&"), b"a&unknown;b&");
    }
}
