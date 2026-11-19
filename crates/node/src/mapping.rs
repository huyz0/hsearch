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
    /// 1 for a mapping as created; each published change makes the next one
    /// `version + 1`. Not stored for version 1, so every mapping stored before
    /// evolution existed keeps its bytes and its hash.
    pub(crate) version: u64,
    /// How many trailing fields were added after creation. Those are optional in a
    /// document, and are the ones a segment written earlier does not have.
    pub(crate) evolved: usize,
}

const INLINE_HEADER: &str = "hsearch-mapping-v0\n";
/// A stored line naming a non-default dynamic setting. Field names cannot start
/// with `#`, so it is never a field.
const DYNAMIC_LINE: &str = "#dynamic\t";
/// `#evolution<TAB>version<TAB>added`: a mapping that is not the first version of
/// itself, and how many of its trailing fields were added after creation.
const EVOLUTION_LINE: &str = "#evolution\t";
const REFERENCE_HEADER: &str = "hsearch-mapping-ref-v0\n";

fn kind_name(kind: Kind) -> String {
    match kind {
        Kind::Keyword => "keyword".into(),
        Kind::Text => "text".into(),
        Kind::Integer => "integer".into(),
        Kind::Boolean => "boolean".into(),
        Kind::Date => "date".into(),
        Kind::Vector(spec) => crate::vector_mapping::spelling(&spec),
        Kind::Nested => "nested".into(),
        Kind::Join => "join".into(),
    }
}

/// Whether a field of this kind can be added to an index that already holds
/// documents: scalars only. A vector field needs its graph built for every
/// document, so it is declared when the index is created.
fn addable(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Keyword | Kind::Text | Kind::Integer | Kind::Boolean | Kind::Date
    )
}

/// One property of a create-index or put-mapping body.
fn parse_field(name: &str, definition: &Value) -> Result<Kind, ApiError> {
    let definition = definition
        .as_object()
        .ok_or_else(|| ApiError::parse(format!("field [{name}] must be an object")))?;
    if definition.get("type").and_then(Value::as_str) == Some("knn_vector") {
        return crate::vector_mapping::parse(name, definition);
    }
    if let Some(extra) = definition.keys().find(|k| *k != "type") {
        return Err(ApiError::new(
            400,
            "mapper_parsing_exception",
            format!("field [{name}] has unsupported parameter [{extra}]"),
        ));
    }
    definition
        .get("type")
        .and_then(Value::as_str)
        .and_then(kind_of)
        .ok_or_else(|| {
            ApiError::new(
                400,
                "mapper_parsing_exception",
                format!("field [{name}] needs a supported type"),
            )
        })
}

/// Whether `name` is a field name the engine accepts.
pub(crate) fn valid_field_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The kind a field met for the first time in a document is mapped as under
/// `dynamic: true`: a string is `text` (OpenSearch's primary mapping for one, with
/// no `.keyword` sub-field), a boolean `boolean`, a whole number in 32 bits
/// `integer`. Anything else, including a float, a larger number, an array or an
/// object, is refused rather than guessed: declare the field to store it.
pub(crate) fn infer(name: &str, value: &Value) -> Result<Kind, ApiError> {
    let refuse = |why: &str| {
        Err(ApiError::new(
            400,
            "mapper_parsing_exception",
            format!(
                "field [{name}]: dynamic mapping cannot map {why}; declare the field in the mapping"
            ),
        ))
    };
    match value {
        Value::String(_) => Ok(Kind::Text),
        Value::Bool(_) => Ok(Kind::Boolean),
        Value::Number(n) => match n.as_i64() {
            Some(n) if i32::try_from(n).is_ok() => Ok(Kind::Integer),
            Some(_) => refuse("an integer outside the 32-bit range"),
            None => refuse("a floating-point number"),
        },
        Value::Array(_) => refuse("an array"),
        Value::Object(_) => refuse("an object"),
        Value::Null => refuse("null"),
    }
}

fn kind_of(name: &str) -> Option<Kind> {
    Some(match name {
        "keyword" => Kind::Keyword,
        "text" => Kind::Text,
        "integer" => Kind::Integer,
        "boolean" => Kind::Boolean,
        "date" => Kind::Date,
        _ => return crate::vector_mapping::from_spelling(name),
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
            fields.push((name.clone(), parse_field(name, definition)?));
        }
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        let mapping = Self {
            fields,
            dynamic,
            version: 1,
            evolved: 0,
        };
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
        if self.version > 1 {
            out.push_str(&format!(
                "{EVOLUTION_LINE}{}\t{}\n",
                self.version, self.evolved
            ));
        }
        for (name, kind) in &self.fields {
            out.push_str(name);
            out.push('\t');
            out.push_str(&kind_name(*kind));
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
        let (mut version, mut evolved) = (1, 0);
        // The header lines come first, each at most once and in this order, so a
        // mapping has exactly one stored spelling.
        let mut header = 0;
        for line in lines {
            if let Some(name) = line.strip_prefix(DYNAMIC_LINE) {
                let parsed = Dynamic::from_name(name).map_err(|_| corrupt())?;
                if header != 0 || !fields.is_empty() || parsed == Dynamic::Strict {
                    return Err(corrupt());
                }
                dynamic = parsed;
                header = 1;
                continue;
            }
            if let Some(rest) = line.strip_prefix(EVOLUTION_LINE) {
                let (v, k) = rest.split_once('\t').ok_or_else(corrupt)?;
                if header > 1 || !fields.is_empty() {
                    return Err(corrupt());
                }
                version = v.parse().map_err(|_| corrupt())?;
                evolved = k.parse().map_err(|_| corrupt())?;
                // Spelled the way `encode` writes it. A version with no added
                // fields is a change of the dynamic setting alone.
                if version < 2 || v != version.to_string() || k != evolved.to_string() {
                    return Err(corrupt());
                }
                header = 2;
                continue;
            }
            let (name, kind) = line.split_once('\t').ok_or_else(corrupt)?;
            fields.push((name.to_string(), kind_of(kind).ok_or_else(corrupt)?));
        }
        if evolved >= fields.len() {
            return Err(corrupt());
        }
        Ok(Self {
            fields,
            dynamic,
            version,
            evolved,
        })
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
        let invalid = |_| {
            ApiError::new(
                400,
                "mapper_parsing_exception",
                "field names must be 1-64 of [A-Za-z0-9_], not start with an underscore, and be unique; at most 14 fields",
            )
        };
        let fields: Vec<(&str, Kind)> = self.fields.iter().map(|(n, k)| (n.as_str(), *k)).collect();
        let base = fields.len() - self.evolved;
        // The fields as created, then the ones added since as optional ones, so a
        // schema opened from the stored mapping is the one a live index evolved to.
        let mut schema = Schema::new(1, &fields[..base]).map_err(invalid)?;
        if self.evolved > 0 {
            schema = schema
                .evolve(self.version, &fields[base..])
                .map_err(invalid)?;
        }
        schema
            .with_limits(Limits::scale().with_document_bytes(document_bytes))
            .map_err(ApiError::from)
    }

    /// How many fields every document must carry: the ones declared at creation.
    pub(crate) fn required(&self) -> usize {
        self.fields.len() - self.evolved
    }

    /// The next version of this mapping: `added` appended after the existing
    /// fields (sorted by name among themselves), under `dynamic`. The field limit
    /// and the engine's field bound apply to the result, and nothing is changed
    /// if either refuses.
    pub(crate) fn next_version(
        &self,
        mut added: Vec<(String, Kind)>,
        dynamic: Dynamic,
        limits: &MappingLimits,
    ) -> Result<Self, ApiError> {
        added.sort_by(|a, b| a.0.cmp(&b.0));
        limits.check_fields(self.fields.len() + added.len())?;
        let evolved = self.evolved + added.len();
        // Only a change that adds fields is a new version, so toggling the dynamic
        // setting back and forth names the same few objects, not a new one each time.
        let version = if added.is_empty() {
            self.version
        } else {
            self.version + 1
        };
        let mut fields = self.fields.clone();
        fields.extend(added);
        let next = Self {
            fields,
            dynamic,
            version,
            evolved,
        };
        next.schema()?;
        Ok(next)
    }

    /// Apply a put-mapping body (`{"properties": {...}, "dynamic": ...}`) to this
    /// mapping: new fields are added, a field redeclared as it is already is
    /// accepted, and a field redeclared as anything else is refused with a pointer
    /// to a new index and `_reindex`. `None` when the body changes nothing.
    pub fn evolve(&self, body: &Value, limits: &MappingLimits) -> Result<Option<Self>, ApiError> {
        let object = body
            .as_object()
            .ok_or_else(|| ApiError::parse("the body must be a JSON object"))?;
        if let Some(extra) = object
            .keys()
            .find(|k| *k != "properties" && *k != "dynamic")
        {
            return Err(ApiError::parse(format!("unsupported key [{extra}]")));
        }
        if object.is_empty() {
            return Err(ApiError::parse("[properties] or [dynamic] is required"));
        }
        let properties = match object.get("properties") {
            None => None,
            Some(value) => Some(
                value
                    .as_object()
                    .ok_or_else(|| ApiError::parse("[properties] must be an object"))?,
            ),
        };
        let dynamic = object
            .get("dynamic")
            .map(Dynamic::parse)
            .transpose()?
            .unwrap_or(self.dynamic);
        let mut added = Vec::new();
        for (name, definition) in properties.into_iter().flatten() {
            let kind = parse_field(name, definition)?;
            match self.kind(name) {
                Some(existing) if existing == kind => {}
                Some(existing) => {
                    return Err(ApiError::new(
                        400,
                        "illegal_argument_exception",
                        format!(
                            "mapper [{name}] cannot be changed from type [{}] to [{}]; \
                             create a new index with the new mapping and use _reindex",
                            kind_name(existing),
                            kind_name(kind)
                        ),
                    ));
                }
                None => added.push((name.clone(), kind)),
            }
        }
        for (name, kind) in &added {
            if !addable(*kind) {
                return Err(ApiError::new(
                    400,
                    "mapper_parsing_exception",
                    format!(
                        "field [{name}] of type [{}] cannot be added to an existing index; \
                         create a new index with the field and use _reindex",
                        kind_name(*kind)
                    ),
                ));
            }
        }
        if added.is_empty() && dynamic == self.dynamic {
            return Ok(None);
        }
        self.next_version(added, dynamic, limits).map(Some)
    }

    pub(crate) fn kind(&self, name: &str) -> Option<Kind> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, k)| *k)
    }

    /// Declared field names in schema order.
    pub(crate) fn names(&self) -> Vec<&str> {
        // A vector is indexed, not stored: it is not part of a hit's source.
        self.fields
            .iter()
            .filter(|(_, k)| !matches!(k, Kind::Vector(_)))
            .map(|(n, _)| n.as_str())
            .collect()
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
        (Kind::Vector(spec), Value::Array(items)) => {
            let mut vector = Vec::with_capacity(items.len());
            for item in &items {
                let c = item
                    .as_f64()
                    .map(|c| c as f32)
                    .filter(|c| c.is_finite())
                    .ok_or_else(|| "a vector holds only finite numbers".to_string())?;
                vector.push(c);
            }
            if vector.len() != usize::from(spec.dimension) {
                return Err(format!(
                    "the vector has {} components, the field declares {}",
                    vector.len(),
                    spec.dimension
                ));
            }
            Ok(Field::Vector(vector.into()))
        }
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

    fn base() -> Mapping {
        Mapping::parse(&json!({"mappings": {"properties": {
            "title": {"type": "text"}, "n": {"type": "integer"}}}}))
        .unwrap()
    }

    #[test]
    fn an_evolved_mapping_appends_its_fields_and_round_trips_with_its_version() {
        let v1 = base();
        let limits = MappingLimits::default();
        let v2 = v1
            .evolve(
                &json!({"properties": {"zed": {"type": "keyword"}, "alpha": {"type": "boolean"}}}),
                &limits,
            )
            .unwrap()
            .unwrap();
        // Added fields follow the declared ones, in name order among themselves.
        let names: Vec<&str> = v2.fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["n", "title", "alpha", "zed"]);
        assert_eq!((v2.version, v2.evolved, v2.required()), (2, 2, 2));
        assert_eq!(Mapping::decode(&v2.encode()).unwrap(), v2);
        assert_ne!(v1.hash(), v2.hash());
        // The first version's stored bytes are what they always were.
        assert_eq!(
            v1.encode(),
            b"hsearch-mapping-v0\nn\tinteger\ntitle\ttext\n"
        );
        // The schema a restarted node builds is the one a live index evolved to.
        let live = v1
            .schema()
            .unwrap()
            .evolve(2, &[("alpha", Kind::Boolean), ("zed", Kind::Keyword)])
            .unwrap();
        assert_eq!(v2.schema().unwrap(), live);
        // A second change stacks: three versions, four added fields.
        let v3 = v2
            .evolve(&json!({"properties": {"mid": {"type": "date"}}}), &limits)
            .unwrap()
            .unwrap();
        assert_eq!((v3.version, v3.evolved), (3, 3));
        assert_eq!(
            v3.schema().unwrap(),
            live.evolve(3, &[("mid", Kind::Date)]).unwrap()
        );
        // A change of the dynamic setting alone adds no version: the same fields
        // under another setting.
        let v4 = v3
            .evolve(&json!({"dynamic": true}), &limits)
            .unwrap()
            .unwrap();
        assert_eq!((v4.version, v4.evolved, v4.dynamic), (3, 3, Dynamic::True));
        assert_eq!(Mapping::decode(&v4.encode()).unwrap(), v4);
        assert_eq!(v4.fields, v3.fields);
    }

    #[test]
    fn evolve_accepts_what_changes_nothing_and_refuses_what_cannot_be_done_in_place() {
        let v1 = base();
        let limits = MappingLimits::default();
        // The same declaration again, and the current dynamic setting, change nothing.
        for same in [
            json!({"properties": {"title": {"type": "text"}}}),
            json!({"properties": {"n": {"type": "integer"}}, "dynamic": "strict"}),
            json!({"properties": {}}),
        ] {
            assert_eq!(v1.evolve(&same, &limits).unwrap(), None, "{same}");
        }
        for (bad, reason) in [
            (
                json!({"properties": {"n": {"type": "keyword"}}}),
                "from type [integer] to [keyword]",
            ),
            (
                json!({"properties": {"title": {"type": "keyword"}}}),
                "_reindex",
            ),
            (
                json!({"properties": {"v": {"type": "knn_vector", "dimension": 4}}}),
                "cannot be added",
            ),
            (
                json!({"properties": {"x": {"type": "weird"}}}),
                "needs a supported type",
            ),
            (
                json!({"properties": {"x": {"type": "keyword", "index": false}}}),
                "unsupported parameter",
            ),
            (
                json!({"properties": {"_x": {"type": "keyword"}}}),
                "field names",
            ),
            (json!({"dynamic": "maybe"}), "[dynamic]"),
            (json!({"mappings": {}}), "unsupported key"),
            (json!({}), "required"),
            (json!([]), "JSON object"),
            (json!({"properties": 3}), "must be an object"),
        ] {
            let error = v1.evolve(&bad, &limits).unwrap_err();
            assert!(error.reason.contains(reason), "{bad}: {}", error.reason);
        }
        // The configured field limit counts the fields added.
        let tight = MappingLimits {
            max_fields: 3,
            ..MappingLimits::default()
        };
        assert!(
            v1.evolve(&json!({"properties": {"a": {"type": "keyword"}}}), &tight)
                .is_ok()
        );
        let error = v1
            .evolve(
                &json!({"properties": {"a": {"type": "keyword"}, "b": {"type": "keyword"}}}),
                &tight,
            )
            .unwrap_err();
        assert!(error.reason.contains("Limit of total fields [3]"));
        // The engine's bound sits under it: 14 fields at most.
        let wide: serde_json::Map<String, Value> = (0..13)
            .map(|i| (format!("f{i}"), json!({"type": "keyword"})))
            .collect();
        let error = v1
            .evolve(&json!({"properties": wide}), &limits)
            .unwrap_err();
        assert!(
            error.reason.contains("at most 14 fields"),
            "{}",
            error.reason
        );
    }

    #[test]
    fn a_stored_mapping_with_a_malformed_evolution_line_is_refused() {
        for bad in [
            &b"hsearch-mapping-v0\n#evolution\t1\t1\na\tkeyword\nb\tkeyword\n"[..],
            b"hsearch-mapping-v0\n#evolution\t2\t2\na\tkeyword\nb\tkeyword\n",
            b"hsearch-mapping-v0\n#evolution\t02\t1\na\tkeyword\nb\tkeyword\n",
            b"hsearch-mapping-v0\n#evolution\t2\na\tkeyword\nb\tkeyword\n",
            b"hsearch-mapping-v0\n#evolution\tx\t1\na\tkeyword\nb\tkeyword\n",
            b"hsearch-mapping-v0\na\tkeyword\n#evolution\t2\t1\nb\tkeyword\n",
            b"hsearch-mapping-v0\n#evolution\t2\t1\n#evolution\t2\t1\na\tkeyword\nb\tkeyword\n",
            b"hsearch-mapping-v0\n#evolution\t2\t1\n#dynamic\ttrue\na\tkeyword\nb\tkeyword\n",
        ] {
            assert!(
                Mapping::decode(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let good =
            b"hsearch-mapping-v0\n#dynamic\ttrue\n#evolution\t2\t1\na\tkeyword\nb\tkeyword\n";
        let mapping = Mapping::decode(good).unwrap();
        assert_eq!((mapping.version, mapping.evolved), (2, 1));
        assert_eq!(mapping.encode(), good);
    }

    #[test]
    fn dynamic_values_map_to_the_kinds_the_engine_has_and_nothing_is_guessed() {
        assert_eq!(infer("a", &json!("x")).unwrap(), Kind::Text);
        assert_eq!(infer("a", &json!(true)).unwrap(), Kind::Boolean);
        assert_eq!(infer("a", &json!(5)).unwrap(), Kind::Integer);
        assert_eq!(
            infer("a", &json!(-2_147_483_648_i64)).unwrap(),
            Kind::Integer
        );
        for value in [
            json!(2_147_483_648_i64),
            json!(1.5),
            json!([1]),
            json!({"a": 1}),
            json!(null),
        ] {
            let error = infer("a", &value).unwrap_err();
            assert!(error.reason.contains("declare the field"), "{value}");
        }
        assert!(valid_field_name("a_B9") && !valid_field_name("_a") && !valid_field_name("a b"));
        assert!(!valid_field_name("") && !valid_field_name(&"x".repeat(65)));
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
