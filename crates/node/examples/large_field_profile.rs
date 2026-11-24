//! Index one document with a text field of N MiB through `Node::bulk` on the
//! in-memory store (a local double without its 64 MiB cap) and report time and peak memory, so memory per document can
//! be read against document size. One process per size: the high-water mark is
//! the process's. Provisional: one shared host.
//!
//!     cargo run --release -p hsearch-node --example large_field_profile -- [MiB] [docs]
use std::sync::Arc;
use std::time::Instant;

use hsearch_index::Wait;
use hsearch_node::{Limits, Node};
use serde_json::json;

fn status_kb(field: &str) -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

/// A measurement double without the in-memory store's 64 MiB retention cap, so a
/// document of up to 200 MiB can be held. Not a conformance-tested store.
mod big {
    use hsearch_object::{
        ByteRange, Error, Metadata, MetadataStore, Mutation, ObjectBackend, ObjectKey, Payload,
        RequestContext, RequestId, VersionToken,
    };
    use hsearch_object::{ObjectInfo, Operation, Precondition, WriteReceipt};
    use std::collections::{BTreeMap, hash_map::DefaultHasher};
    use std::hash::{Hash, Hasher};
    use std::sync::Mutex;

    #[derive(Default)]
    struct State {
        current: BTreeMap<ObjectKey, VersionToken>,
        versions: BTreeMap<VersionToken, (ObjectKey, ObjectInfo, Vec<u8>)>,
        accepted: BTreeMap<RequestId, (u64, WriteReceipt)>,
        next: u64,
    }

    #[derive(Default)]
    pub struct BigStore(Mutex<State>);

    fn digest(request: &Mutation) -> u64 {
        let mut h = DefaultHasher::new();
        request.key.as_bytes().hash(&mut h);
        request.payload.as_bytes().hash(&mut h);
        request.metadata.as_bytes().hash(&mut h);
        h.finish()
    }

    impl BigStore {
        fn write(&self, ctx: &RequestContext, request: &Mutation) -> Result<WriteReceipt, Error> {
            ctx.charge_request((request.payload.len() + request.key.len()) as u64)?;
            let mut s = self.0.lock().map_err(|_| Error::Unavailable)?;
            let d = digest(request);
            if let Some((seen, receipt)) = s.accepted.get(&request.request_id) {
                return if *seen == d {
                    Ok(receipt.clone())
                } else {
                    Err(Error::IdentityConflict)
                };
            }
            match (&request.precondition, s.current.get(&request.key)) {
                (Precondition::Absent, None) => {}
                (Precondition::Matches(a), Some(b)) if a == b => {}
                _ => return Err(Error::Conflict),
            }
            s.next += 1;
            let version = VersionToken::new(&s.next.to_be_bytes())?;
            let info = ObjectInfo {
                version: version.clone(),
                length: request.payload.len() as u64,
                metadata: request.metadata.clone(),
            };
            let receipt = WriteReceipt {
                request_id: request.request_id.clone(),
                object: info.clone(),
            };
            s.current.insert(request.key.clone(), version.clone());
            s.versions.insert(
                version,
                (
                    request.key.clone(),
                    info,
                    request.payload.as_bytes().to_vec(),
                ),
            );
            s.accepted
                .insert(request.request_id.clone(), (d, receipt.clone()));
            Ok(receipt)
        }
    }

    impl ObjectBackend for BigStore {
        fn head<'a>(
            &'a self,
            ctx: &'a RequestContext,
            key: &'a ObjectKey,
        ) -> Operation<'a, ObjectInfo> {
            Box::pin(async move {
                ctx.check()?;
                let s = self.0.lock().map_err(|_| Error::Unavailable)?;
                let v = s.current.get(key).ok_or(Error::NotFound)?;
                Ok(s.versions.get(v).ok_or(Error::Corrupt)?.1.clone())
            })
        }
        fn create<'a>(
            &'a self,
            ctx: &'a RequestContext,
            request: &'a Mutation,
        ) -> Operation<'a, WriteReceipt> {
            Box::pin(async move {
                if request.precondition != Precondition::Absent {
                    return Err(Error::InvalidInput);
                }
                self.write(ctx, request)
            })
        }
        fn read_range<'a>(
            &'a self,
            ctx: &'a RequestContext,
            key: &'a ObjectKey,
            version: &'a VersionToken,
            range: ByteRange,
        ) -> Operation<'a, Payload> {
            Box::pin(async move {
                ctx.check()?;
                let s = self.0.lock().map_err(|_| Error::Unavailable)?;
                let (k, info, bytes) = s.versions.get(version).ok_or(Error::NotFound)?;
                if k != key {
                    return Err(Error::NotFound);
                }
                range.validate(info.length)?;
                let start = range.offset() as usize;
                Payload::new(&bytes[start..start + range.length() as usize])
            })
        }
    }

    /// An upload held in memory until it completes: the profile's store has no part limits.
    pub struct Upload {
        key: ObjectKey,
        data: Vec<u8>,
    }

    impl hsearch_object::LargeObjectWriter for BigStore {
        type Upload = Upload;
        fn begin_upload<'a>(
            &'a self,
            _: &'a RequestContext,
            key: &'a ObjectKey,
        ) -> Operation<'a, Upload> {
            Box::pin(async move {
                Ok(Upload {
                    key: key.clone(),
                    data: Vec::new(),
                })
            })
        }
        fn put_part<'a>(
            &'a self,
            _: &'a RequestContext,
            upload: &'a mut Upload,
            bytes: &'a [u8],
        ) -> Operation<'a, ()> {
            Box::pin(async move {
                upload.data.extend_from_slice(bytes);
                Ok(())
            })
        }
        fn complete_upload<'a>(
            &'a self,
            ctx: &'a RequestContext,
            upload: &'a Upload,
        ) -> Operation<'a, (VersionToken, u64)> {
            Box::pin(async move {
                let mut id = b"upload-".to_vec();
                id.extend_from_slice(upload.key.as_bytes());
                let request = Mutation {
                    request_id: RequestId::new(&id)?,
                    key: upload.key.clone(),
                    payload: Payload::new(&upload.data)?,
                    metadata: Metadata::new(b"")?,
                    precondition: Precondition::Absent,
                };
                let receipt = self.write(ctx, &request)?;
                Ok((receipt.object.version, receipt.object.length))
            })
        }
        fn abort_upload<'a>(&'a self, _: &'a RequestContext, _: &'a Upload) -> Operation<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    impl MetadataStore for BigStore {
        fn compare_exchange<'a>(
            &'a self,
            ctx: &'a RequestContext,
            request: &'a Mutation,
        ) -> Operation<'a, WriteReceipt> {
            Box::pin(async move { self.write(ctx, request) })
        }
        fn resolve<'a>(
            &'a self,
            _: &'a RequestContext,
            request: &'a Mutation,
        ) -> Operation<'a, WriteReceipt> {
            Box::pin(async move {
                let s = self.0.lock().map_err(|_| Error::Unavailable)?;
                match s.accepted.get(&request.request_id) {
                    Some((d, r)) if *d == digest(request) => Ok(r.clone()),
                    Some(_) => Err(Error::IdentityConflict),
                    None => Err(Error::ResolutionUnavailable),
                }
            })
        }
    }
}
use big::BigStore;

fn main() {
    let mut args = std::env::args().skip(1);
    let mib: f64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let docs: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(1);
    let bytes = (mib * 1_048_576.0) as usize;
    let dir = std::env::temp_dir().join(format!("hsearch-largefield-{}", std::process::id()));
    let store = Arc::new(BigStore::default());
    let limits = Limits {
        max_document_bytes: 200 << 20,
        max_bulk_bytes: 256 << 20,
        max_bulk_items: 100,
        buffer_bytes: 256 << 20,
        memory_budget_bytes: 8192 << 20,
        auto_merge_mb: 0,
    };
    let node = Node::with_limits(store, dir.clone(), limits).expect("node");
    let mapping = json!({"mappings":{"properties":{"title":{"type":"text"},"tag":{"type":"keyword"},
        "n":{"type":"integer"},"ok":{"type":"boolean"},"at":{"type":"date"}}}});
    node.create("orders", &mapping).expect("create");
    // Distinct words, so postings are not trivially one term.
    let mut title = String::with_capacity(bytes);
    let mut i = 0u64;
    while title.len() < bytes {
        title.push_str(&format!("w{} ", i % 5000));
        i += 1;
    }
    let baseline = status_kb("VmRSS:");
    let started = Instant::now();
    for d in 0..docs {
        let body = format!(
            "{{\"index\":{{\"_id\":\"d{d}\"}}}}\n{{\"title\":\"{title}\",\"tag\":\"a\",\"n\":1,\"ok\":true,\"at\":1}}\n"
        );
        let out = node
            .bulk(
                body.as_bytes(),
                Some(body.len() as u64),
                Some("orders"),
                Wait::Searchable,
            )
            .expect("bulk")
            .1;
        assert_eq!(out["errors"], json!(false), "{out}");
    }
    let secs = started.elapsed().as_secs_f64();
    let peak = status_kb("VmHWM:").saturating_sub(baseline);
    println!(
        "{docs} doc(s) of {:.2} MiB: {secs:.2}s, {:.1} MiB/s, peak RSS over baseline {:.1} MiB ({:.1}x the document)",
        title.len() as f64 / 1_048_576.0,
        docs as f64 * title.len() as f64 / 1_048_576.0 / secs,
        peak as f64 / 1024.0,
        peak as f64 / 1024.0 / (title.len() as f64 / 1_048_576.0),
    );
    let _ = std::fs::remove_dir_all(dir);
}
