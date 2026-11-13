//! Index mappings: explicit, typed, finite. Stored with the index so a restarted
//! node can rebuild its schema from the object store alone.
use hsearch_engine::{Kind, Limits, Schema};
use hsearch_index::Field;
use serde_json::Value;

use crate::ApiError;
use crate::date::parse_date;

/// Declared fields, sorted by name so the schema order is canonical.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mapping {
    pub(crate) fields: Vec<(String, Kind)>,
}

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
        if let Some(extra) = mappings.keys().find(|k| *k != "properties") {
            return Err(ApiError::parse(format!(
                "unsupported mappings key [{extra}]"
            )));
        }
        let properties = mappings
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| ApiError::parse("[mappings.properties] is required"))?;
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
        let mapping = Self { fields };
        mapping.schema()?;
        Ok(mapping)
    }

    /// The v0 stored form, one `name<TAB>type` line per field.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = String::from("hsearch-mapping-v0\n");
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
        if lines.next() != Some("hsearch-mapping-v0") {
            return Err(corrupt());
        }
        let mut fields = Vec::new();
        for line in lines {
            let (name, kind) = line.split_once('\t').ok_or_else(corrupt)?;
            fields.push((name.to_string(), kind_of(kind).ok_or_else(corrupt)?));
        }
        Ok(Self { fields })
    }

    /// The engine schema, under the scale profile.
    pub fn schema(&self) -> Result<Schema, ApiError> {
        let fields: Vec<(&str, Kind)> = self.fields.iter().map(|(n, k)| (n.as_str(), *k)).collect();
        Schema::new(1, &fields)
            .map_err(|_| {
                ApiError::new(
                    400,
                    "mapper_parsing_exception",
                    "field names must be 1-64 of [A-Za-z0-9_], not start with an underscore, and be unique; at most 14 fields",
                )
            })?
            .with_limits(Limits::scale())
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

/// Convert one JSON value to the declared kind. No coercion between kinds.
pub(crate) fn typed(kind: Kind, value: &Value) -> Result<Field, String> {
    match (kind, value) {
        (Kind::Keyword | Kind::Text, Value::String(s)) => Ok(Field::Str(s.clone())),
        (Kind::Integer, Value::Number(n)) => n
            .as_i64()
            .filter(|n| i32::try_from(*n).is_ok())
            .map(Field::Int)
            .ok_or_else(|| "integer out of the 32-bit range".to_string()),
        (Kind::Boolean, Value::Bool(b)) => Ok(Field::Bool(*b)),
        (Kind::Date, Value::Number(n)) => n
            .as_i64()
            .map(Field::Date)
            .ok_or_else(|| "date must be epoch milliseconds".to_string()),
        (Kind::Date, Value::String(s)) => parse_date(s)
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
    fn mapping_refuses_everything_it_does_not_implement() {
        for body in [
            json!([]),
            json!({}),
            json!({"settings": {}, "mappings": {"properties": {"a": {"type": "keyword"}}}}),
            json!({"mappings": {"dynamic": "true", "properties": {"a": {"type": "keyword"}}}}),
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
        assert_eq!(
            typed(Kind::Keyword, &json!("a")),
            Ok(Field::Str("a".into()))
        );
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
