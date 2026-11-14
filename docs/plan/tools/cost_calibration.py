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


def check_rate_card(card):
    """A price is data with provenance: value, unit, source URL and date, or it is rejected."""
    for key in ("provider", "region", "as_of", "status", "period_seconds"):
        if key not in card:
            raise ValueError(f"rate_card: missing {key}")
    if not DATE.match(card["as_of"]):
        raise ValueError("rate_card: as_of must be YYYY-MM-DD")
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
    kind = "sparse" if name == "sparse" else ("skewed" if name == "skewed" else "busy")
    return cost_research.trace(kind, seed, spec["seconds"])


def batch_rates(events, wait, seconds):
    result = cost_research.batch(events, "tenant-key", wait)
    return result, result["batches"] / seconds


def steady_state(name, spec, profile, seeds):
    """Mean per-second counts for one streaming workload under one profile."""
    seconds = spec["seconds"]
    totals = {k: 0.0 for k in KINDS}
    bytes_s = 0.0
    waits = []
    for seed in seeds:
        events = workload_trace(name, spec, seed)
        main, batches_s = batch_rates(events, profile["max_wait_s"], seconds)
        puts = batches_s + main["checkpoint_puts"] / seconds  # fenced append plus amortized checkpoints
        kms = batches_s
        if profile["archive_wait_s"]:
            archive, archive_s = batch_rates(events, profile["archive_wait_s"], seconds)
            puts += archive_s  # one asynchronous regional archive object per archive window
        totals["put"] += puts
        totals["kms"] += kms
        bytes_s += sum(e[2] for e in events) / seconds
        waits.append(main["batch_wait_p95_ms"])
    n = len(seeds)
    counts = {k: v / n for k, v in totals.items()}
    return counts, bytes_s / n * profile["cross_az_copies"], sum(waits) / n


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


def recovery(spec, card):
    """Cost of one cell loss, per event: checkpoint GETs, tail GETs and the bounded cold LIST."""
    d = spec["active_domains"]
    counts = {"get": d * (spec["checkpoint_gets_per_domain"] + spec["tail_batches_per_domain"]),
              "list": d * spec["cold_list_per_domain"], "head": 0, "put": 0, "kms": d}
    usd = sum(Decimal(card["requests"][k]["usd_per_1000"]) * Decimal(v) / 1000 for k, v in counts.items() if v)
    tail_bytes = d * spec["tail_batches_per_domain"] * spec["batch_bytes"]
    return counts, usd, tail_bytes


def recovery_floor_seconds(tail_bytes, measured):
    """Lower bound on tail-read time from the measured large-payload rate on one process."""
    return tail_bytes / (measured["large_payload_mib_s_lower_bound"] * 2**20)


def poisson_check(rate_per_domain, wait):
    """Expected batches/s per domain for Poisson arrivals: one batch per (wait + 1/rate) cycle."""
    return rate_per_domain / (1 + rate_per_domain * wait)


def zero_idle(card):
    counts, cross = {k: 0.0 for k in KINDS}, 0.0
    return price(card, counts, cross)[1]


def run(data):
    card = check_rate_card(data["rate_card"])
    measured = data["measured"]
    rows = []
    for wl in ("sparse", "busy", "skewed"):
        spec = data["workloads"][wl]
        for pname, profile in data["profiles"].items():
            counts, cross, wait_ms = steady_state(wl, spec, profile, data["seeds"])
            components, total = price(card, counts, cross)
            clients = math.ceil(counts["put"] / measured["small_put_saturation_per_s"])
            row = {"workload": wl, "profile": pname, "requests_per_s": {k: round(v, 2) for k, v in counts.items() if v},
                   "cross_az_bytes_per_s": round(cross), "freshness_floor_p95_batch_wait_ms": round(wait_ms),
                   "min_write_processes_at_measured_saturation": max(clients, 1),
                   "usd_per_30_days_components": components, "usd_per_30_days_subtotal": str(total.quantize(Decimal("0.01")))}
            if wl == "skewed":
                hot = measured["hot_key_per_commit"]
                row["hot_key_sensitivity"] = ("if the hot domain used payload+head CAS with competing writers instead of one ordered lane: "
                    f"x{hot['put']} PUT, +{hot['head']} HEAD and +{hot['list']} LIST per commit on that domain's commits (measured, M02.10)")
            rows.append(row)
    rec_counts, rec_usd, rec_bytes = recovery(data["workloads"]["recovery"], card)
    probes = {
        "idle_usd": str(zero_idle(card)),
        "poisson_vs_simulation": [],
    }
    for wl, wait in (("sparse", 0.25), ("busy", 0.25), ("busy", 1.0)):
        spec = data["workloads"][wl]
        events = workload_trace(wl, spec, data["seeds"][0])
        sim = cost_research.batch(events, "tenant-key", wait)["batches"] / spec["seconds"]
        domains = (spec["streams"] // 50) * 2
        model = domains * poisson_check(spec["records_per_s"] / domains, wait)
        probes["poisson_vs_simulation"].append({"workload": wl, "wait_s": wait, "simulated_batches_per_s": round(sim, 2),
                                                "poisson_model_batches_per_s": round(model, 2),
                                                "relative_difference": round(abs(sim - model) / model, 3)})
    return {"rows": rows, "recovery_event": {"requests": rec_counts, "usd": str(rec_usd.quantize(Decimal("0.01"))),
            "tail_bytes_read": rec_bytes,
            "single_process_read_floor_s": round(recovery_floor_seconds(rec_bytes, measured)),
            "note": "upper-bound tail (every domain at its replay bound with full 4 MiB batches); one process; a smaller tail bound or parallel readers shortens it"}, "checks": probes, "rate_card": {k: card[k] for k in ("provider", "region", "as_of", "status")}}


def report(result):
    lines = ["| workload | profile | PUT/s | KMS/s | cross-AZ MiB/s | p95 batch wait ms | min write processes | USD per 30 days |",
             "|---|---|---|---|---|---|---|---|"]
    for r in result["rows"]:
        q = r["requests_per_s"]
        lines.append(f"| {r['workload']} | {r['profile']} | {q.get('put', 0)} | {q.get('kms', 0)} | "
                     f"{r['cross_az_bytes_per_s'] / 2**20:.2f} | {r['freshness_floor_p95_batch_wait_ms']} | "
                     f"{r['min_write_processes_at_measured_saturation']} | {Decimal(r['usd_per_30_days_subtotal']):,.2f} |")
    return "\n".join(lines)


def selftest():
    data = json.loads(DEFAULT.read_text(encoding="utf-8"))
    card = check_rate_card(data["rate_card"])
    assert zero_idle(card) == 0
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
