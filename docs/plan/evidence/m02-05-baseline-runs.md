# M02.05 baseline: two fresh runs of the object-store bench

`provisional`: one 4-CPU, 16 GiB sandbox VM (kernel 6.18.44-fc-v470) running both client and store, RustFS 1.0.4 in Docker (image `rustfs/rustfs:1.0.4@sha256:ed9210d6411d77ffceb70d94b964a3cb7d225983368bc25bebe01e995293dcc2`, started by `scripts/rustfs.sh up`), repository commit `a06bed57b37159d3ee6765c75f6357f804811b52`, release build.

Command, run twice back to back with the same binary and a fresh bucket namespace per cell: `BENCH_ONLY=tables BENCH_SECONDS=5 cargo run --release -p hsearch-s3 --example bench` with `HSEARCH_S3_ENDPOINT=127.0.0.1:9000` and the test credentials of `scripts/rustfs.sh`. Each cell is five seconds, so the large-object cells hold about 30-120 requests: **no p99 here meets the 10,000-request rule of the M02.05 protocol**, and the p99 columns are reported only to show how unstable they are. There is one configuration, so interleaving does not apply; the runs are sequential.

## Run 1 (raw)

```
cpus: 4, seconds per cell: 5

## PUT by payload size, one writer, distinct keys (batching)

| payload | PUT/s | MiB/s | p50 ms | p99 ms |
|---|---|---|---|---|
| 1 KiB | 223 | 0.2 | 4.3 | 6.8 |
| 16 KiB | 211 | 3.3 | 4.6 | 7.2 |
| 256 KiB | 126 | 31.4 | 7.5 | 11.0 |
| 1024 KiB | 65 | 65.5 | 14.4 | 38.3 |
| 4096 KiB | 24 | 94.4 | 40.1 | 103.0 |
| 16384 KiB | 6 | 90.4 | 151.3 | 518.1 |

## Concurrent PUTs of 64 KiB to distinct keys

| writers | PUT/s | p50 ms | p99 ms |
|---|---|---|---|
| 1 | 108 | 6.2 | 85.4 |
| 2 | 306 | 5.8 | 11.8 |
| 4 | 517 | 7.4 | 10.7 |
| 8 | 700 | 11.1 | 18.7 |
| 16 | 767 | 20.5 | 30.9 |
| 32 | 785 | 40.3 | 59.8 |

## Contended compare-and-set on one 1 KiB key

| writers | attempts/s | winners/s | conflicts | other errors | p50 ms | p99 ms |
|---|---|---|---|---|---|---|
| 1 | 85 | 85 | 0 | 0 | 11.4 | 16.8 |
| 2 | 121 | 95 | 132 | 0 | 10.8 | 74.6 |
| 4 | 153 | 80 | 367 | 0 | 18.1 | 85.9 |
| 8 | 140 | 83 | 287 | 0 | 13.9 | 619.5 |
| 16 | 112 | 93 | 76 | 25 | 10.3 | 2359.1 |

requests: RequestCounts { get: 1, put: 22323, head: 2016, list: 887, delete: 0 }
other error variants seen: ["ResolutionUnavailable"]
```

## Run 2 (raw)

```
cpus: 4, seconds per cell: 5

## PUT by payload size, one writer, distinct keys (batching)

| payload | PUT/s | MiB/s | p50 ms | p99 ms |
|---|---|---|---|---|
| 1 KiB | 196 | 0.2 | 5.0 | 7.7 |
| 16 KiB | 188 | 2.9 | 5.2 | 8.1 |
| 256 KiB | 122 | 30.4 | 8.1 | 10.9 |
| 1024 KiB | 63 | 63.4 | 14.9 | 41.1 |
| 4096 KiB | 23 | 90.6 | 42.3 | 109.7 |
| 16384 KiB | 6 | 101.7 | 155.2 | 182.6 |

## Concurrent PUTs of 64 KiB to distinct keys

| writers | PUT/s | p50 ms | p99 ms |
|---|---|---|---|
| 1 | 127 | 6.7 | 30.7 |
| 2 | 273 | 6.6 | 14.8 |
| 4 | 477 | 8.1 | 13.9 |
| 8 | 674 | 11.7 | 16.5 |
| 16 | 773 | 20.3 | 32.4 |
| 32 | 767 | 41.0 | 59.8 |

## Contended compare-and-set on one 1 KiB key

| writers | attempts/s | winners/s | conflicts | other errors | p50 ms | p99 ms |
|---|---|---|---|---|---|---|
| 1 | 95 | 95 | 0 | 0 | 10.3 | 17.5 |
| 2 | 123 | 96 | 137 | 0 | 10.9 | 67.8 |
| 4 | 150 | 76 | 372 | 0 | 17.7 | 94.5 |
| 8 | 138 | 87 | 263 | 0 | 12.6 | 670.7 |
| 16 | 105 | 93 | 46 | 19 | 10.4 | 3047.0 |

requests: RequestCounts { get: 1, put: 21598, head: 1829, list: 837, delete: 0 }
other error variants seen: ["ResolutionUnavailable"]
```

## Comparison

| cell | run 1 | run 2 | spread |
|---|---|---|---|
| 1 KiB PUT/s, p50 ms | 223, 4.3 | 196, 5.0 | 13% on rate |
| 16 KiB PUT/s, p50 ms | 211, 4.6 | 188, 5.2 | 11% on rate |
| 4 MiB PUT/s, MiB/s | 24, 94.4 | 23, 90.6 | 4% |
| 4 MiB p99 ms | 103.0 | 109.7 | 6% |
| 16 MiB p99 ms | 518.1 | 182.6 | 2.8x |
| 64 KiB, 16 writers, PUT/s | 767 | 773 | 1% |
| 64 KiB, 32 writers, PUT/s | 785 | 767 | 2% |
| hot-key CAS winners/s, 1 / 8 / 16 writers | 85 / 83 / 93 | 95 / 87 / 93 | up to 12% |
| hot-key CAS p99 at 16 writers, ms | 2,359 | 3,047 | 29% |

Reading: rates and medians reproduce within about 13%; saturation is at 16-32 writers near 770-785 small PUT/s on this run pair (the earlier recorded runs gave 692-742, so the figure moves by about 10% between sessions); tail latencies at large payloads and under contention do not reproduce (16 MiB p99 differs by 2.8x between two back-to-back runs). This is the evidence for the rule that no p99 target is judged from short cells. Not established: absolute numbers on a reference machine, a real provider, or a network path.

Earlier committed runs of the same bench (before the control and open-loop sections were added) are in [m02-bench-profile-bulk.md](m02-bench-profile-bulk.md) and its git history; they are context, not part of this pair.
