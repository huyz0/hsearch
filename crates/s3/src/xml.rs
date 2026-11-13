//! Minimal reader for the one XML document this adapter parses: the
//! `ListObjectVersions` response. Delete markers and unrelated elements are skipped.

/// One object version in newest-first listing order.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Listed {
    pub(crate) key: Vec<u8>,
    pub(crate) version_id: String,
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
