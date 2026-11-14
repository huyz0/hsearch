#!/usr/bin/env python3
"""M03.09: commit-lane sharing options at the PR-01 scale, plus a key-revocation model.

Cost part: a modelled comparison (NOT a measurement) of one lane per index, lanes shared by
indexes with the same key/residency/durability policy, and shared lanes with per-index payload
encryption, using the M02.07 batcher and rate card. Revocation part: an exhaustive search of small
histories that finds the sharing rule under which revoking one index's key blocks its lane-mates.
"""
from __future__ import annotations

import argparse
import itertools
import json
import random
import sys
from collections import Counter
from decimal import Decimal
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cost_calibration as cc  # noqa: E402
import cost_research  # noqa: E402

OPTIONS = ("A one lane per index", "B shared by same policy, BYOK own lane", "C shared, per-index payload encryption")


def is_byok(index, fraction):
    return (index * 2654435761) % 10000 < fraction * 10000


def trace(spec, indexes_active, seed):
    rng = random.Random(seed)
    hot = max(1, int(indexes_active * spec["hot_fraction_of_indexes"]))
    t, events = 0.0, []
    while True:
        t += rng.expovariate(spec["records_per_s"])
        if t >= spec["seconds"]:
            return events
        pool = hot if rng.random() < spec["hot_share"] else indexes_active
        index = rng.randrange(pool)
        if pool == hot:
            index += spec.get("hot_offset", 0)  # a hot index that is not the BYOK-flagged index 0
        events.append((t, index, spec["record_bytes"]))


def lane_of(option, index, cfg):
    k = cfg["lane_group_size"]
    if option == OPTIONS[0]:
        return ("idx", index)
    if option == OPTIONS[1] and is_byok(index, cfg["byok_fraction"]):
        return ("idx", index)  # BYOK cannot share a lane unless payload is encrypted per index
    return ("grp", index // k)


def run_option(option, events, cfg, wait, seconds, ckpt):
    """Requests per second and lane statistics for one option on one trace."""
    domain_ids, mapped = {}, []
    shares = {}
    for t, index, size in events:
        lane = lane_of(option, index, cfg)
        domain_ids.setdefault(lane, len(domain_ids))
        mapped.append((t, domain_ids[lane] * 50, size))  # (stream//50, stream%2) = (lane, 0)
        shares.setdefault(lane, Counter())[index] += 1
    result = cost_research.batch(mapped, "tenant-key", wait)
    batches_s = result["batches"] / seconds
    puts = batches_s * (1 + ckpt["puts"] / ckpt["every_batches"])
    # One wrap per batch for the lane key; per-index encryption adds one wrap per distinct BYOK
    # index key in the batch (default-key indexes share one wrap).
    wraps = batches_s
    if option == OPTIONS[2]:
        byok_records = sum(1 for _, i, _ in events if is_byok(i, cfg["byok_fraction"]))
        wraps += byok_records / seconds  # upper bound: every BYOK record carries its own wrap
    worst = hottest_share(shares, cfg.get("min_records_for_share", 1000))
    return {"batches_per_s": batches_s, "put_per_s": puts, "kms_per_s": wraps,
            "lanes_active": len(domain_ids), "writes_per_s": len(events) / seconds,
            "max_index_share_of_busy_lane": worst,
            "promoted_indexes": len(promotion_candidates(shares, cfg))}


def hottest_share(shares, min_records):
    """Largest single-index share of any lane that saw at least `min_records` records.

    A lane with a handful of records says nothing (two hits on one index of twenty reads as 10%),
    so only lanes with enough volume count.
    """
    return max((max(c.values()) / sum(c.values()) for lane, c in shares.items()
                if lane[0] == "grp" and sum(c.values()) >= min_records), default=0.0)


def promotion_candidates(shares, cfg):
    """Indexes the promotion rule would move to their own lane over this window.

    The rule is a share AND a floor: one index above `promote_share` of its lane's records, in a lane
    with at least `promote_floor_records` records in the window. Without the floor a lane in which a
    single index wrote once would show a 100% share and be promoted for nothing.
    """
    out = []
    for lane, c in shares.items():
        total = sum(c.values())
        if total < cfg["promote_floor_records"] or lane[0] == "idx":
            continue
        out.extend((lane, i) for i, n in c.items() if n / total > cfg["promote_share"])
    return out


def lanes_total(option, cfg):
    n, k, f = cfg["indexes"], cfg["lane_group_size"], cfg["byok_fraction"]
    if option == OPTIONS[0]:
        return n
    if option == OPTIONS[1]:
        return int(n * f) + int(n * (1 - f)) // k + 1
    return n // k + 1


def blast_radius(option, cfg):
    return 1 if option == OPTIONS[0] else cfg["lane_group_size"]


def byok_sensitivity(data, fractions=(0.05, 0.25, 0.5, 1.0)):
    """Requests per acknowledged write for options B and C as the BYOK share grows (steady_small)."""
    cfg = dict(data["lane_sharing"])
    active = int(cfg["indexes"] * cfg["active_fraction"])
    spec = cfg["workloads"]["steady_small"]
    profile = data["profiles"][cfg["profile"]]
    events = trace(spec, active, data["seeds"][0])
    out = []
    for f in fractions:
        c = dict(cfg, byok_fraction=f)
        row = {"byok_fraction": f}
        for option in OPTIONS[1:]:
            r = run_option(option, events, c, profile["max_wait_s"], spec["seconds"], data["checkpoint"])
            row[option[0]] = round((r["put_per_s"] + r["kms_per_s"]) / r["writes_per_s"], 3)
            row[option[0] + "_lanes_at_scale"] = lanes_total(option, c)
        out.append(row)
    return out


def compare(data):
    cfg = data["lane_sharing"]
    card = cc.check_rate_card(data["rate_card"])
    profile = data["profiles"][cfg["profile"]]
    active = int(cfg["indexes"] * cfg["active_fraction"])
    rows = []
    for wname, spec in cfg["workloads"].items():
        events = trace(spec, active, data["seeds"][0])
        for option in OPTIONS:
            r = run_option(option, events, cfg, profile["max_wait_s"], spec["seconds"], data["checkpoint"])
            components, total = cc.price(card, {"put": r["put_per_s"], "kms": r["kms_per_s"]}, 0)
            lanes = lanes_total(option, cfg)
            rows.append({"workload": wname, "option": option,
                         "requests_per_ack": round((r["put_per_s"] + r["kms_per_s"]) / r["writes_per_s"], 3),
                         "put_per_ack": round(r["put_per_s"] / r["writes_per_s"], 3),
                         "kms_per_ack": round(r["kms_per_s"] / r["writes_per_s"], 3),
                         "active_lanes": r["lanes_active"], "lanes_total_at_scale": lanes,
                         "retained_objects_total": lanes * cfg["retained_objects_per_lane"],
                         "blast_radius_indexes": blast_radius(option, cfg),
                         "max_index_share_of_busy_lane": round(r["max_index_share_of_busy_lane"], 3),
                         "promoted_indexes": r["promoted_indexes"],
                         "usd_per_30_days": str(total.quantize(Decimal("0.01")))})
    return {"rows": rows, "idle_requests_per_s": 0,
            "idle_note": "dirty-only batching and no per-index timers: a 90% idle index issues no request in any option (checked by the empty-trace test); idle objects retained per lane are listed, and they are what option A multiplies"}


# ---- key revocation model -------------------------------------------------------------------
def revocation_search(sharing, length=5, recovery="skip"):
    """Exhaustively explore histories on one lane shared by indexes a and b.

    `sharing` is how the lane's payload is protected, as the key whose revocation makes a record
    unreadable (`protect`) against the key that is the index's own (`own`):
      per_index_dek:    a's records under key_a, b's under key_b (separately encrypted per index).
      lane_dek_default: both indexes use the one default key, so neither has a key of its own to
                        revoke separately; revoking the default key is deployment-wide by definition.
      lane_dek_a_byok:  BROKEN CONTROL: one lane DEK wrapped by a's BYOK key while b shares the lane.
    `recovery` is what lane recovery does on a block whose key is revoked: "skip" it, or "fail" the
    whole lane. Invariant checked after every history ending in recovery: the lane recovers, and a
    record is readable exactly when its own key is not revoked.

    Limits: this is a stand-in for the M03.04 authority model, not a proof of the cryptography. For the
    per-index and default-key sharings `protect` equals `own` by construction, so those two cases check
    the recovery policy and the rule that no revocable key wraps a shared lane; they do not check AAD
    or replay across indexes, key rotation, or a revoked-then-reinstated key (M03.04 and M05).
    """
    own = {"per_index_dek": {"a": "key_a", "b": "key_b"},
           "lane_dek_default": {"a": "default", "b": "default"},
           "lane_dek_a_byok": {"a": "key_a", "b": "key_b"}}[sharing]
    protect = {"per_index_dek": own,
               "lane_dek_default": own,
               "lane_dek_a_byok": {"a": "key_a", "b": "key_a"}}[sharing]
    revoke_ops = {"per_index_dek": ("revoke_key_a", "revoke_key_b"),
                  "lane_dek_default": ("revoke_default",),
                  "lane_dek_a_byok": ("revoke_key_a", "revoke_key_b")}[sharing]
    ops = ("append_a", "append_b", "recover") + revoke_ops
    checked = 0
    for history in itertools.product(ops, repeat=length):
        if history[-1] != "recover":
            continue
        acked, revoked = [], set()
        for op in history:
            if op.startswith("append_"):
                acked.append(op[-1])
            elif op.startswith("revoke_"):
                revoked.add(op[len("revoke_"):])
        checked += 1
        if recovery == "fail" and any(protect[r] in revoked for r in acked):
            return {"safe": False, "histories": checked, "counterexample": list(history), "record": None,
                    "why": "recovery failed the whole lane on a block whose key is revoked"}
        for rec in acked:
            readable = protect[rec] not in revoked
            should = own[rec] not in revoked
            if readable != should:
                return {"safe": False, "histories": checked, "counterexample": list(history), "record": rec,
                        "why": f"record of index {rec} readable={readable} but its own key revoked={not should}"}
    return {"safe": True, "histories": checked}


def report(result):
    lines = ["| workload | option | requests per ack (PUT / KMS) | active lanes in window | lanes at 10M | retained objects (idle) | blast radius (indexes) | hottest index share of a busy lane | USD per 30 days |",
             "|---|---|---|---|---|---|---|---|---|"]
    for r in result["rows"]:
        lines.append(f"| {r['workload']} | {r['option']} | {r['requests_per_ack']} ({r['put_per_ack']} / {r['kms_per_ack']}) | {r['active_lanes']:,} | "
                     f"{r['lanes_total_at_scale']:,} | {r['retained_objects_total']:,} | {r['blast_radius_indexes']} | "
                     f"{r['max_index_share_of_busy_lane']} | {Decimal(r['usd_per_30_days']):,.2f} |")
    return "\n".join(lines)


def sensitivity_report(rows):
    lines = ["| BYOK share | B | C | lanes at 10M, B | lanes at 10M, C |", "|---|---|---|---|---|"]
    for r in rows:
        lines.append(f"| {int(r['byok_fraction'] * 100)}% | {r['B']} | {r['C']} | {r['B_lanes_at_scale']:,} | {r['C_lanes_at_scale']:,} |")
    return "\n".join(lines)


def revocation_report(rev):
    lines = []
    for name, r in rev.items():
        if r["safe"]:
            lines.append(f"- `{name}`: safe over {r['histories']} histories.")
        else:
            lines.append(f"- `{name}`: unsafe, first counterexample after {r['histories']} histories: {r['counterexample']} ({r['why']}).")
    return "\n".join(lines)


def selftest():
    assert revocation_search("per_index_dek")["safe"]
    assert revocation_search("lane_dek_default")["safe"]
    assert not revocation_search("lane_dek_a_byok")["safe"]
    assert not revocation_search("per_index_dek", recovery="fail")["safe"]
    print("PASS: revocation model (two safe sharing rules, one deliberately broken control)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, default=cc.DEFAULT)
    parser.add_argument("--selftest", action="store_true")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--markdown", action="store_true", help="all three generated sections, as the evidence file embeds them")
    args = parser.parse_args()
    if args.selftest:
        selftest()
        return
    data = json.loads(args.input.read_text(encoding="utf-8"))
    result = compare(data)
    result["byok_sensitivity"] = byok_sensitivity(data)
    result["revocation"] = {s: revocation_search(s) for s in ("per_index_dek", "lane_dek_default", "lane_dek_a_byok")}
    result["revocation"]["per_index_dek_recovery_fails_lane"] = revocation_search("per_index_dek", recovery="fail")
    if args.markdown:
        print(report(result), "\n\nBYOK sensitivity (requests per write, steady_small):\n\n", sensitivity_report(result["byok_sensitivity"]),
              "\n\nRevocation search (histories of 5 operations ending in recovery):\n\n", revocation_report(result["revocation"]), sep="")
    else:
        print(json.dumps(result, indent=2) if args.json else report(result))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError) as error:
        raise SystemExit(f"FAIL: {error}") from error
