//! Index mappings: explicit, typed, finite. Stored with the index so a restarted
//! node can rebuild its schema from the object store alone.
use hsearch_engine::{Kind, Limits, Schema};
use hsearch_format::{Digest, MappingObject};
use hsearch_index::Field;
use serde_json::Value;
use std::sync::Arc;

use crate::ApiError;
use crate::date::parse_date;
use crate::policy::{Dynamic, MappingLimits};

/// Declared fields, sorted by name so the schema order is canonical.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mapping {
    pub(crate) fields: Vec<(String, Kind)>,
    /// What the index does with an undeclared field. `Strict`, the behaviour
    /// before the setting existed, is the default and is not written into the
    /// stored form, so every mapping stored earlier keeps its hash.
    pub(crate) dynamic: Dynamic,
}

const INLINE_HEADER: &str = "hsearch-mapping-v0\n";
/// A stored line naming a non-default dynamic setting. Field names cannot start
/// with `#`, so it is never a field.
const DYNAMIC_LINE: &str = "#dynamic\t";
const REFERENCE_HEADER: &str = "hsearch-mapping-ref-v0\n";

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Keyword => "keyword",
        Kind::Text => "text",
        Kind::Integer => "integer",
        Kind::Boolean => "boolean",
        Kind::Date => "date",
    }
}

fn kind_of(name: &str) -> Option<Kind> {
    Some(match name {
        "keyword" => Kind::Keyword,
        "text" => Kind::Text,
        "integer" => Kind::Integer,
        "boolean" => Kind::Boolean,
        "date" => Kind::Date,
        _ => return None,
    })
}

impl Mapping {
    /// Parse a create-index body: `{"mappings": {"properties": {...}}}`.
    pub fn parse(body: &Value) -> Result<Self, ApiError> {
        Self::parse_with(body, &MappingLimits::default())
    }

    /// [`Self::parse`] under explicit limits: a mapping over the field limit is
    /// refused whole.
    pub fn parse_with(body: &Value, limits: &MappingLimits) -> Result<Self, ApiError> {
        let object = body
            .as_object()
            .ok_or_else(|| ApiError::parse("the body must be a JSON object"))?;
        if let Some(extra) = object.keys().find(|k| *k != "mappings") {
            return Err(ApiError::parse(format!("unsupported key [{extra}]")));
        }
        let mappings = object
            .get("mappings")
            .and_then(Value::as_object)
            .ok_or_else(|| ApiError::parse("[mappings] is required"))?;
        if let Some(extra) = mappings
            .keys()
            .find(|k| *k != "properties" && *k != "dynamic")
        {
            return Err(ApiError::parse(format!(
                "unsupported mappings key [{extra}]"
            )));
        }
        let properties = mappings
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| ApiError::parse("[mappings.properties] is required"))?;
        let dynamic = mappings
            .get("dynamic")
            .map(Dynamic::parse)
            .transpose()?
            .unwrap_or(Dynamic::Strict);
        limits.check_fields(properties.len())?;
        let mut fields = Vec::new();
        for (name, definition) in properties {
            let definition = definition
                .as_object()
                .ok_or_else(|| ApiError::parse(format!("field [{name}] must be an object")))?;
            if let Some(extra) = definition.keys().find(|k| *k != "type") {
                return Err(ApiError::new(
                    400,
                    "mapper_parsing_exception",
                    format!("field [{name}] has unsupported parameter [{extra}]"),
                ));
            }
            let kind = definition
                .get("type")
                .and_then(Value::as_str)
                .and_then(kind_of)
                .ok_or_else(|| {
                    ApiError::new(
                        400,
                        "mapper_parsing_exception",
                        format!("field [{name}] needs a supported type"),
                    )
                })?;
            fields.push((name.clone(), kind));
        }
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        let mapping = Self { fields, dynamic };
        mapping.schema()?;
        Ok(mapping)
    }

    /// The v0 stored form, one `name<TAB>type` line per field.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = String::from(INLINE_HEADER);
        if self.dynamic != Dynamic::Strict {
            out.push_str(DYNAMIC_LINE);
            out.push_str(self.dynamic.name());
            out.push('\n');
        }
        for (name, kind) in &self.fields {
            out.push_str(name);
            out.push('\t');
            out.push_str(kind_name(*kind));
            out.push('\n');
        }
        out.into_bytes()
    }

    /// Read the stored form back.
    pub fn decode(bytes: &[u8]) -> Result<Self, ApiError> {
        let corrupt = || ApiError::new(500, "corrupt_exception", "stored mapping is unreadable");
        let text = std::str::from_utf8(bytes).map_err(|_| corrupt())?;
        let mut lines = text.lines();
        if lines.next() != Some(INLINE_HEADER.trim_end()) {
            return Err(corrupt());
        }
        let mut fields = Vec::new();
        let mut dynamic = Dynamic::Strict;
        for (i, line) in lines.enumerate() {
            if let Some(name) = line.strip_prefix(DYNAMIC_LINE) {
                // Only the first line, only a non-default value: the one form
                // `encode` writes, so a mapping has exactly one stored spelling.
                let parsed = Dynamic::from_name(name).map_err(|_| corrupt())?;
                if i != 0 || parsed == Dynamic::Strict {
                    return Err(corrupt());
                }
                dynamic = parsed;
                continue;
            }
            let (name, kind) = line.split_once('\t').ok_or_else(corrupt)?;
            fields.push((name.to_string(), kind_of(kind).ok_or_else(corrupt)?));
        }
        Ok(Self { fields, dynamic })
    }

    /// The content hash that names this mapping: SHA-256 of its canonical stored
    /// form, which depends on the declared fields and nothing else. The order the
    /// request wrote its keys in is already gone after parsing, so two spellings
    /// of one mapping share one hash.
    #[must_use]
    pub fn hash(&self) -> Digest {
        MappingObject::new(&self.encode())
            .expect("a parsed mapping is within the 1 MiB object bound")
            .hash
    }

    /// The immutable shared object for this mapping: envelope, hash and canonical bytes.
    pub fn to_object(&self) -> Result<Vec<u8>, ApiError> {
        let mut out = Vec::new();
        MappingObject::new(&self.encode())
            .and_then(|object| object.encode(&mut out))
            .map_err(|_| {
                ApiError::new(400, "mapper_parsing_exception", "the mapping is too large")
            })?;
        Ok(out)
    }

    /// Read a shared object back, requiring the hash the index's reference named.
    pub fn from_object(bytes: &[u8], expected: &Digest) -> Result<Self, ApiError> {
        let object = MappingObject::decode_expecting(bytes, expected).map_err(|_| {
            ApiError::new(
                500,
                "corrupt_exception",
                "a shared mapping object failed its hash",
            )
        })?;
        Self::decode(&object.canonical)
    }

    /// An index's stored reference to its shared mapping: a few dozen bytes
    /// naming the hash, in place of the mapping itself.
    #[must_use]
    pub fn reference(hash: &Digest) -> Vec<u8> {
        let mut out = String::from(REFERENCE_HEADER);
        for byte in hash {
            out.push_str(&format!("{byte:02x}"));
        }
        out.push('\n');
        out.into_bytes()
    }

    /// The hash a stored reference names, or `None` if the bytes are not a reference.
    #[must_use]
    pub fn parse_reference(bytes: &[u8]) -> Option<Digest> {
        let text = std::str::from_utf8(bytes).ok()?;
        let hex = text.strip_prefix(REFERENCE_HEADER)?.strip_suffix('\n')?;
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let mut hash = [0u8; 32];
        for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
            hash[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(hash)
    }

    /// Whether stored bytes are a mapping written inline by an earlier node.
    #[must_use]
    pub fn is_inline(bytes: &[u8]) -> bool {
        bytes.starts_with(INLINE_HEADER.as_bytes())
    }

    /// The engine schema, under the scale profile admitting documents up to the
    /// largest the index can hold.
    pub fn schema(&self) -> Result<Schema, ApiError> {
        self.schema_for(hsearch_index::MAX_INDEXED_DOCUMENT_BYTES)
    }

    /// The engine schema, under the scale profile admitting documents of up to
    /// `document_bytes`.
    pub fn schema_for(&self, document_bytes: usize) -> Result<Schema, ApiError> {
        let fields: Vec<(&str, Kind)> = self.fields.iter().map(|(n, k)| (n.as_str(), *k)).collect();
        Schema::new(1, &fields)
            .map_err(|_| {
                ApiError::new(
                    400,
                    "mapper_parsing_exception",
                    "field names must be 1-64 of [A-Za-z0-9_], not start with an underscore, and be unique; at most 14 fields",
                )
            })?
            .with_limits(Limits::scale().with_document_bytes(document_bytes))
            .map_err(ApiError::from)
    }

    pub(crate) fn kind(&self, name: &str) -> Option<Kind> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, k)| *k)
    }

    /// Declared field names in schema order.
    pub(crate) fn names(&self) -> Vec<&str> {
        self.fields.iter().map(|(n, _)| n.as_str()).collect()
    }
}

/// Convert a borrowed JSON value to the declared kind (queries: small values).
pub(crate) fn typed(kind: Kind, value: &Value) -> Result<Field, String> {
    typed_owned(kind, value.clone())
}

/// Convert one JSON value to the declared kind, moving its string instead of
/// copying it: the ingest path, where a value can be large. No coercion between kinds.
pub(crate) fn typed_owned(kind: Kind, value: Value) -> Result<Field, String> {
    match (kind, value) {
        (Kind::Keyword | Kind::Text, Value::String(s)) => Ok(Field::Str(Arc::new(s))),
        (Kind::Integer, Value::Number(n)) => n
            .as_i64()
            .filter(|n| i32::try_from(*n).is_ok())
            .map(Field::Int)
            .ok_or_else(|| "integer out of the 32-bit range".to_string()),
        (Kind::Boolean, Value::Bool(b)) => Ok(Field::Bool(b)),
        (Kind::Date, Value::Number(n)) => n
            .as_i64()
            .map(Field::Date)
            .ok_or_else(|| "date must be epoch milliseconds".to_string()),
        (Kind::Date, Value::String(s)) => parse_date(&s)
            .map(Field::Date)
            .ok_or_else(|| "unparseable date".to_string()),
        _ => Err("value does not match the field type".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mapping_parses_sorts_and_round_trips() {
        let body = json!({"mappings": {"properties": {
            "title": {"type": "text"}, "tag": {"type": "keyword"}, "n": {"type": "integer"},
            "ok": {"type": "boolean"}, "at": {"type": "date"}}}});
        let mapping = Mapping::parse(&body).unwrap();
        assert_eq!(mapping.names(), vec!["at", "n", "ok", "tag", "title"]);
        assert_eq!(Mapping::decode(&mapping.encode()).unwrap(), mapping);
        assert_eq!(mapping.kind("n"), Some(Kind::Integer));
        assert!(mapping.schema().is_ok());
    }

    #[test]
    fn key_order_and_spelling_do_not_change_the_hash_and_content_does() {
        let a = r#"{"mappings":{"properties":{"title":{"type":"text"},"tag":{"type":"keyword"}}}}"#;
        let b = r#"{ "mappings" : { "properties" : { "tag" : {"type": "keyword"},
            "title": { "type": "text" } } } }"#;
        let c = r#"{"mappings":{"properties":{"title":{"type":"text"},"tag":{"type":"text"}}}}"#;
        let d =
            r#"{"mappings":{"properties":{"title":{"type":"text"},"tags":{"type":"keyword"}}}}"#;
        let hash = |body: &str| {
            Mapping::parse(&serde_json::from_str(body).unwrap())
                .unwrap()
                .hash()
        };
        assert_eq!(hash(a), hash(b));
        assert_ne!(hash(a), hash(c), "a different type is a different mapping");
        assert_ne!(
            hash(a),
            hash(d),
            "a different field name is a different mapping"
        );
    }

    #[test]
    fn dynamic_is_stored_only_when_not_strict_so_old_hashes_hold() {
        let strict = r#"{"mappings":{"properties":{"a":{"type":"keyword"}}}}"#;
        let explicit = r#"{"mappings":{"dynamic":"strict","properties":{"a":{"type":"keyword"}}}}"#;
        let off = r#"{"mappings":{"dynamic":false,"properties":{"a":{"type":"keyword"}}}}"#;
        let on = r#"{"mappings":{"dynamic":true,"properties":{"a":{"type":"keyword"}}}}"#;
        let parse = |b: &str| Mapping::parse(&serde_json::from_str(b).unwrap()).unwrap();
        // The default and the explicit default are one mapping, byte for byte as before.
        assert_eq!(parse(strict).encode(), b"hsearch-mapping-v0\na\tkeyword\n");
        assert_eq!(parse(strict).hash(), parse(explicit).hash());
        // Each other setting is a different mapping and survives the stored form.
        assert_ne!(parse(strict).hash(), parse(off).hash());
        assert_ne!(parse(off).hash(), parse(on).hash());
        for body in [off, on, strict] {
            let mapping = parse(body);
            assert_eq!(Mapping::decode(&mapping.encode()).unwrap(), mapping);
        }
        assert!(Mapping::decode(b"hsearch-mapping-v0\n#dynamic\tmaybe\na\tkeyword\n").is_err());
        for bad in [
            &b"hsearch-mapping-v0\n#dynamic\tstrict\na\tkeyword\n"[..],
            b"hsearch-mapping-v0\na\tkeyword\n#dynamic\tfalse\n",
            b"hsearch-mapping-v0\n#dynamic\tfalse\n#dynamic\ttrue\na\tkeyword\n",
        ] {
            assert!(Mapping::decode(bad).is_err());
        }
    }

    #[test]
    fn a_mapping_over_the_field_limit_is_refused_and_one_at_it_is_not() {
        let limits = MappingLimits {
            max_fields: 2,
            ..MappingLimits::default()
        };
        let body = |n: usize| {
            let props: serde_json::Map<String, Value> = (0..n)
                .map(|i| (format!("f{i}"), json!({"type": "keyword"})))
                .collect();
            json!({"mappings": {"properties": props}})
        };
        assert!(Mapping::parse_with(&body(2), &limits).is_ok());
        let error = Mapping::parse_with(&body(3), &limits).unwrap_err();
        assert!(error.reason.contains("Limit of total fields [2]"));
    }

    #[test]
    fn a_shared_object_and_a_reference_round_trip_and_refuse_tampering() {
        let body = json!({"mappings": {"properties": {"a": {"type": "keyword"}}}});
        let mapping = Mapping::parse(&body).unwrap();
        let hash = mapping.hash();
        let object = mapping.to_object().unwrap();
        assert_eq!(Mapping::from_object(&object, &hash).unwrap(), mapping);
        // Another mapping's hash, a flipped byte and a truncation are all refused.
        let other =
            Mapping::parse(&json!({"mappings": {"properties": {"b": {"type": "text"}}}})).unwrap();
        assert!(Mapping::from_object(&object, &other.hash()).is_err());
        let mut flipped = object.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert!(Mapping::from_object(&flipped, &hash).is_err());
        assert!(Mapping::from_object(&object[..object.len() - 1], &hash).is_err());
        let reference = Mapping::reference(&hash);
        assert_eq!(Mapping::parse_reference(&reference), Some(hash));
        assert!(
            reference.len() < object.len() + 64,
            "a reference is a hash, not a mapping"
        );
        assert!(!Mapping::is_inline(&reference));
        assert!(Mapping::is_inline(&mapping.encode()));
        for bad in [
            &b""[..],
            b"hsearch-mapping-ref-v0\n",
            b"hsearch-mapping-ref-v0\nabc\n",
            b"hsearch-mapping-ref-v0\nXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX\n",
            b"hsearch-mapping-ref-v0\nAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
            &[0xff],
        ] {
            assert_eq!(Mapping::parse_reference(bad), None);
        }
    }

    #[test]
    fn mapping_refuses_everything_it_does_not_implement() {
        for body in [
            json!([]),
            json!({}),
            json!({"settings": {}, "mappings": {"properties": {"a": {"type": "keyword"}}}}),
            json!({"mappings": {"dynamic": "maybe", "properties": {"a": {"type": "keyword"}}}}),
            json!({"mappings": {"dynamic_templates": [], "properties": {"a": {"type": "keyword"}}}}),
            json!({"mappings": {"properties": {"a": {"type": "geo_point"}}}}),
            json!({"mappings": {"properties": {"a": {"type": "keyword", "index": false}}}}),
            json!({"mappings": {"properties": {"a": "keyword"}}}),
            json!({"mappings": {"properties": {"_a": {"type": "keyword"}}}}),
            json!({"mappings": {"properties": {}}}),
        ] {
            assert!(Mapping::parse(&body).is_err(), "{body}");
        }
        assert!(Mapping::decode(b"junk").is_err());
        assert!(Mapping::decode(b"hsearch-mapping-v0\nname-without-tab\n").is_err());
        assert!(Mapping::decode(b"hsearch-mapping-v0\na\tweird\n").is_err());
        assert!(Mapping::decode(&[0xff]).is_err());
    }

    #[test]
    fn values_are_typed_without_coercion() {
        assert_eq!(typed(Kind::Keyword, &json!("a")), Ok(Field::text("a")));
        assert_eq!(typed(Kind::Integer, &json!(5)), Ok(Field::Int(5)));
        assert!(typed(Kind::Integer, &json!(5_000_000_000_i64)).is_err());
        assert!(typed(Kind::Integer, &json!("5")).is_err());
        assert!(typed(Kind::Integer, &json!(1.5)).is_err());
        assert_eq!(typed(Kind::Boolean, &json!(true)), Ok(Field::Bool(true)));
        assert!(typed(Kind::Boolean, &json!("true")).is_err());
        assert_eq!(typed(Kind::Date, &json!(1000)), Ok(Field::Date(1000)));
        assert_eq!(
            typed(Kind::Date, &json!("1970-01-01T00:00:01Z")),
            Ok(Field::Date(1000))
        );
        assert!(typed(Kind::Date, &json!("nope")).is_err());
        assert!(typed(Kind::Date, &json!(true)).is_err());
    }
}
