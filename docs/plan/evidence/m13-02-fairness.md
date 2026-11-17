# M13.02 evidence: tenant fairness and hot placement

`provisional`: a deterministic model (fixed seeds, no I/O) on a shared 4-CPU cloud VM, release build. The admission and cache runs are simulations driven by a fixed arrival process, so they show what the mechanism does against a control, not what a node serves. Numbers are relative. No store, no network, so no MinIO, S3 or RustFS in this task: nothing here reads or writes an object, which is itself one of the checks. Reproduce:

```
cargo test -p hsearch-lane --test fairness
cargo run --release -p hsearch-lane --example fairness_profile
```

## What is built (`crates/lane`, all sans-I/O)

- **Admission** (`admission.rs`): per-tenant weighted fair shares of a node's in-flight capacity. A tenant is guaranteed `capacity x weight / active weight` (never more than the capacity less the reserve) and may borrow beyond it only while `reserve_permille` of the capacity stays free for tenants below their share. A refusal says why (`Share`: borrowing refused; `Saturated`: within share but the node is full) and changes nothing, so rejected work is countable by tenant and reason. A tenant has an entry only while it has work in flight: at most `capacity` entries, an idle tenant costs nothing, every operation is one hash lookup.
- **Weighted placement** (`rendezvous.rs`): weighted rendezvous hashing, `weight / -ln(u)`, with the logarithm computed in integer arithmetic from a 4,096-entry compile-time table so nodes on different CPUs agree on every placement. A node wins in proportion to its weight; the nodes for `k` replicas are always among those for `k + 1`, so a tenant that turns hot (`replicas_for`) gains replicas without moving the ones it has; zones are used before they repeat, so losing a zone loses at most one replica. Load feedback enters as a per-node factor on the weight.
- **Load feedback** (`feedback.rs`): reported utilisation, smoothed, mapped to a factor (1000 up to the target, falling linearly to a minimum at full), changed only past a deadband so a node hovering at its target does not flap, and neutral when silent. One record per node, bounded.
- **Cache quotas** (`quota.rs`): a floor every tenant keeps against other tenants' pressure and a ceiling a tenant evicts itself above. Pressure is taken from surplus above the floor, largest first, from a bounded sample (32 slots round-robin plus the requester's own); if no room exists that does not belong to a tenant under its floor the read is served uncached (`Bypass`). Slots exist only for tenants with bytes resident.
- **Bounded subscriptions** (`subscribe.rs`): which nodes hear about which index's head change by push, capped in all, per subscriber and per index (the fan-out of one notification). An index nobody has active has none.

**Moving compute does not change authority.** Placement is a pure function of the fleet and a tenant id: it holds no head, epoch or version and issues no request. Who may write is decided only by the structural head's conditional transition (M03.05). `moving_compute_neither_reads_nor_writes_authority` places one tenant under two fleets and three replica counts against a real head and asserts zero store requests and the head and its version byte-identical afterwards.

## Tests (`crates/lane/tests/fairness.rs`, 23)

Admission: one hot tenant refused (4,200 of 5,000) while 150 small tenants are all admitted from the reserve; weights set shares (75/25); within-share work waits for room rather than preempting; an alone tenant stops at capacity less reserve; bad input counted; a changed weight replaces the old; a departed tenant's weight does not shrink later shares. Placement: proportional to weight within 12% over 70,000 tenants (weights 1, 2, 4); removing a node moves **no** tenant that was not on it (0 of 20,000); replica prefix and zone spread for k = 1 to 8 over 2,000 tenants; load feedback moves tenants only off the loaded node (other moves: 0); zero-weight nodes skipped, refusals, the zone cap; replica count bounds. Quota: a 4,000-object cold storm evicts nothing from 50 tenants under their floor; surplus taken largest first and never below a floor; a bypass changes nothing; slot removal keeps others addressable. Feedback: gradual loss of weight, no change inside the deadband over 40 wobbles, silence is neutral, exact factors at both ends, table bound. Subscriptions: three caps, a refusal changes nothing, a leaving subscriber frees all.

Fifteen mutations were applied by hand and every one was killed by a named test except one equivalent: the reserve check, the share clamp, the capacity check, the departed-weight accounting (first run: survived, test added, then killed), the quota take, the tenant cap, victim ordering, zone fill, the load factor, the feedback deadband, the subscription total and the subscriber drop; `utilization <= target` against `<` maps to the same factor at the target, so it cannot be told apart. The code was written before the tests in this task, so those tests were not seen red first; the mutations above are the substitute and are weaker than red-first.

## Measurements (`examples/fairness_profile.rs`)

**One hot tenant among 999 small ones** (one node, capacity 1,000 in flight, service 10 ticks; the hot tenant offers 400 a tick, about 4,000 in flight against 1,000; each small tenant offers 2% a tick, about 200 in flight in all; 1,900 measured ticks):

| admission | small tenants refused | hot tenant served | node utilisation |
|---|---|---|---|
| control: one global cap | 76.26% of 38,233 | 23.8% of 760,000 | 100.0% |
| fair, reserve 100 | 0.00% | 17.9% | 91.7% |
| fair, reserve 250 | 0.00% | 14.2% | 76.7% |
| fair, reserve 500 | 0.00% | 7.9% | 51.9% |

Without admission the hot tenant takes the node and three quarters of the small tenants' requests are refused. With it none is, at the price of idle capacity: the reserve is capacity held back for tenants that may arrive, so utilisation falls with it (91.7% at a reserve of 10%, about the small tenants' demand of 20% plus slack). **The reserve is a setting, not a constant**: it must cover the small tenants' concurrent demand, and a node with no such tenants wastes it. Admission does not preempt; a tenant owed its share waits for a running request to finish (`Saturated`).

**Cold cache storm** (1 MiB cache; one tenant reads never-seen 1 KiB objects as often as 50 small tenants re-read their 8 objects, a working set of 400 KiB):

| cache | small tenants' hit ratio | small objects evicted |
|---|---|---|
| control: one shared LRU | 82.5% | 10,395 |
| quota: floor 16 KiB, ceiling 512 KiB | 100.0% | 0 |

**Placement skew and AZ imbalance** (100,000 tenants; busiest node over the mean node, and the busiest zone's share of replicas):

| nodes per zone | replicas | busiest node / mean | busiest zone |
|---|---|---|---|
| 20, 20, 20 | 1 | 1.04 | 33.5% |
| 20, 20, 20 | 3 | 1.02 | 33.3% |
| 20, 20, 10 | 1 | 1.04 | 40.1% |
| 20, 20, 10 | 3 | 1.69 | 33.3% |
| 30, 15, 5 | 3 | 3.35 | 33.3% |

Single replicas follow node weight across unequal zones (the 10-node zone holds 40.1% only because the largest zone holds the most). **Spreading three replicas over three zones is a cost when the zones are unequal**: each zone holds a third of the replicas, so the nodes of the small zone carry more (1.69 times the mean at 20/20/10, 3.35 at 30/15/5). Zone spread buys survival of a zone loss with at most one replica gone; the imbalance is the price, and a deployment with unequal zones should either size zones equally or place fewer replicas than zones.

**Cost of a placement** (release build, one thread): 1,534 ns at 100 nodes, 12.1 µs at 1,000 and 120 µs at 10,000 (one replica; three replicas 1.9, 15.5 and 144 µs): about 12 ns a node, one pass, no allocation beyond the result. A first version computed the logarithm by 16 squarings and cost 80 ns a node (846 µs at 10,000 nodes); the table is 7 times faster with a score error under 0.02%. A placement is made when a tenant becomes active and is then held by the activation set, so it is not on a hit.

## Limits and downsides

- **Nothing here runs on a node yet.** These are mechanisms with the model's evidence; wiring them to the request path is M07 (distributed admission is M07.08) and the cache's own tenant-scoped LRU is M04.05. "Cold cache storm" is shown against an accounting-level cache model, not the engine's cache.
- **Quota victim selection is sampled**: a tenant below its floor that needs room, when the sample of 32 slots holds no surplus, is served uncached even if a heavy tenant exists elsewhere in the table; the round-robin hand reaches it on later reservations. Safe (nothing under a floor is evicted) and degraded (a bypass).
- **A victim gives up its whole surplus before the next is touched** (largest first), harsher on the largest tenant than proportional shrinking.
- **Admission has no preemption and no queue**: a refusal is a signal; the caller queues or backs off. Weights are the caller's input; the weight a tenant has while it has work in flight is the last one given.
- **Placement is O(nodes)**: 0.12 ms at 10,000 nodes, made once per activation. A fleet of hundreds of thousands of nodes would want a prefilter by zone or rack; not needed at the 10,000-node target.
- **Rendezvous moves 1/N of tenants when a node leaves**, which is the minimum; consumers must treat a placement change as a cold start of that tenant on the new node (it reads the head and the objects again).
- **Real AZ imbalance (latency, inter-AZ traffic cost) is not measured**: only the share of replicas by zone. Per-tenant SLO latency interference needs the engine; what is measured is refused work, which is the part admission controls.
