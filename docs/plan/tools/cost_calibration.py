#!/usr/bin/env python3
"""M02.07 cost calibration: priced request/byte envelopes per profile and workload.

Extends cost_research (batching traces) and cost_model (rate-card arithmetic) with a versioned
rate-card schema, sparse/busy/skewed/recovery workloads and measured inputs. It prices counted
requests and directed bytes only. It does not predict latency, availability or total cost of
ownership, and the rate card in the inputs file is an illustrative example, not a quote.
"""
from __future__ import annotations

import argparse
import json
import math
import random
import re
import sys
from decimal import Decimal
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cost_model  # noqa: E402
import cost_research  # noqa: E402

PLAN = Path(__file__).resolve().parents[1]
DEFAULT = PLAN / "cost-calibration-inputs.json"
KINDS = ("put", "get", "head", "list", "kms")
DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
STATUSES = {"illustrative_inputs_not_a_deployable_price_card", "operator_quote"}


def check_rate_card(card):
    """A price is data with provenance: value, unit, source URL and date, or it is rejected."""
    for key in ("provider", "region", "as_of", "status", "period_seconds"):
        if key not in card:
            raise ValueError(f"rate_card: missing {key}")
    if not DATE.match(card["as_of"]):
        raise ValueError("rate_card: as_of must be YYYY-MM-DD")
    if card["status"] not in STATUSES:
        raise ValueError(f"rate_card: status must be one of {sorted(STATUSES)}")
    for kind, entry in card.get("requests", {}).items():
        label = f"request {kind}"
        cost_model.number(entry.get("usd_per_1000"), label)
        for field in ("unit", "source", "as_of"):
            if not entry.get(field):
                raise ValueError(f"{label}: missing {field}")
        if not entry["source"].startswith("https://") or not DATE.match(entry["as_of"]):
            raise ValueError(f"{label}: source must be an https URL and as_of a date")
    for edge, entry in card.get("traffic", {}).items():
        label = f"traffic {edge}"
        for field in ("sender_usd_per_billing_gb", "receiver_usd_per_billing_gb", "billing_gb_bytes"):
            cost_model.number(entry.get(field), f"{label} {field}")
        for field in ("unit", "source", "as_of"):
            if not entry.get(field):
                raise ValueError(f"{label}: missing {field}")
    return card


def legacy_card(card):
    return {"period_seconds": card["period_seconds"],
            "request_usd_per_1000": {k: v["usd_per_1000"] for k, v in card["requests"].items()}}


def workload_trace(name, spec, seed):
    """Seeded arrivals built from the workload's own parameters: streams, rate, record bytes,
    optional hot share (fraction of arrivals on tenant 0's 50 streams) and optional on/off bursts
    (burst_on_s of arrivals at burst_factor x rate, then burst_off_s idle, same mean rate)."""
    rng = random.Random(seed)
    streams, rate, size = spec["streams"], spec["records_per_s"], spec["record_bytes"]
    hot = spec.get("hot_share", 0.0)
    on, off = spec.get("burst_on_s"), spec.get("burst_off_s")
    factor = (on + off) / on if on else 1.0
    t, events = 0.0, []
    while True:
        if on:
            t += rng.expovariate(rate * factor)
            if t % (on + off) >= on:
                t += (on + off) - (t % (on + off))  # skip the idle gap to the next burst
        else:
            t += rng.expovariate(rate)
        if t >= spec["seconds"]:
            break
        stream = rng.randrange(50) if hot and rng.random() < hot else rng.randrange(streams)
        events.append((t, stream, size))
    return events


def batch_rates(events, wait, seconds):
    result = cost_research.batch(events, "tenant-key", wait)
    return result, result["batches"] / seconds


def profile_counts(events, seconds, profile, ckpt):
    """Per-second counts and cross-AZ bytes for one trace under one profile.

    Object profiles commit one PUT per batch (fenced append). Journal profiles commit to the
    quorum journal, so only the asynchronous archive batches (and their checkpoints) hit the
    object store; journal volumes, compute and network framing are NOT priced. Every commit
    batch still pays one KMS wrap. Checkpoint PUTs are amortized (puts/cadence per object write),
    not truncated per 30 s trace."""
    main, batches_s = batch_rates(events, profile["max_wait_s"], seconds)
    counts = {k: 0.0 for k in KINDS}
    counts["kms"] = batches_s
    if profile["commit_to_object"]:
        object_writes = batches_s
    else:
        archive, object_writes = batch_rates(events, profile["archive_wait_s"], seconds)
    if profile["commit_to_object"] and profile["archive_wait_s"]:
        archive, archive_s = batch_rates(events, profile["archive_wait_s"], seconds)
        object_writes += archive_s
    counts["put"] = object_writes * (1 + ckpt["puts"] / ckpt["every_batches"])
    cross = sum(e[2] for e in events) / seconds * profile["cross_az_copies"]
    return counts, cross, main


def steady_state(name, spec, profile, seeds, ckpt):
    """Mean per-second counts, cross-AZ bytes, p95 batch wait and dirty domains over seeds."""
    acc = {k: 0.0 for k in KINDS}
    cross = wait = dirty = 0.0
    for seed in seeds:
        events = workload_trace(name, spec, seed)
        counts, c, main = profile_counts(events, spec["seconds"], profile, ckpt)
        for k in KINDS:
            acc[k] += counts[k]
        cross += c
        wait += main["batch_wait_p95_ms"]
        dirty += main["activated_domains"]
    n = len(seeds)
    return {k: v / n for k, v in acc.items()}, cross / n, wait / n, dirty / n


def price(card, counts, cross_az_bytes_s):
    scenario = {"attempts_per_second": {k: v for k, v in counts.items() if v}}
    if cross_az_bytes_s:
        edge = card["traffic"]["cross_az"]
        scenario["traffic"] = [{"id": "cross_az", "billing_gb_bytes": edge["billing_gb_bytes"],
                                "bytes_per_second": cross_az_bytes_s,
                                "sender_usd_per_billing_gb": edge["sender_usd_per_billing_gb"],
                                "receiver_usd_per_billing_gb": edge["receiver_usd_per_billing_gb"]}]
    components, total = cost_model.estimate(legacy_card(card), scenario)
    return {k: str(v.quantize(Decimal("0.01"))) for k, v in components.items()}, total


def recovery(spec, card, batch_bytes=None):
    """One cell loss: reactivation PUTs (fence + checkpoint per domain), checkpoint and tail
    GETs, the bounded cold LIST per domain and a KMS unwrap per domain. `batch_bytes` defaults to
    the workload's cap (an upper bound); pass a typical batch size for an expected tail."""
    d = spec["active_domains"]
    counts = {"get": d * (spec["checkpoint_gets_per_domain"] + spec["tail_batches_per_domain"]),
              "list": d * spec["cold_list_per_domain"], "head": 0,
              "put": d * spec["reactivation_puts_per_domain"], "kms": d}
    usd = sum(Decimal(card["requests"][k]["usd_per_1000"]) * Decimal(v) / 1000 for k, v in counts.items() if v)
    tail_bytes = d * spec["tail_batches_per_domain"] * (batch_bytes or spec["batch_bytes"])
    return counts, usd, tail_bytes


def tail_read_seconds(tail_bytes, mib_s):
    """Time to read the tail at an ASSUMED single-process rate. No GET throughput was measured;
    the only measured rate is for PUTs, so this is a sensitivity, not a floor or a bound."""
    return tail_bytes / (mib_s * 2**20)


def poisson_check(rate_per_domain, wait):
    """Expected batches/s per domain for Poisson arrivals: one batch per (wait + 1/rate) cycle."""
    return rate_per_domain / (1 + rate_per_domain * wait)


def idle_cost(card, data):
    """An empty trace through every profile must price to zero."""
    total = Decimal(0)
    for profile in data["profiles"].values():
        counts, cross, _ = profile_counts([], 30, profile, data["checkpoint"])
        total += price(card, counts, cross)[1]
    return total


def run(data):
    card = check_rate_card(data["rate_card"])
    measured = data["measured"]
    ckpt = data["checkpoint"]
    rows = []
    for wl in ("sparse", "busy", "skewed"):
        spec = data["workloads"][wl]
        for pname, profile in data["profiles"].items():
            counts, cross, wait_ms, dirty = steady_state(wl, spec, profile, data["seeds"], ckpt)
            components, total = price(card, counts, cross)
            clients = math.ceil(counts["put"] / measured["small_put_saturation_per_s"])
            row = {"workload": wl, "profile": pname, "requests_per_s": {k: round(v, 2) for k, v in counts.items() if v},
                   "dirty_domains_per_30s_window": round(dirty), "cross_az_bytes_per_s": round(cross),
                   "freshness_floor_p95_batch_wait_ms": round(wait_ms),
                   "min_write_processes_at_measured_saturation": max(clients, 1),
                   "usd_per_30_days_components": components, "usd_per_30_days_subtotal": str(total.quantize(Decimal("0.01")))}
            if wl == "skewed":
                hot = measured["hot_key_per_commit"]
                row["hot_key_sensitivity"] = ("if the hot domain used payload+head CAS with competing writers instead of one ordered lane: "
                    f"x{hot['put']} PUT, +{hot['head']} HEAD and +{hot['list']} LIST per commit on that domain's commits (measured, M02.10)")
            rows.append(row)
    rspec = data["workloads"]["recovery"]
    busy_events = workload_trace("busy", data["workloads"]["busy"], data["seeds"][0])
    typical = cost_research.batch(busy_events, "tenant-key", data["profiles"]["object-resilient"]["max_wait_s"])
    typical_bytes = typical["records"] * data["workloads"]["busy"]["record_bytes"] / typical["batches"]
    rec = {}
    for label, bb in (("cap_upper_bound", None), ("typical_busy_batch", typical_bytes)):
        counts, usd, tail = recovery(rspec, card, bb)
        rec[label] = {"requests": counts, "usd": str(usd.quantize(Decimal("0.01"))), "tail_bytes": round(tail),
                      "tail_read_s_at_assumed_rate": {f"{r} MiB/s": round(tail_read_seconds(tail, r)) for r in rspec["assumed_read_mib_s"]}}
    checks = {"idle_usd": str(idle_cost(card, data)), "poisson_vs_simulation": []}
    for wl, wait in (("sparse", 0.25), ("busy", 0.25), ("busy", 1.0), ("bursty", 0.25), ("bursty", 1.0)):
        spec = data["workloads"][wl]
        sims = [cost_research.batch(workload_trace(wl, spec, sd), "tenant-key", wait)["batches"] / spec["seconds"] for sd in data["seeds"]]
        sim = sum(sims) / len(sims)
        domains = (spec["streams"] // 50) * 2
        model = domains * poisson_check(spec["records_per_s"] / domains, wait)
        checks["poisson_vs_simulation"].append({"workload": wl, "wait_s": wait, "simulated_batches_per_s": round(sim, 2),
            "poisson_model_batches_per_s": round(model, 2), "relative_difference": round(abs(sim - model) / model, 3)})
    return {"rows": rows, "recovery_event": rec, "checks": checks,
            "rate_card": {k: card[k] for k in ("provider", "region", "as_of", "status")}}


def report(result):
    lines = [f"rate card: {result['rate_card']['provider']} {result['rate_card']['region']} as of "
             f"{result['rate_card']['as_of']}, status {result['rate_card']['status']}"
             + (" (NOT A QUOTE)" if result["rate_card"]["status"] != "operator_quote" else ""), "",
             "| workload | profile | dirty domains / 30 s | PUT/s | KMS/s | cross-AZ MiB/s | p95 batch wait ms | min write processes | USD per 30 days |",
             "|---|---|---|---|---|---|---|---|---|"]
    for r in result["rows"]:
        q = r["requests_per_s"]
        lines.append(f"| {r['workload']} | {r['profile']} | {r['dirty_domains_per_30s_window']} | {q.get('put', 0)} | {q.get('kms', 0)} | "
                     f"{r['cross_az_bytes_per_s'] / 2**20:.2f} | {r['freshness_floor_p95_batch_wait_ms']} | "
                     f"{r['min_write_processes_at_measured_saturation']} | {Decimal(r['usd_per_30_days_subtotal']):,.2f} |")
    return "\n".join(lines)


def selftest():
    data = json.loads(DEFAULT.read_text(encoding="utf-8"))
    card = check_rate_card(data["rate_card"])
    assert idle_cost(card, data) == 0
    bad = [lambda c: c["requests"]["put"].pop("source"), lambda c: c["requests"]["put"].update(as_of="yesterday"),
           lambda c: c["requests"]["put"].update(usd_per_1000="-1"), lambda c: c.pop("provider"),
           lambda c: c["traffic"]["cross_az"].pop("unit")]
    for mutate in bad:
        broken = json.loads(json.dumps(data["rate_card"]))
        mutate(broken)
        try:
            check_rate_card(broken)
        except ValueError:
            continue
        raise AssertionError("an unprovenanced or invalid rate card passed")
    try:
        price(card, {"unpriced": 1.0}, 0)
    except ValueError:
        pass
    else:
        raise AssertionError("an unpriced path was accepted")
    print("PASS: rate-card provenance, unpriced-path and zero-idle checks")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, default=DEFAULT)
    parser.add_argument("--selftest", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    if args.selftest:
        selftest()
        return
    result = run(json.loads(args.input.read_text(encoding="utf-8")))
    print(json.dumps(result, indent=2) if args.json else report(result))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError) as error:
        raise SystemExit(f"FAIL: {error}") from error
