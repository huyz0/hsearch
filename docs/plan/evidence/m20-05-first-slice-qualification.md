# M20.05 evidence: the first slice end to end

Provisional: real RustFS 1.0.4 (digest-pinned, `dev/images.json`) in Docker, **debug build**, one node, shared cloud container (x86_64, 4 CPUs, 15 GiB, Linux 6.18). Relative numbers only.

Reproduce from a clean checkout with Docker: `scripts/e2e.sh` (builds `hsearch-node`, starts the pinned RustFS unless `HSEARCH_S3_ENDPOINT` is set, runs `scripts/e2e.py`, which speaks plain HTTP with Python's `urllib`, no SDK, and starts and kills the node itself). `N=20000 scripts/e2e.sh` is a quicker run. CI runs the same script against RustFS in the `check` job and writes the output to the job summary. There is no docker-compose file: the one command above is the one-command run.

Output of the run on this container (verbatim, `scripts/e2e.sh`):

```
hardware: x86_64, 4 CPUs, Linux-6.18.44-fc-v445-x86_64-with-glibc2.39
binary: target/debug/hsearch-node
documents: 100000, bulk size 5000, bucket e2e-1794601291-3689
bulk latency ms (n=20, 5000 docs each): p50 919, p95 1032, max 1032; total 18.5s
search match_all ms (n=30): p50 31.0, p95 40.2
search term ms (n=30): p50 19.1, p95 25.7
search range ms (n=30): p50 18.6, p95 27.7
search match ms (n=30): p50 44.1, p95 63.9
search bool ms (n=30): p50 34.8, p95 53.1
PASS live totals: match_all 100000, tag t0 33334
PASS restart on an empty data dir serves every document
restart plus first answer (full log replay): 11.4s
kill -9 after 1 acknowledged bulk(s) of 4 sent
PASS after kill: 108000 documents, between 105000 acknowledged and 120000 sent
PASS all 5000 acknowledged documents present
unacknowledged documents that became durable: 3000 (allowed: they were never acknowledged)
PASS the restarted node accepts new writes
PASS a second restart answers the fixed query set identically
```

What this shows: 100,000 documents indexed over HTTP; the node stopped and restarted on an empty data directory answers every document; a `kill -9` a moment after one bulk was acknowledged (the next bulk in flight), followed by a restart on an empty data directory, lost no acknowledged document (the script checks this one property from outside); the restarted node accepts writes. In this run the kill landed inside the second bulk: 3,000 of its 5,000 documents (three of five slots) were durable and unacknowledged, which is allowed and is the partial case; the 5,000 acknowledged documents were all present. The kill hits a healthy store, so this is a node-crash test; the durability boundaries inside the write path (before the slot write, after it before the ack, after the ack before apply, mid-publish) are the M20.03 crash matrix on RustFS (`crates/index/tests/rustfs.rs`), which this script does not repeat. A bulk above 1,000 operations is several log slots, so an unacknowledged bulk can be partly durable; only an acknowledged bulk is guaranteed whole.

Requests to the store per HTTP call, by type, on RustFS (`crates/node/tests/rustfs.rs`, `requests_per_http_call_by_type_never_list`, counted from `S3Store::counts`; bulk, search, get and LIST are asserted, create and refresh are printed only):

| Call | GET | PUT | HEAD | LIST | DELETE |
|---|---|---|---|---|---|
| create index | 1 | 3 | 2 | 0 | 0 |
| bulk of 1,000, durable | 0 | 1 | 0 | 0 | 0 |
| bulk of 1,000, `refresh=wait_for` | 0 | 1 | 0 | 0 | 0 |
| refresh | 0 | 0 | 0 | 0 | 0 |
| search (match_all, term, range, match, bool) | 0 | 0 | 0 | 0 | 0 |
| get by id | 0 | 0 | 0 | 0 | 0 |

A search costs no store request because the node serves from the engine it rebuilt locally; nothing yet publishes segments from the node, so searching from segments in the store, and restarting from them, are not exercised over HTTP. A restart replays the whole log (11.3 s for 100k documents), which is the cost of that gap.

Deliberate gaps of this slice: authentication and TLS, BYOK, quorum mode, multiple nodes (one writer per index, fenced by a stale root or slot), frozen formats (v0 formats are disposable), real-time get (get reads the latest searchable view), mapping evolution and dynamic mapping, generated ids, `create`/`update` bulk actions, scoring (`_score` is null), aggregations, scroll, socket timeouts, publishing segments from the node, multipart upload (M03.10). Bulks in the 100k run are 5,000 documents, several 1,000-operation slots each, so their request count is the table's per-1,000 figure times five; it was not measured separately. Not run here: the Linux dev container run and the macOS and ARM runs; this run is on a cloud container, and CI runs the script on ubuntu-24.04.
