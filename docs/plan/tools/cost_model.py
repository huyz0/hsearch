#!/usr/bin/env python3
"""Count-based cost sensitivity model. It does not predict latency or full TCO."""
from __future__ import annotations

import argparse
import json
from decimal import Decimal
from pathlib import Path

DEFAULT = Path(__file__).resolve().parents[1] / "cost-scenarios.json"


def number(value, label):
    if isinstance(value, bool):
        raise ValueError(f"{label}: boolean is not a quantity")
    try:
        result = Decimal(str(value))
    except Exception as exc:
        raise ValueError(f"{label}: invalid number") from exc
    if not result.is_finite() or result < 0:
        raise ValueError(f"{label}: needs a finite nonnegative quantity")
    return result


def estimate(card, scenario):
    seconds = number(card["period_seconds"], "period_seconds")
    if not seconds:
        raise ValueError("period_seconds must be positive")
    components = {}
    for kind, attempts_s in scenario.get("attempts_per_second", {}).items():
        if kind not in card["request_usd_per_1000"]:
            raise ValueError(f"Missing tariff for {kind}")
        attempts = number(attempts_s, kind) * seconds
        rate = number(card["request_usd_per_1000"][kind], kind)
        components[f"requests:{kind}"] = attempts * rate / 1000
    seen = set()
    for edge in scenario.get("traffic", []):
        label = edge["id"]
        if not label or label in seen:
            raise ValueError("Traffic edges require unique nonempty IDs")
        seen.add(label)
        unit = number(edge["billing_gb_bytes"], "billing_gb_bytes")
        if not unit:
            raise ValueError("billing_gb_bytes must be positive")
        volume = number(edge["bytes_per_second"], label) * seconds / unit
        # One directed payload; the two endpoint rates are summed once.
        rate = number(edge["sender_usd_per_billing_gb"], label) + number(edge["receiver_usd_per_billing_gb"], label)
        components[f"traffic:{label}"] = volume * rate
    for label, value in scenario.get("other_component_usd", {}).items():
        components[f"other:{label}"] = number(value, label)
    return components, sum(components.values(), Decimal(0))


def break_even_bytes(avoided_gets, get_usd_per_1000, sender, receiver, unit):
    saved = number(avoided_gets, "avoided_gets") * number(get_usd_per_1000, "get") / 1000
    rate = number(sender, "sender") + number(receiver, "receiver")
    unit = number(unit, "unit")
    if not unit:
        raise ValueError("unit must be positive")
    return saved * unit / rate if rate else None


def selftest():
    card = {"period_seconds": 2592000, "request_usd_per_1000": {"put": "0.005", "get": "0.0004"}}
    assert estimate(card, {"attempts_per_second": {"put": 10000}})[1] == Decimal("129600")
    edge = {"id": "one-way", "billing_gb_bytes": 1073741824, "bytes_per_second": 1073741824,
            "sender_usd_per_billing_gb": "0.01", "receiver_usd_per_billing_gb": "0.01"}
    assert estimate(card, {"traffic": [edge]})[1] == Decimal("51840")  # both endpoints, one direction
    assert estimate(card, {"traffic": [edge, dict(edge, id="actual-return")]})[1] == Decimal("103680")
    assert estimate(card, {"attempts_per_second": {"put": 0, "get": 0}})[1] == 0
    assert break_even_bytes(1, "0.0004", "0.01", "0.01", 1073741824) == Decimal("21474.83648")
    assert break_even_bytes(1, "0.0004", 0, 0, 1073741824) is None
    # Packing payloads changes no explicitly counted per-shard root operations.
    packed = estimate(card, {"attempts_per_second": {"put": 10100}})[1]
    assert packed == Decimal("130896")
    assert estimate(card, {"attempts_per_second": {"put": 20200}})[1] == 2 * packed
    bad = [
        ({"attempts_per_second": {"unpriced-op": 1}}, "missing tariff"),
        ({"attempts_per_second": {"put": -1}}, "negative count"),
        ({"attempts_per_second": {"put": "NaN"}}, "nonfinite count"),
        ({"attempts_per_second": {"put": True}}, "boolean count"),
        ({"traffic": [edge, edge]}, "double-counted edge"),
        ({"traffic": [dict(edge, billing_gb_bytes=0)]}, "zero byte unit"),
        ({"traffic": [dict(edge, sender_usd_per_billing_gb=-1)]}, "negative tariff"),
    ]
    for scenario, label in bad:
        try:
            estimate(card, scenario)
        except ValueError:
            continue
        raise AssertionError(f"Negative control unexpectedly passed: {label}")
    print("PASS: 8 arithmetic/boundary checks and 7 invalid-input controls; component model only")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, default=DEFAULT)
    parser.add_argument("--selftest", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    if args.selftest:
        selftest()
        return
    data = json.loads(args.input.read_text(encoding="utf-8"))
    if data["schema_version"] != 1 or data["currency"] != "USD":
        raise ValueError("Unsupported schema or currency")
    rows = []
    for scenario in data["scenarios"]:
        components, total = estimate(data["rate_card"], scenario)
        rows.append({"id": scenario["id"], "component_subtotal_usd": str(total),
                     "components_usd": {k: str(v) for k, v in components.items()}, "assumptions": scenario["assumptions"]})
    if args.json:
        print(json.dumps({"notice": data["notice"], "results": rows}, indent=2))
    else:
        print(data["notice"])
        for row in rows:
            print(f"{row['id']}: USD {Decimal(row['component_subtotal_usd']):,.2f} per modeled 30-day period")
        size = break_even_bytes(1, data["rate_card"]["request_usd_per_1000"]["get"], "0.01", "0.01", 1073741824)
        print(f"Illustrative one-GET vs two-endpoint transfer break-even: {size / 1024:.5f} KiB")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError) as error:
        raise SystemExit(f"FAIL: {error}") from error
