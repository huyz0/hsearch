//! M06.18 measurements of the payload write path. One markdown row to stdout.
//!
//!     cargo run --release -p hsearch-store --example payload_profile -- <MiB> <first|last>
//!
//! `first` puts the large field before a small one, `last` after it. The store discards the bytes
//! (it keeps lengths), so what is measured is this crate's own cost: assembling the batch body, the
//! digest and the framing of parts, and this process's peak resident memory beside the document.
//! Provisional: one shared host, no network, no real store, so the numbers are relative. A real
//! store adds one request per 8 MiB and its own latency.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hsearch_engine::{Mutation, Value, encode_batch};
use hsearch_format::Layout;
use hsearch_object::{
    ByteRange, Error, LargeObjectWriter, Metadata, MetadataStore, Mutation as Put, ObjectBackend,
    ObjectInfo, ObjectKey, Operation, Payload, RequestContext, VersionToken, WriteReceipt,
};
use hsearch_store::payload::{ObjectPayloads, PayloadSink};
use sha2::{Digest, Sha256};

#[derive(Default)]
struct Discard {
    lengths: Mutex<HashMap<Vec<u8>, usize>>,
    requests: AtomicU64,
    largest: AtomicU64,
}

fn info(n: usize) -> ObjectInfo {
    ObjectInfo {
        version: VersionToken::new(b"v").unwrap(),
        length: n as u64,
        metadata: Metadata::new(b"").unwrap(),
    }
}

impl ObjectBackend for Discard {
    fn head<'a>(&'a self, _: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        Box::pin(async move {
            let l = self.lengths.lock().unwrap();
            l.get(k.as_bytes()).map(|n| info(*n)).ok_or(Error::NotFound)
        })
    }
    fn create<'a>(&'a self, _: &'a RequestContext, r: &'a Put) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.largest
                .fetch_max(r.payload.len() as u64, Ordering::Relaxed);
            self.lengths
                .lock()
                .unwrap()
                .insert(r.key.as_bytes().to_vec(), r.payload.len());
            Ok(WriteReceipt {
                request_id: r.request_id.clone(),
                object: info(r.payload.len()),
            })
        })
    }
    fn read_range<'a>(
        &'a self,
        _: &'a RequestContext,
        _: &'a ObjectKey,
        _: &'a VersionToken,
        _: ByteRange,
    ) -> Operation<'a, Payload> {
        Box::pin(async { Err(Error::Unsupported) })
    }
}

impl MetadataStore for Discard {
    fn compare_exchange<'a>(
        &'a self,
        _: &'a RequestContext,
        _: &'a Put,
    ) -> Operation<'a, WriteReceipt> {
        Box::pin(async { Err(Error::Unsupported) })
    }
    fn remove_version<'a>(
        &'a self,
        _: &'a RequestContext,
        _: &'a ObjectKey,
        _: &'a VersionToken,
    ) -> Operation<'a, ()> {
        Box::pin(async { Err(Error::Unsupported) })
    }
    fn resolve<'a>(&'a self, _: &'a RequestContext, _: &'a Put) -> Operation<'a, WriteReceipt> {
        Box::pin(async { Err(Error::Unsupported) })
    }
}

impl LargeObjectWriter for Discard {
    type Upload = (Vec<u8>, usize);
    fn begin_upload<'a>(
        &'a self,
        _: &'a RequestContext,
        k: &'a ObjectKey,
    ) -> Operation<'a, Self::Upload> {
        Box::pin(async move {
            self.requests.fetch_add(1, Ordering::Relaxed);
            Ok((k.as_bytes().to_vec(), 0))
        })
    }
    fn put_part<'a>(
        &'a self,
        _: &'a RequestContext,
        u: &'a mut Self::Upload,
        b: &'a [u8],
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.largest.fetch_max(b.len() as u64, Ordering::Relaxed);
            u.1 += b.len();
            Ok(())
        })
    }
    fn complete_upload<'a>(
        &'a self,
        _: &'a RequestContext,
        u: &'a Self::Upload,
    ) -> Operation<'a, (VersionToken, u64)> {
        Box::pin(async move {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.lengths.lock().unwrap().insert(u.0.clone(), u.1);
            Ok((VersionToken::new(b"v").unwrap(), u.1 as u64))
        })
    }
    fn abort_upload<'a>(&'a self, _: &'a RequestContext, _: &'a Self::Upload) -> Operation<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// Peak resident set of this process so far, in MiB (Linux).
fn peak_mib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())
        })
        .map_or(f64::NAN, |kb| kb / 1024.0)
}

fn best<T>(runs: usize, mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut keep = None;
    let mut fastest = Duration::MAX;
    for _ in 0..runs {
        let t = Instant::now();
        let out = f();
        fastest = fastest.min(t.elapsed());
        keep = Some(out);
    }
    (fastest, keep.unwrap())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mib: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
    let first = args.get(2).is_none_or(|s| s == "first");
    let text = "x".repeat(mib << 20);
    let fields_first = [("body", Value::String(&text)), ("tag", Value::String("t"))];
    let fields_last = [("tag", Value::String("t")), ("body", Value::String(&text))];
    let fields: &[(&str, Value<'_>)] = if first { &fields_first } else { &fields_last };
    let mutations = [Mutation::Put {
        id: "doc",
        version: 1,
        fields,
    }];
    let baseline = peak_mib();
    let body = encode_batch("r", &mutations).unwrap();
    let after_encode = peak_mib();
    let (encode, _) = best(5, || encode_batch("r", &mutations).unwrap());
    let (hash, _) = best(5, || Sha256::digest(&body));
    let store = Arc::new(Discard::default());
    let layout = Layout::new("bench").unwrap();
    let sink = ObjectPayloads::new(Arc::clone(&store), layout, Duration::from_secs(60));
    let (stage, _) = best(5, || sink.stage(&[7; 16], &body).unwrap());
    let ms = |d: Duration| format!("{:.1} ms", d.as_secs_f64() * 1e3);
    println!(
        "| {mib} MiB, {} | {} | {:.0} MiB | {} ({:.0} MB/s) | {} | {} | {} MiB |",
        if first {
            "large field first"
        } else {
            "large field last"
        },
        ms(encode),
        after_encode - baseline,
        ms(hash),
        body.len() as f64 / hash.as_secs_f64() / 1e6,
        ms(stage),
        store.requests.load(Ordering::Relaxed) / 5,
        store.largest.load(Ordering::Relaxed) >> 20,
    );
}
