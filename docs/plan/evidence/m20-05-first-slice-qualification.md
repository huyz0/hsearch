# M20.05 evidence: the first slice end to end

Provisional: real RustFS 1.0.4 (digest-pinned, `dev/images.json`) in Docker, **debug build**, one node, shared cloud container (x86_64, 4 CPUs, 15 GiB, Linux 6.18). Relative numbers only.

Reproduce from a clean checkout with Docker: `scripts/e2e.sh` (builds `hsearch-node`, starts the pinned RustFS unless `HSEARCH_S3_ENDPOINT` is set, runs `scripts/e2e.py`, which speaks plain HTTP with Python's `urllib`, no SDK, and starts and kills the node itself). `N=20000 scripts/e2e.sh` is a quicker run. CI runs the same script against RustFS in the `check` job and writes the output to the job summary. There is no docker-compose file: the one command above is the one-command run.

Output of the run on this container (verbatim, `scripts/e2e.sh`):

```
hardware: x86_64, 4 CPUs, Linux-6.18.44-fc-v445-x86_64-with-glibc2.39
binary: target/debug/hsearch-node
documents: 100000, bulk size 5000, bucket e2e-1794601082-1913
bulk latency ms (n=20, 5000 docs each): p50 899, p95 968, max 968; total 17.8s
search match_all ms (n=30): p50 32.0, p95 36.1
search term ms (n=30): p50 19.7, p95 36.7
search range ms (n=30): p50 18.7, p95 30.3
search match ms (n=30): p50 39.0, p95 46.6
search bool ms (n=30): p50 32.9, p95 35.3
PASS live totals: match_all 100000, tag t0 33334
PASS restart on an empty data dir serves every document
restart plus first answer (full log replay): 11.3s
kill -9 after 1 acknowledged bulk(s) of 4 sent
PASS after kill: 105000 documents, between 105000 acknowledged and 120000 sent
PASS all 5000 acknowledged documents present
unacknowledged documents that became durable: 0
PASS the restarted node accepts new writes
PASS a second restart answers the fixed query set identically
```

What this shows: 100,000 documents indexed over HTTP; the node stopped and restarted on an empty data directory answers every document; a `kill -9` while a bulk was in flight, followed by a restart on an empty data directory, lost no acknowledged document and reported nothing unacknowledged as acknowledged; the restarted node accepts writes. In this run the in-flight bulk had not become durable (0 extra documents). A bulk above 1,000 operations is several log slots, so an unacknowledged bulk can be partly durable; only an acknowledged bulk is guaranteed whole.

Requests to the store per HTTP call, by type, on RustFS (`crates/node/tests/rustfs.rs`, `requests_per_http_call_by_type_never_list`, counted from `S3Store::counts`, asserted):

| Call | GET | PUT | HEAD | LIST | DELETE |
|---|---|---|---|---|---|
| create index | 1 | 3 | 2 | 0 | 0 |
| bulk of 1,000, durable | 0 | 1 | 0 | 0 | 0 |
| bulk of 1,000, `refresh=wait_for` | 0 | 1 | 0 | 0 | 0 |
| refresh | 0 | 0 | 0 | 0 | 0 |
| search (match_all, term, range, match, bool) | 0 | 0 | 0 | 0 | 0 |
| get by id | 0 | 0 | 0 | 0 | 0 |

A search costs no store request because the node serves from the engine it rebuilt locally; nothing yet publishes segments from the node, so searching from segments in the store, and restarting from them, are not exercised over HTTP. A restart replays the whole log (11.3 s for 100k documents), which is the cost of that gap.

Deliberate gaps of this slice: authentication and TLS, BYOK, quorum mode, multiple nodes (one writer per index, fenced by a stale root or slot), frozen formats (v0 formats are disposable), real-time get (get reads the latest searchable view), mapping evolution and dynamic mapping, generated ids, `create`/`update` bulk actions, scoring (`_score` is null), aggregations, scroll, socket timeouts, publishing segments from the node, multipart upload (M03.10). Not run here: the Linux dev container run and the macOS and ARM runs; this run is on a cloud container, and CI runs the script on ubuntu-24.04.
