//! Field limits and the dynamic-mapping policy (PR-42).
//!
//! The limits guard against mapping explosion: a request that would take an
//! index past one is refused whole, with a structured error, and a refusal never
//! changes the stored mapping. Everything here is a pure check over a mapping or
//! a parsed source; nothing reads or writes the store.
use serde_json::{Map, Value};

use crate::ApiError;

/// Largest `max_fields` a configuration may ask for (the "upper bound
/// configurable" of PR-42). The default is 1,000, OpenSearch's; the engine's own
/// schema cap (14 fields today) still applies underneath.
pub const MAX_FIELDS_CEILING: usize = 10_000;

/// What an index does with a field its mapping does not declare.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dynamic {
    /// Add it to the mapping (online mapping evolution, M21.04).
    True,
    /// Ignore it: not indexed, not searchable.
    False,
    /// Reject the document.
    Strict,
}

impl Dynamic {
    /// The spelling the mapping body and stored form use.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::True => "true",
            Self::False => "false",
            Self::Strict => "strict",
        }
    }

    /// `true`, `false` or `"strict"`, and the string forms of the three, as
    /// OpenSearch accepts them.
    pub(crate) fn parse(value: &Value) -> Result<Self, ApiError> {
        match value {
            Value::Bool(true) => Ok(Self::True),
            Value::Bool(false) => Ok(Self::False),
            Value::String(s) => Self::from_name(s),
            _ => Err(Self::refused()),
        }
        .map_err(|_| Self::refused())
    }

    pub(crate) fn from_name(name: &str) -> Result<Self, ApiError> {
        match name {
            "true" => Ok(Self::True),
            "false" => Ok(Self::False),
            "strict" => Ok(Self::Strict),
            _ => Err(Self::refused()),
        }
    }

    fn refused() -> ApiError {
        ApiError::new(
            400,
            "mapper_parsing_exception",
            "[dynamic] must be true, false or strict",
        )
    }
}

/// Bounds on the shape of mappings and documents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappingLimits {
    /// Most fields one index may declare.
    pub max_fields: usize,
    /// Deepest object nesting of a field in a document, the top level being 1.
    pub max_depth: usize,
    /// Most objects held in arrays in one document.
    pub max_nested_objects: usize,
}

impl Default for MappingLimits {
    /// The M02.05 values, OpenSearch's own: 1,000 fields, depth 20, 10,000
    /// nested objects.
    fn default() -> Self {
        Self {
            max_fields: 1000,
            max_depth: 20,
            max_nested_objects: 10_000,
        }
    }
}

impl MappingLimits {
    /// Refuse a configuration that cannot work, rather than clamping it.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_fields == 0 || self.max_fields > MAX_FIELDS_CEILING {
            return Err(format!("the field limit must be 1..={MAX_FIELDS_CEILING}"));
        }
        if self.max_depth == 0 {
            return Err("the depth limit must be positive".into());
        }
        if self.max_nested_objects == 0 {
            return Err("the nested-object limit must be positive".into());
        }
        Ok(())
    }

    /// Refuse `fields` fields in one index, naming the limit.
    pub(crate) fn check_fields(&self, fields: usize) -> Result<(), ApiError> {
        if fields > self.max_fields {
            return Err(ApiError::bad(format!(
                "Limit of total fields [{}] has been exceeded",
                self.max_fields
            )));
        }
        Ok(())
    }

    /// Check a parsed source's shape: its object depth and the objects it holds
    /// in arrays. Iterative, so a hostile nesting cannot exhaust the stack, and
    /// it never looks inside a string, so a 100 MiB text field costs nothing.
    pub(crate) fn check_source(&self, source: &Map<String, Value>) -> Result<(), ApiError> {
        // Fast path, the usual document: every value is a scalar.
        if source
            .values()
            .all(|v| !matches!(v, Value::Object(_) | Value::Array(_)))
        {
            return Ok(());
        }
        let mut objects = 0usize;
        // (value, depth of the field that holds it). Top-level fields are depth 1.
        let mut stack: Vec<(&Value, usize)> = source
            .values()
            .filter(|v| v.is_object() || v.is_array())
            .map(|v| (v, 1))
            .collect();
        while let Some((value, depth)) = stack.pop() {
            match value {
                Value::Object(map) => {
                    // The object is a field of `depth`; its members are one deeper.
                    if depth + 1 > self.max_depth && !map.is_empty() {
                        return Err(ApiError::new(
                            400,
                            "mapper_parsing_exception",
                            format!("The depth of field exceeds the limit [{}]", self.max_depth),
                        ));
                    }
                    stack.extend(
                        map.values()
                            .filter(|v| v.is_object() || v.is_array())
                            .map(|v| (v, depth + 1)),
                    );
                }
                Value::Array(items) => {
                    for item in items {
                        if item.is_object() {
                            objects += 1;
                            if objects > self.max_nested_objects {
                                return Err(ApiError::bad(format!(
                                    "The number of nested objects has exceeded the allowed limit of [{}]",
                                    self.max_nested_objects
                                )));
                            }
                        }
                        // An array adds no depth: its elements sit where it does.
                        // Scalars are not pushed, so a long array of numbers costs
                        // one pass and no stack.
                        if item.is_object() || item.is_array() {
                            stack.push((item, depth));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn check(limits: &MappingLimits, source: Value) -> Result<(), ApiError> {
        limits.check_source(source.as_object().unwrap())
    }

    fn small() -> MappingLimits {
        MappingLimits {
            max_depth: 3,
            max_nested_objects: 2,
            ..MappingLimits::default()
        }
    }

    #[test]
    fn depth_counts_object_levels_and_arrays_add_none() {
        let limits = small();
        assert!(check(&limits, json!({"a": 1, "b": "x"})).is_ok());
        assert!(
            check(&limits, json!({"o": {"p": {"q": 1}}})).is_ok(),
            "q is depth 3"
        );
        let error = check(&limits, json!({"o": {"p": {"q": {"r": 1}}}})).unwrap_err();
        assert_eq!(error.kind, "mapper_parsing_exception");
        assert!(error.reason.contains("[3]"), "{}", error.reason);
        // An array between objects adds no level.
        assert!(check(&limits, json!({"o": [{"p": {"q": 1}}]})).is_ok());
        assert!(check(&limits, json!({"o": [{"p": {"q": {"r": 1}}}]})).is_err());
        // An empty object holds no field, so it is not a level.
        assert!(check(&limits, json!({"o": {"p": {"q": {}}}})).is_ok());
    }

    #[test]
    fn nested_objects_are_counted_across_arrays_and_the_limit_is_inclusive() {
        let limits = small();
        assert!(check(&limits, json!({"o": [{"p": 1}, {"p": 2}]})).is_ok());
        let error = check(&limits, json!({"o": [{"p": 1}, {"p": 2}, {"p": 3}]})).unwrap_err();
        assert_eq!(error.kind, "illegal_argument_exception");
        assert!(error.reason.contains("nested objects"), "{}", error.reason);
        // Counted over the whole document, through arrays of arrays and across fields.
        assert!(check(&limits, json!({"o": [[{"p": 1}], [{"p": 2}, {"p": 3}]]})).is_err());
        assert!(
            check(
                &limits,
                json!({"a": [{"p": 1}], "b": [{"p": 2}], "c": [{"p": 3}]})
            )
            .is_err()
        );
        // Scalars in arrays and objects outside arrays are not nested objects.
        assert!(check(&limits, json!({"a": [1, 2, 3, 4], "o": {"p": 1}})).is_ok());
        assert!(check(&limits, json!({"v": vec![0.5; 4096]})).is_ok());
    }

    #[test]
    fn a_hostile_nesting_is_refused_without_recursion() {
        let limits = MappingLimits::default();
        // Far past the parser's own recursion limit, built directly.
        let mut value = json!(1);
        for _ in 0..100_000 {
            // `json!` would serialize the inner value by reference, recursively.
            let mut map = Map::new();
            map.insert("k".into(), value);
            value = Value::Object(map);
        }
        let mut top = Map::new();
        top.insert("deep".into(), value);
        let source = Value::Object(top);
        let refused = limits.check_source(source.as_object().unwrap()).is_err();
        // Dropping or cloning it recurses; leak it instead.
        std::mem::forget(source);
        assert!(refused);
    }

    #[test]
    fn field_limit_is_inclusive_and_names_itself() {
        let limits = MappingLimits {
            max_fields: 5,
            ..MappingLimits::default()
        };
        assert!(limits.check_fields(5).is_ok());
        let error = limits.check_fields(6).unwrap_err();
        assert_eq!(error.status, 400);
        assert!(error.reason.contains("[5]"), "{}", error.reason);
    }

    #[test]
    fn dynamic_parses_booleans_and_strings_only() {
        assert_eq!(Dynamic::parse(&json!(true)).unwrap(), Dynamic::True);
        assert_eq!(Dynamic::parse(&json!("false")).unwrap(), Dynamic::False);
        assert_eq!(Dynamic::parse(&json!("strict")).unwrap(), Dynamic::Strict);
        for bad in [json!("STRICT"), json!(0), json!(null), json!([]), json!("")] {
            assert!(Dynamic::parse(&bad).is_err(), "{bad}");
        }
        for d in [Dynamic::True, Dynamic::False, Dynamic::Strict] {
            assert_eq!(Dynamic::from_name(d.name()).unwrap(), d);
        }
    }
}
