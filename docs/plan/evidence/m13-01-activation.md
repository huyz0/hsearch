# M13.01 evidence: paged catalog partitioning and cold activation under caps

`provisional`: RustFS 1.0.4 (digest pinned in `scripts/rustfs.sh`) in Docker on a shared 4-CPU cloud VM, loopback, release build, one host. Numbers are relative. No MinIO, no real S3, no network. Reproduce:

```
cargo run --release -p hsearch-lane --example activation_profile -- footprint <idle> set|registry
eval "$(scripts/rustfs.sh up)"; cargo run --release -p hsearch-lane --example activation_profile -- cold 40 25
python3 tools/catalog_index_scale.py --output /private/new-directory     # 100,000 vs 1,000,000 idle indexes
```

## What is built (`crates/lane`)

- **Partition strategy** (`partition.rs`): `bucket_bits_for(names)` picks the smallest fan-out (at least 2 buckets) at which the names average at most `PAGE_TARGET` = 1,024 per page, half of the format's 2,048-entry bound, so hash skew and the tombstones a catalog keeps do not overflow a page. A page object exists only once a name is written to it, so a tenant of 50 names costs a root and at most two pages whatever the fan-out; 10 million names in one catalog get 14 bits (16,384 pages, 610 names a page on average).
- **Lazy lifecycle controller** (`activation.rs`): `ActiveSet<H>` holds only indexes that are active or loading. Caps: active count, bytes, loads in flight. One load per cold index (later callers get `Pending`), a burst beyond the loading cap is refused (`Loading`), the least recently used index nobody holds a lease on is evicted to make room, and a set whose every index is leased refuses (`Capacity`) instead of growing. A refusal never evicts anything: feasibility is checked first. Eviction is O(1) (a doubly linked list of unleased indexes; leasing unlinks, the last release relinks). Sans-I/O: the caller resolves, reads and opens the engine and reports what it got. A load nobody answers within `load_ticks` of the caller's logical clock is reclaimed (a cancelled or panicked caller cannot hold a loading place), and its late `finish` is refused with the handle handed back to be closed, as is a handle that does not fit.
- **Footprint model and RustFS profile** (`examples/activation_profile.rs`), the **re-keyed catalog gate** (`tools/catalog_index_scale.py`, M02.06 condition C8).

## Tests (`crates/lane/tests/activation.rs`, 12, written first and seen red)

Hit and single-flight load; LRU order; a leased index never evicted and a full set refusing; byte cap; the loading cap and a failed load returning its place; a load that grows past its estimate (makes room, or is given up whole, its reservation returned and the handle handed back); a load nobody answers being reclaimed and its late ticket refused; invalid limits, a stale ticket, a double release; work per operation (at most 12 steps, hits and misses both exercised, hit counts equal) whether ids come from 1,000 or 10,000,000 indexes; the partition strategy's bounds; and a cold index behind a rotted catalog page failing cleanly, then activating after `repair_page` at 1 HEAD + 1 read for the resolve. Seven mutations were each killed by a named test (the seventh is disabling the reclaim of an unanswered load): evicting the head instead of the tail, a lease that leaves the index evictable, no loading cap, no single flight, ignoring the byte cap, and the off-by-one in the fan-out.

## Measurements

**Memory with idle indexes (model).** This restates the cap rather than discovering it: the set holds at most `max_active` entries by construction, and the registry is the design the cap exists to avoid (one record per index, as per-index bookkeeping or timers would need). What it shows is the cost of the alternative and that the set's own operations stay at constant steps. One process, release build; the registry has one 48-byte record per index (what per-index bookkeeping or timers would need), the set has cap 4,096 and 200,000 random activations over the population:

| design | idle indexes | RSS growth | entries held | note |
|---|---|---|---|---|
| registry | 100,000 | 6 MiB | 100,000 | |
| registry | 1,000,000 | 98 MiB | 1,000,000 | |
| registry | 10,000,000 | 784 MiB | 10,000,000 | |
| set | 100,000 | under 1 MiB | 4,096 | set 830 KiB, worst 10 steps an operation (begin + get, hits and misses) |
| set | 1,000,000 | under 1 MiB | 4,096 | set 830 KiB, worst 10 steps |
| set | 10,000,000 | under 1 MiB | 4,096 | set 831 KiB, worst 10 steps |

**On RustFS** (40 tenants x 25 names = 1,000 indexes, a catalog each with 2 buckets, one shared lane head; setup 2,043 PUT, about 2 per name):

| step | requests | latency |
|---|---|---|
| idle: 1,000 indexes catalogued, none active, 3 s | 0 requests | (a harness with no poller: it shows the controller adds none, not that a node design has none) |
| cold activation = resolve the name + read the lane head | 2 GET + 2 HEAD, 0 PUT, 0 LIST | p50 4.5 to 5.6 ms, p99 6.0 to 19.7 ms (5 runs) |
| active index (hit) | 0 requests | under 0.1 ms |
| burst: all 1,000 cold at once, 16 workers, caps 64 active / 8 loading | 2 GET + 2 HEAD each, 0 PUT, 0 LIST | 730 to 850 activations/s, p50 13 to 17 ms, p99 71 to 85 ms; 936 evictions (5 runs); the set never held more than its cap of 64, which the cap enforces, so that is a check of the cap, not a finding |

**Fixed-work gate at 100x (C8).** The M02.08 model re-keyed on the index (one row per index, no tenant column), at 100,000 and 1,000,000 idle indexes (100x the model's 1,000 and 10,000), fresh processes, 3 repetitions: the hot index's selection costs 2 statements and 17 rows at both sizes, plans are indexed `SEARCH` only, median peak RSS 19.37 MB against 19.42 MB (ratio 1.003, gate 1.10). Two controls must refuse and do: a scan that holds every row (33.4 MB against 156.0 MB, ratio 4.67, table `SCAN`, rows touched grow with the catalog) and a scan that streams the rows without holding any (RSS flat at 1.001, so memory alone is not what refuses it: it fails on the table `SCAN` and on rows touched). Fixtures 10.5 MB and 104.4 MB. Report: `evidence/m13-01/index-scale-report.json`.

What this gate is not: it checks that SQLite answers the two model queries from the primary key at fixed work and flat memory. The 1.10 ratio on a 19 MB interpreter baseline is a coarse memory screen (about 1.9 MB of room); the work and plan checks carry the weight. It says nothing about the Rust catalog (whose resolve is 1 HEAD + 1 GET, measured in M03.05) or physical I/O.

## Arithmetic at PR-01 scale (a model, not a measurement)

At 1M tenants of 50 indexes (50M indexes), from the layout measured above: one identity binding object per index (50M), one root per tenant and at most 2 page objects per tenant (about 3M), so about 53M catalog objects and about 100M PUT to create it. While idle it costs nothing: no timer, no poll and no per-index state in a node; a node's memory is set by `max_active` and `max_bytes`. At 10M indexes with 90% idle the same holds for the 90%. These sizes were not built.

## Limits and downsides

- **Not covered from the task's validation list:** a one-document index's steady-state requests (PR-02) are not measured: no document is written, no engine opened and no index queried here, so that waits on the engine wiring (M04, M06). "At least 99% idle" is the model (the set at 10M and the SQLite catalog at 1M idle); the RustFS run is 1,000 indexes with 0% active before its burst and every index cold during it. "No O(total indexes) polling" is true of this controller by construction and is not shown for a node, which does not exist yet.
- **`ActiveSet` assumes one logical clock the caller advances;** with a constant clock a lost load is never reclaimed.
- **The fan-out is chosen at creation** from a name count the creator must foresee: a tenant created at 0 names gets 2 pages and can hold about 4,096 names comfortably (1,024 each) before a page nears its bound; re-splitting is not built (the format fixes the bits).
- **The engine is not wired in.** `ActiveSet<H>` is generic: it controls when an index is loaded and evicted and holds a handle, but opening and closing a real engine (M04, and the node and lifecycle tasks M06 and M07) is the caller's. "Lazy engine lifecycle" is delivered as the controller, not as an engine integration.
- **A cold activation costs 2 HEAD + 2 GET here** (resolve + lane head), with the lane head read per index. It is cacheable across the indexes of a lane; that cache is not built, and the figure is an upper bound for that reason.
- **A refusal under the loading cap is a signal, not a queue.** The profile's workers spin and retry (2.4 million waits in a 1.3 s burst); a caller should queue or back off. The set holds no waiters.
- **Refused when full of leases.** A node whose active indexes are all in use refuses new ones; sizing `max_active` against concurrent queries is the caller's.
- **LRU only, under pressure.** No idle timeout: an inactive index stays until something needs the room. That costs memory held by cold-but-recent indexes and avoids any timer.
- The partition target (1,024 entries) is a planning rule: a catalog that outgrows its fan-out is not re-split here (the format fixes the bits at creation).
- Measured on 1,000 indexes against RustFS; the larger sizes are the SQLite model (1,000,000) and the set's footprint (10,000,000). No fleet, no ARM, no real provider.
