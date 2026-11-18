//! The body of one commit-log record: a batch of mutations a recovering writer can replay.
//!
//! `encode_batch` writes the request identity and the mutations exactly as `Schema::prepare`
//! takes them; `apply_batch` reads a body back through the same preparation, so a replayed batch
//! meets every check a live one did and produces the same receipts. A body that does not decode
//! is `Error::Document`, never a partial batch (only batches the engine accepted belong in the
//! log: a replayed refusal stops recovery): the reader checks every length against the bytes
//! that remain, refuses trailing bytes, and holds nothing beyond one decoded batch.
use crate::schema::Value;
use crate::{Engine, Error, Mutation, Receipt, Schema};

const VERSION: u8 = 1;
const PUT: u8 = 0;
const CREATE: u8 = 1;
const DELETE: u8 = 2;

const T_STRING: u8 = 0;
const T_INTEGER: u8 = 1;
const T_BOOLEAN: u8 = 2;
const T_DATE: u8 = 3;
const T_VECTOR: u8 = 4;
const T_NESTED: u8 = 5;
const T_JOIN: u8 = 6;

type Result<T> = std::result::Result<T, Error>;
/// The fields of one object, borrowed from a decoded batch.
type Fields<'a> = Vec<(&'a str, Value<'a>)>;
/// The objects of one nested value, as slices of their fields.
type Objects<'a> = Vec<&'a [(&'a str, Value<'a>)]>;

fn put_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let n = u32::try_from(s.len()).map_err(|_| Error::Limit)?;
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn put_value(out: &mut Vec<u8>, value: &Value<'_>, nested_ok: bool) -> Result<()> {
    match value {
        Value::String(s) => {
            out.push(T_STRING);
            put_str(out, s)?;
        }
        Value::Shared(s) => {
            out.push(T_STRING);
            put_str(out, s)?;
        }
        Value::Integer(n) => {
            out.push(T_INTEGER);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Boolean(b) => {
            out.push(T_BOOLEAN);
            out.push(u8::from(*b));
        }
        Value::Date(n) => {
            out.push(T_DATE);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Vector(v) => {
            out.push(T_VECTOR);
            let n = u32::try_from(v.len()).map_err(|_| Error::Limit)?;
            out.extend_from_slice(&n.to_le_bytes());
            for x in *v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        // An object holds scalars only: decode refuses a deeper body, so encode never writes one.
        Value::Nested(_) if !nested_ok => return Err(Error::Document),
        Value::Nested(objects) => {
            out.push(T_NESTED);
            let n = u32::try_from(objects.len()).map_err(|_| Error::Limit)?;
            out.extend_from_slice(&n.to_le_bytes());
            for object in *objects {
                put_fields(out, object, false)?;
            }
        }
        Value::Join { relation, parent } => {
            out.push(T_JOIN);
            put_str(out, relation)?;
            match parent {
                Some(p) => {
                    out.push(1);
                    put_str(out, p)?;
                }
                None => out.push(0),
            }
        }
    }
    Ok(())
}

fn put_fields(out: &mut Vec<u8>, fields: &[(&str, Value<'_>)], nested_ok: bool) -> Result<()> {
    let n = u32::try_from(fields.len()).map_err(|_| Error::Limit)?;
    out.extend_from_slice(&n.to_le_bytes());
    for (name, value) in fields {
        put_str(out, name)?;
        put_value(out, value, nested_ok)?;
    }
    Ok(())
}

/// The record body for one batch: `request` and `mutations` as `Schema::prepare` takes them.
pub fn encode_batch(request: &str, mutations: &[Mutation<'_>]) -> Result<Vec<u8>> {
    let mut out = vec![VERSION];
    put_str(&mut out, request)?;
    let n = u32::try_from(mutations.len()).map_err(|_| Error::Limit)?;
    out.extend_from_slice(&n.to_le_bytes());
    for mutation in mutations {
        match mutation {
            Mutation::Put {
                id,
                version,
                fields,
            }
            | Mutation::Create {
                id,
                version,
                fields,
            } => {
                out.push(if matches!(mutation, Mutation::Put { .. }) {
                    PUT
                } else {
                    CREATE
                });
                put_str(&mut out, id)?;
                out.extend_from_slice(&version.to_le_bytes());
                put_fields(&mut out, fields, true)?;
            }
            Mutation::Delete { id, version } => {
                out.push(DELETE);
                put_str(&mut out, id)?;
                out.extend_from_slice(&version.to_le_bytes());
            }
        }
    }
    Ok(out)
}

enum Owned {
    Str(String),
    Int(i64),
    Bool(bool),
    Date(i64),
    Vector(Vec<f32>),
    Nested(Vec<Vec<(String, Owned)>>),
    Join(String, Option<String>),
}

struct Op {
    kind: u8,
    id: String,
    version: i64,
    fields: Vec<(String, Owned)>,
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.bytes.len() {
            return Err(Error::Document);
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<usize> {
        let b: [u8; 4] = self.take(4)?.try_into().map_err(|_| Error::Document)?;
        Ok(u32::from_le_bytes(b) as usize)
    }
    fn i64(&mut self) -> Result<i64> {
        let b: [u8; 8] = self.take(8)?.try_into().map_err(|_| Error::Document)?;
        Ok(i64::from_le_bytes(b))
    }
    fn string(&mut self) -> Result<String> {
        let n = self.u32()?;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| Error::Document)
    }
    /// A count that cannot exceed what the remaining bytes could hold at `min` bytes an item.
    fn count(&mut self, min: usize) -> Result<usize> {
        let n = self.u32()?;
        if n.saturating_mul(min) > self.bytes.len() {
            return Err(Error::Document);
        }
        Ok(n)
    }
    fn value(&mut self, nested_ok: bool) -> Result<Owned> {
        Ok(match self.u8()? {
            T_STRING => Owned::Str(self.string()?),
            T_INTEGER => Owned::Int(self.i64()?),
            T_BOOLEAN => match self.u8()? {
                0 => Owned::Bool(false),
                1 => Owned::Bool(true),
                _ => return Err(Error::Document),
            },
            T_DATE => Owned::Date(self.i64()?),
            T_VECTOR => {
                let n = self.count(4)?;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    let b: [u8; 4] = self.take(4)?.try_into().map_err(|_| Error::Document)?;
                    v.push(f32::from_le_bytes(b));
                }
                Owned::Vector(v)
            }
            T_NESTED if nested_ok => {
                let n = self.count(4)?;
                let mut objects = Vec::with_capacity(n);
                for _ in 0..n {
                    objects.push(self.fields(false)?);
                }
                Owned::Nested(objects)
            }
            T_JOIN => {
                let relation = self.string()?;
                let parent = match self.u8()? {
                    0 => None,
                    1 => Some(self.string()?),
                    _ => return Err(Error::Document),
                };
                Owned::Join(relation, parent)
            }
            _ => return Err(Error::Document),
        })
    }
    fn fields(&mut self, nested_ok: bool) -> Result<Vec<(String, Owned)>> {
        let n = self.count(5)?;
        let mut fields = Vec::with_capacity(n);
        for _ in 0..n {
            let name = self.string()?;
            fields.push((name, self.value(nested_ok)?));
        }
        Ok(fields)
    }
}

fn decode(body: &[u8]) -> Result<(String, Vec<Op>)> {
    let mut r = Reader { bytes: body };
    if r.u8()? != VERSION {
        return Err(Error::Document);
    }
    let request = r.string()?;
    let n = r.count(13)?;
    let mut ops = Vec::with_capacity(n);
    for _ in 0..n {
        let kind = r.u8()?;
        let id = r.string()?;
        let version = r.i64()?;
        let fields = match kind {
            PUT | CREATE => r.fields(true)?,
            DELETE => Vec::new(),
            _ => return Err(Error::Document),
        };
        ops.push(Op {
            kind,
            id,
            version,
            fields,
        });
    }
    if !r.bytes.is_empty() {
        return Err(Error::Document);
    }
    Ok((request, ops))
}

/// Replay one record body: decode it, prepare it against `schema` as a live batch is, and apply
/// it. The receipts are the ones the original apply returned.
pub fn apply_batch(schema: &Schema, engine: &Engine<'_>, body: &[u8]) -> Result<Vec<Receipt>> {
    let (request, ops) = decode(body)?;
    // Borrow in stages, each finished before the next so a stage only borrows what is complete:
    // the objects of every nested value, then the lists of them, then each mutation's fields.
    let objects: Vec<Vec<Vec<Fields<'_>>>> = ops
        .iter()
        .map(|op| {
            op.fields
                .iter()
                .map(|(_, v)| match v {
                    Owned::Nested(objs) => objs
                        .iter()
                        .map(|o| o.iter().map(|(n, v)| (n.as_str(), flat(v))).collect())
                        .collect(),
                    _ => Vec::new(),
                })
                .collect()
        })
        .collect();
    let lists: Vec<Vec<Objects<'_>>> = objects
        .iter()
        .map(|per_op| {
            per_op
                .iter()
                .map(|objs| objs.iter().map(Vec::as_slice).collect())
                .collect()
        })
        .collect();
    let fields: Vec<Vec<(&str, Value<'_>)>> = ops
        .iter()
        .zip(&lists)
        .map(|(op, per_field)| {
            op.fields
                .iter()
                .zip(per_field)
                .map(|((name, v), list)| {
                    let value = match v {
                        Owned::Nested(_) => Value::Nested(list),
                        other => flat(other),
                    };
                    (name.as_str(), value)
                })
                .collect()
        })
        .collect();
    let mutations: Vec<Mutation<'_>> = ops
        .iter()
        .zip(&fields)
        .map(|(op, f)| match op.kind {
            PUT => Mutation::Put {
                id: &op.id,
                version: op.version,
                fields: f,
            },
            CREATE => Mutation::Create {
                id: &op.id,
                version: op.version,
                fields: f,
            },
            _ => Mutation::Delete {
                id: &op.id,
                version: op.version,
            },
        })
        .collect();
    engine.apply(schema.prepare(&request, &mutations)?)
}

fn flat(v: &Owned) -> Value<'_> {
    match v {
        Owned::Str(s) => Value::String(s),
        Owned::Int(n) => Value::Integer(*n),
        Owned::Bool(b) => Value::Boolean(*b),
        Owned::Date(n) => Value::Date(*n),
        Owned::Vector(v) => Value::Vector(v),
        Owned::Join(relation, parent) => Value::Join {
            relation,
            parent: parent.as_deref(),
        },
        // Nested values are lifted by the caller; a nested value inside an object is refused at
        // decode, so this is unreachable for a decoded batch.
        Owned::Nested(_) => Value::Nested(&[]),
    }
}
