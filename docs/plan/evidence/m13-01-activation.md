# M13.01 evidence: paged catalog partitioning and cold activation under caps

`provisional`: RustFS 1.0.4 (digest pinned in `scripts/rustfs.sh`) in Docker on a shared 4-CPU cloud VM, loopback, release build, one host. Numbers are relative. No MinIO, no real S3, no network. Reproduce:

```
cargo run --release -p hsearch-lane --example activation_profile -- footprint <idle> set|registry
eval "$(scripts/rustfs.sh up)"; cargo run --release -p hsearch-lane --example activation_profile -- cold 40 25
python3 tools/catalog_index_scale.py --output /private/new-directory     # 100,000 vs 1,000,000 idle indexes
```

## What is built (`crates/lane`)

- **Partition strategy** (`partition.rs`): `bucket_bits_for(names)` picks the smallest fan-out (at least 2 buckets) at which the names average at most `PAGE_TARGET` = 1,024 per page, half of the format's 2,048-entry bound, so hash skew and the tombstones a catalog keeps do not overflow a page. A page object exists only once a name is written to it, so a tenant of 50 names costs a root and at most two pages whatever the fan-out; 10 million names in one catalog get 14 bits (16,384 pages, 610 names a page on average).
- **Lazy lifecycle controller** (`activation.rs`): `ActiveSet<H>` holds only indexes that are active or loading. Caps: active count, bytes, loads in flight. One load per cold index (later callers get `Pending`), a burst beyond the loading cap is refused (`Loading`), the least recently used index nobody holds a lease on is evicted to make room, and a set whose every index is leased refuses (`Capacity`) instead of growing. A refusal never evicts anything: feasibility is checked first. Eviction is O(1) (a doubly linked list of unleased indexes; leasing unlinks, the last release relinks). Sans-I/O: the caller resolves, reads and opens the engine and reports what it got.
- **Footprint model and RustFS profile** (`examples/activation_profile.rs`), the **re-keyed catalog gate** (`tools/catalog_index_scale.py`, M02.06 condition C8).

## Tests (`crates/lane/tests/activation.rs`, 11, written first and seen red)

Hit and single-flight load; LRU order; a leased index never evicted and a full set refusing; byte cap; the loading cap and a failed load returning its place; a load that grows past its estimate (makes room, or is given up whole and its reservation returned); invalid limits, a stale ticket, a double release; work per operation (at most 8 steps) and the set's footprint identical whether ids come from 1,000 or 10,000,000 indexes; the partition strategy's bounds; and a cold index behind a rotted catalog page failing cleanly, then activating after `repair_page` at 1 HEAD + 1 read for the resolve. Six mutations were each killed by a named test: evicting the head instead of the tail, a lease that leaves the index evictable, no loading cap, no single flight, ignoring the byte cap, and the off-by-one in the fan-out.

## Measurements

**Memory with idle indexes (model).** One process, release build; the registry has one 48-byte record per index (what per-index bookkeeping or timers would need), the set has cap 4,096 and 200,000 random activations over the population:

| design | idle indexes | RSS growth | entries held | note |
|---|---|---|---|---|
| registry | 100,000 | 6 MiB | 100,000 | |
| registry | 1,000,000 | 98 MiB | 1,000,000 | |
| registry | 10,000,000 | 784 MiB | 10,000,000 | |
| set | 100,000 | under 1 MiB | 4,096 | set 797 KiB, worst 8 steps an operation |
| set | 1,000,000 | under 1 MiB | 4,096 | set 796 KiB, worst 8 steps |
| set | 10,000,000 | under 1 MiB | 4,096 | set 795 KiB, worst 8 steps |

**On RustFS** (40 tenants x 25 names = 1,000 indexes, a catalog each with 2 buckets, one shared lane head; setup 2,043 PUT, about 2 per name):

| step | requests | latency |
|---|---|---|
| idle: 1,000 indexes catalogued, none active, 3 s | 0 requests | |
| cold activation = resolve the name + read the lane head | 2 GET + 2 HEAD, 0 PUT, 0 LIST | p50 4.5 to 5.3 ms, p99 6.0 to 9.0 ms (3 runs) |
| active index (hit) | 0 requests | under 0.1 ms |
| burst: all 1,000 cold at once, 16 workers, caps 64 active / 8 loading | 2 GET + 2 HEAD each, 0 PUT, 0 LIST | 750 to 800 activations/s, p50 15.6 to 16.8 ms, p99 71 to 76 ms; peak active 64; 936 evictions (3 runs) |

**Fixed-work gate at 100x (C8).** The M02.08 model re-keyed on the index (one row per index, no tenant column), at 100,000 and 1,000,000 idle indexes (100x the model's 1,000 and 10,000), fresh processes, 3 repetitions: the hot index's selection costs 2 statements and 17 rows at both sizes, plans are indexed `SEARCH` only, median peak RSS 19.09 MB against 19.05 MB (ratio 0.998, gate 1.10). The full-scan control refuses (47.3 MB against 292.4 MB, ratio 6.18, and a table `SCAN`). Fixtures 10.5 MB and 104.4 MB. Report: `evidence/m13-01/index-scale-report.json`.

## Arithmetic at PR-01 scale (a model, not a measurement)

At 1M tenants of 50 indexes (50M indexes), from the layout measured above: one identity binding object per index (50M), one root per tenant and at most 2 page objects per tenant (about 3M), so about 53M catalog objects and about 100M PUT to create it. While idle it costs nothing: no timer, no poll and no per-index state in a node; a node's memory is set by `max_active` and `max_bytes`. At 10M indexes with 90% idle the same holds for the 90%. These sizes were not built.

## Limits and downsides

- **The engine is not wired in.** `ActiveSet<H>` is generic: it controls when an index is loaded and evicted and holds a handle, but opening and closing a real engine (M04, and the node and lifecycle tasks M06 and M07) is the caller's. "Lazy engine lifecycle" is delivered as the controller, not as an engine integration.
- **A cold activation costs 2 HEAD + 2 GET here** (resolve + lane head), with the lane head read per index. It is cacheable across the indexes of a lane; that cache is not built, and the figure is an upper bound for that reason.
- **A refusal under the loading cap is a signal, not a queue.** The profile's workers spin and retry (2.4 million waits in a 1.3 s burst); a caller should queue or back off. The set holds no waiters.
- **Refused when full of leases.** A node whose active indexes are all in use refuses new ones; sizing `max_active` against concurrent queries is the caller's.
- **LRU only, under pressure.** No idle timeout: an inactive index stays until something needs the room. That costs memory held by cold-but-recent indexes and avoids any timer.
- The partition target (1,024 entries) is a planning rule: a catalog that outgrows its fan-out is not re-split here (the format fixes the bits at creation).
- Measured on 1,000 indexes against RustFS; the larger sizes are the SQLite model (1,000,000) and the set's footprint (10,000,000). No fleet, no ARM, no real provider.
