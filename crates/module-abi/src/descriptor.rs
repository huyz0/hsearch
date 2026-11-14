//! The descriptor a module writes from `hs_module_describe`, parsed strictly.

use crate::{MAX_BUFFER_BYTES, MAX_DESCRIPTOR_BYTES, Version};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fmt;

/// Most extension points one module may declare. Bounds the descriptor's work.
pub const MAX_EXTENSION_POINTS: usize = 64;

/// What an extension point is.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Kind {
    /// Text to tokens.
    Analyzer = 1,
    /// A module clause lowered to the core query IR.
    Query = 2,
    /// A batch of documents processed before they are logged.
    Ingest = 3,
    /// A block of feature rows scored.
    Scoring = 4,
}

impl Kind {
    /// The `HS_KIND_*` value.
    #[must_use]
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The kind for an `HS_KIND_*` value.
    #[must_use]
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Analyzer),
            2 => Some(Self::Query),
            3 => Some(Self::Ingest),
            4 => Some(Self::Scoring),
            _ => None,
        }
    }

    /// The descriptor spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Analyzer => "analyzer",
            Self::Query => "query",
            Self::Ingest => "ingest",
            Self::Scoring => "scoring",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [Self::Analyzer, Self::Query, Self::Ingest, Self::Scoring]
            .into_iter()
            .find(|k| k.name() == text)
    }
}

/// One declared extension point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionPoint {
    /// Which kind of point.
    pub kind: Kind,
    /// Unique per kind; what a mapping and a call name.
    pub id: String,
    /// The point's own `major.minor.patch`.
    pub version: String,
}

/// A parsed, validated descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Descriptor {
    /// Unique among loaded modules.
    pub name: String,
    /// The module's own `major.minor.patch`.
    pub version: String,
    /// ABI the module was built against.
    pub abi: Version,
    /// Engine API the module requires.
    pub engine_api: Version,
    /// The most calls the host may run at once.
    pub max_concurrent_calls: u32,
    /// Size of each WASM I/O region; ignored for native.
    pub io_bytes: u32,
    /// Declared points, never empty.
    pub extension_points: Vec<ExtensionPoint>,
}

/// Why a descriptor was refused. Never carries module text beyond a key name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DescriptorError(pub &'static str);

impl fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "descriptor refused: {}", self.0)
    }
}

impl std::error::Error for DescriptorError {}

const KEYS: [&str; 7] = [
    "name",
    "version",
    "abi",
    "engine_api",
    "max_concurrent_calls",
    "io_bytes",
    "extension_points",
];
const POINT_KEYS: [&str; 4] = ["kind", "id", "version", "deterministic"];

fn refuse<T>(why: &'static str) -> Result<T, DescriptorError> {
    Err(DescriptorError(why))
}

impl Descriptor {
    /// Parses and validates a descriptor, refusing unknown keys, anything over
    /// [`MAX_DESCRIPTOR_BYTES`] and every rule of section 2.
    ///
    /// # Errors
    /// [`DescriptorError`] naming the first rule broken.
    pub fn parse(bytes: &[u8]) -> Result<Self, DescriptorError> {
        if bytes.len() > MAX_DESCRIPTOR_BYTES as usize {
            return refuse("larger than the descriptor ceiling");
        }
        let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(bytes) else {
            return refuse("not a JSON object");
        };
        only_keys(&map, &KEYS)?;
        let name = text(&map, "name")?;
        if !valid_name(name, b"-") {
            return refuse("name does not match ^[a-z][a-z0-9_-]{0,63}$");
        }
        let version = text(&map, "version")?;
        if !semver(version) {
            return refuse("version is not major.minor.patch");
        }
        let abi =
            Version::parse(text(&map, "abi")?).ok_or(DescriptorError("abi is not major.minor"))?;
        let engine_api = Version::parse(text(&map, "engine_api")?)
            .ok_or(DescriptorError("engine_api is not major.minor"))?;
        let max_concurrent_calls = bounded(&map, "max_concurrent_calls", 1, u64::from(u32::MAX))?;
        let io_bytes = bounded(&map, "io_bytes", 1, u64::from(MAX_BUFFER_BYTES))?;
        let Some(Value::Array(points)) = map.get("extension_points") else {
            return refuse("extension_points is not an array");
        };
        if points.is_empty() || points.len() > MAX_EXTENSION_POINTS {
            return refuse("extension_points must hold 1 to 64 entries");
        }
        let mut extension_points = Vec::with_capacity(points.len());
        let mut seen = HashSet::new();
        for point in points {
            let Value::Object(point) = point else {
                return refuse("an extension point is not an object");
            };
            only_keys(point, &POINT_KEYS)?;
            let kind = Kind::parse(text(point, "kind")?)
                .ok_or(DescriptorError("unknown extension point kind"))?;
            let id = text(point, "id")?;
            if !valid_name(id, b"") {
                return refuse("point id does not match ^[a-z][a-z0-9_]{0,63}$");
            }
            if !semver(text(point, "version")?) {
                return refuse("point version is not major.minor.patch");
            }
            if point.get("deterministic") != Some(&Value::Bool(true)) {
                return refuse("deterministic must be true");
            }
            if !seen.insert((kind, id.to_owned())) {
                return refuse("duplicate point id for a kind");
            }
            extension_points.push(ExtensionPoint {
                kind,
                id: id.to_owned(),
                version: text(point, "version")?.to_owned(),
            });
        }
        Ok(Self {
            name: name.to_owned(),
            version: version.to_owned(),
            abi,
            engine_api,
            max_concurrent_calls: u32::try_from(max_concurrent_calls).unwrap_or(u32::MAX),
            io_bytes: u32::try_from(io_bytes).unwrap_or(u32::MAX),
            extension_points,
        })
    }

    /// The declared point for a kind and id, if any.
    #[must_use]
    pub fn point(&self, kind: Kind, id: &str) -> Option<&ExtensionPoint> {
        self.extension_points
            .iter()
            .find(|p| p.kind == kind && p.id == id)
    }
}

fn only_keys(map: &Map<String, Value>, allowed: &[&str]) -> Result<(), DescriptorError> {
    if map.len() != allowed.len() || map.keys().any(|k| !allowed.contains(&k.as_str())) {
        return refuse("unknown or missing key");
    }
    Ok(())
}

fn text<'a>(map: &'a Map<String, Value>, key: &'static str) -> Result<&'a str, DescriptorError> {
    match map.get(key) {
        Some(Value::String(s)) => Ok(s),
        _ => refuse("a required string is missing or not a string"),
    }
}

fn bounded(
    map: &Map<String, Value>,
    key: &'static str,
    min: u64,
    max: u64,
) -> Result<u64, DescriptorError> {
    match map.get(key).and_then(Value::as_u64) {
        Some(n) if (min..=max).contains(&n) => Ok(n),
        _ => refuse("an integer is missing, not an unsigned integer, or out of range"),
    }
}

fn valid_name(name: &str, extra: &[u8]) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || extra.contains(c)
        })
}

fn semver(text: &str) -> bool {
    let mut parts = text.split('.');
    let three = (0..3).all(|_| parts.next().is_some_and(plain_number));
    three && parts.next().is_none()
}

fn plain_number(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 9
        && part.bytes().all(|b| b.is_ascii_digit())
        && (part.len() == 1 || !part.starts_with('0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Vec<u8> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/extensions/abi-v1/descriptor.example.json"
        );
        std::fs::read(path).expect("example descriptor")
    }

    fn edited(f: impl FnOnce(&mut Value)) -> Vec<u8> {
        let mut v: Value = serde_json::from_slice(&example()).expect("json");
        f(&mut v);
        serde_json::to_vec(&v).expect("encode")
    }

    #[test]
    fn example_descriptor_parses() {
        let d = Descriptor::parse(&example()).expect("valid");
        assert_eq!(d.name, "example-lowercase-ascii");
        assert_eq!(
            (d.abi, d.engine_api),
            (
                Version { major: 1, minor: 0 },
                Version { major: 1, minor: 0 }
            )
        );
        assert_eq!(d.max_concurrent_calls, 8);
        assert_eq!(d.io_bytes, 65536);
        assert!(d.point(Kind::Analyzer, "lowercase_ascii").is_some());
        assert!(d.point(Kind::Query, "terms_lowercase").is_some());
        assert!(d.point(Kind::Query, "lowercase_ascii").is_none());
        assert!(d.point(Kind::Scoring, "terms_lowercase").is_none());
    }

    #[test]
    fn every_rule_refuses() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("not json", b"nope".to_vec()),
            ("array", b"[]".to_vec()),
            ("unknown key", edited(|v| v["extra"] = Value::Bool(true))),
            (
                "missing key",
                edited(|v| drop(v.as_object_mut().expect("o").remove("io_bytes"))),
            ),
            ("uppercase name", edited(|v| v["name"] = "Bad".into())),
            (
                "name starts with digit",
                edited(|v| v["name"] = "1bad".into()),
            ),
            (
                "name too long",
                edited(|v| v["name"] = ("a".repeat(65)).into()),
            ),
            ("empty name", edited(|v| v["name"] = "".into())),
            ("module version", edited(|v| v["version"] = "1.0".into())),
            (
                "leading zero version",
                edited(|v| v["version"] = "1.00.0".into()),
            ),
            ("abi", edited(|v| v["abi"] = "1".into())),
            ("engine api", edited(|v| v["engine_api"] = "x.y".into())),
            (
                "zero concurrency",
                edited(|v| v["max_concurrent_calls"] = 0.into()),
            ),
            (
                "negative concurrency",
                edited(|v| v["max_concurrent_calls"] = (-1).into()),
            ),
            (
                "string concurrency",
                edited(|v| v["max_concurrent_calls"] = "8".into()),
            ),
            ("zero io", edited(|v| v["io_bytes"] = 0.into())),
            (
                "io over ceiling",
                edited(|v| v["io_bytes"] = (u64::from(MAX_BUFFER_BYTES) + 1).into()),
            ),
            (
                "points not array",
                edited(|v| v["extension_points"] = Value::Null),
            ),
            (
                "no points",
                edited(|v| v["extension_points"] = Value::Array(vec![])),
            ),
            (
                "point not object",
                edited(|v| v["extension_points"] = Value::Array(vec![1.into()])),
            ),
            (
                "point unknown key",
                edited(|v| v["extension_points"][0]["x"] = 1.into()),
            ),
            (
                "point kind",
                edited(|v| v["extension_points"][0]["kind"] = "prefix".into()),
            ),
            (
                "point id",
                edited(|v| v["extension_points"][0]["id"] = "Bad-Id".into()),
            ),
            (
                "point id hyphen",
                edited(|v| v["extension_points"][0]["id"] = "a-b".into()),
            ),
            (
                "point version",
                edited(|v| v["extension_points"][0]["version"] = "1".into()),
            ),
            (
                "nondeterministic",
                edited(|v| v["extension_points"][0]["deterministic"] = false.into()),
            ),
            (
                "deterministic as string",
                edited(|v| v["extension_points"][0]["deterministic"] = "true".into()),
            ),
            (
                "duplicate point",
                edited(|v| {
                    let first = v["extension_points"][0].clone();
                    v["extension_points"].as_array_mut().expect("a").push(first);
                }),
            ),
            (
                "too many points",
                edited(|v| {
                    let points = (0..=MAX_EXTENSION_POINTS)
                    .map(|i| serde_json::json!({"kind": "analyzer", "id": format!("p{i}"), "version": "1.0.0", "deterministic": true}))
                    .collect();
                    v["extension_points"] = Value::Array(points);
                }),
            ),
            ("oversize", {
                let mut big = example();
                big.resize(MAX_DESCRIPTOR_BYTES as usize + 1, b' ');
                big
            }),
        ];
        for (label, bytes) in cases {
            assert!(
                Descriptor::parse(&bytes).is_err(),
                "{label} must be refused"
            );
        }
    }

    #[test]
    fn same_id_may_repeat_across_kinds() {
        let bytes = edited(|v| {
            let mut second = v["extension_points"][0].clone();
            second["kind"] = "scoring".into();
            v["extension_points"]
                .as_array_mut()
                .expect("a")
                .push(second);
        });
        assert!(Descriptor::parse(&bytes).is_ok());
    }

    #[test]
    fn kind_codes_round_trip() {
        for kind in [Kind::Analyzer, Kind::Query, Kind::Ingest, Kind::Scoring] {
            assert_eq!(Kind::from_code(kind.code()), Some(kind));
            assert_eq!(Kind::parse(kind.name()), Some(kind));
        }
        assert_eq!(Kind::from_code(0), None);
        assert_eq!(Kind::from_code(5), None);
    }

    #[test]
    fn error_text_names_only_the_rule() {
        let text = Descriptor::parse(b"nope").expect_err("refused").to_string();
        assert_eq!(text, "descriptor refused: not a JSON object");
    }
}
