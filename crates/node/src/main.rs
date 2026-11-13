//! `hsearch-node`: serve the HTTP subset against an S3-compatible store.
//!
//! Environment: `HSEARCH_S3_ENDPOINT` (host:port), `HSEARCH_S3_ACCESS_KEY`,
//! `HSEARCH_S3_SECRET_KEY`, optional `HSEARCH_S3_BUCKET` (default `hsearch`).
//! Flags: `--listen ADDR` (default 127.0.0.1:9200), `--data-dir DIR` (default
//! `./hsearch-data`, disposable), `--insecure-no-auth-listen` to allow a
//! non-loopback address.
use std::sync::Arc;
use std::time::Duration;

use hsearch_node::{Node, parse_args, serve};
use hsearch_object::{Budget, Cancellation, RequestContext};
use hsearch_s3::{S3Config, S3Store};

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

fn run() -> Result<(), String> {
    let options = parse_args(std::env::args().skip(1))?;
    let store = S3Store::new(S3Config {
        endpoint: env("HSEARCH_S3_ENDPOINT")?,
        bucket: std::env::var("HSEARCH_S3_BUCKET").unwrap_or_else(|_| "hsearch".into()),
        access_key: env("HSEARCH_S3_ACCESS_KEY")?,
        secret_key: env("HSEARCH_S3_SECRET_KEY")?,
        region: "us-east-1".into(),
    })
    .map_err(|e| format!("store configuration: {e}"))?;
    let context = RequestContext::new(
        Duration::from_secs(60),
        Cancellation::default(),
        Budget {
            request_bytes: 1 << 20,
            response_bytes: 1 << 20,
            attempts: 64,
        },
    )
    .map_err(|e| e.to_string())?;
    store
        .prepare_bucket(&context)
        .map_err(|e| format!("preparing the bucket: {e}"))?;
    let node = Arc::new(Node::new(Arc::new(store), options.data_dir));
    let running = serve(node, &options.listen, 8, options.insecure)?;
    println!("listening on http://{}", running.addr);
    // Serve until the process is stopped.
    loop {
        std::thread::park();
    }
}

fn main() {
    if let Err(message) = run() {
        eprintln!("hsearch-node: {message}");
        std::process::exit(2);
    }
}
