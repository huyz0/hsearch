#!/usr/bin/env python3
"""Fixed source-bound RustFS capability controller, not generic task dispatch.

The normal TaskStore integration supplies charged append/run/input callbacks.
The fixed TaskStore integration retains ordinary claim/resource admission. No CLI
fallback may allocate an alternate claim, pool, Docker client or unchecked run.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import runpy
import time

ROOT = Path(__file__).resolve().parents[1]
HISTORY = runpy.run_path(str(ROOT / "dev/rustfs_probe_history.py"))
PROFILE = HISTORY["PROFILE"]
MODES = ("probe-checker-build", "rustfs-capabilities-before", "rustfs-capabilities-after", "probe-history-check")
LIMITS = {"clients": 16, "operations": 256, "attempts": 512, "atoms": 1024,
          "wire_bytes": 704*1024, "diagnostics": 64*1024, "seconds": 120,
          "transitions": 100000, "working_bytes": 64*1024**2}


class Refused(RuntimeError):
    """Stable controller refusal; original charged evidence remains retained."""


def need(value, code):
    if not value:
        raise Refused(code)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


def restart_receipt(before, after, expected):
    """Check actual inspect results, never Config.Image as actual image proof."""
    fields = {}
    for prefix, info in (("before", before), ("after", after)):
        need(type(info) is dict and re.fullmatch(r"[a-f0-9]{64}", info.get("Id", "")), "restart-cid")
        need(info.get("Image") == expected["image"] and info["Id"] == expected["cid"], "restart-actual-image")
        labels = info.get("Config", {}).get("Labels", {})
        need(labels.get("io.hsearch.owner") == expected["owner"] and
             labels.get("io.hsearch.namespace") == expected["namespace"], "restart-owner")
        mounts = info.get("Mounts", [])
        data = [m for m in mounts if m.get("Destination") == "/data"]
        need(len(data) == 1 and data[0].get("Name") == expected["volume"] and
             data[0].get("Type") == "volume" and data[0].get("RW") is True, "restart-volume")
        state = info.get("State", {})
        need(state.get("Running") is True and type(state.get("Pid")) is int and state["Pid"] > 0
             and type(state.get("StartedAt")) is str and 0 < len(state["StartedAt"]) <= 128, "restart-boot")
        # Config is the actual immutable admission configuration. Store its
        # digest rather than environment strings or credentials in evidence.
        need(digest(canonical(info["Config"])) == expected["config_sha256"], "restart-config")
        fields[prefix+"_boot"] = digest(canonical({"pid": state["Pid"], "started": state["StartedAt"]}))
        fields[prefix+"_cid"] = info["Id"]
        fields[prefix+"_image"] = digest(info["Image"].encode())
        fields[prefix+"_volume"] = digest(data[0]["Name"].encode())
        fields[prefix+"_config"] = expected["config_sha256"]
    need(fields["before_boot"] != fields["after_boot"], "restart-did-not-change-boot")
    return fields


def check_execution(result, *, image, budget, platform):
    need(type(result) is dict and result.get("truncated") is False and
         type(result.get("exit_code")) is int and result["exit_code"] in (0, 1, 2), "unknown-or-truncated-profile-result")
    need(result.get("output_protocol") == "rustfs-raw-streams-v1" and
         result.get("encoding") == "utf-8-strict" and result.get("stderr_bytes") == 0,
         "raw-output-protocol-or-encoding-refused")
    proof = result.get("execution")
    need(type(proof) is dict and proof.get("image_id") == image and
         re.fullmatch(r"[a-f0-9]{64}", proof.get("container_id", "")) and
         proof.get("architecture") == {1: "x86_64", 2: "aarch64"}.get(platform) and proof.get("user") == "10001:10001"
         and proof.get("read_only") is True and proof.get("budget") == budget and
         proof.get("state", {}).get("running") is False and
         proof["state"].get("exit_code") == result["exit_code"], "actual-profile-execution-unbound")
    need(type(result.get("output")) is str and len(result["output"].encode()) <= 1024**2,
         "profile-output-bound")
    return result["output"].encode()


def split_monitor(raw):
    """Remove only the last trusted monitor record, leaving child data intact."""
    need(type(raw) is bytes and len(raw) <= 1024**2, "monitor-framing-bound")
    body, marker, footer = raw.rpartition(b"\nHPMONITOR2\t")
    need(marker and len(footer) <= 4096, "monitor-receipt-missing")
    try:
        receipt = json.loads(footer)
    except (ValueError, UnicodeError):
        raise Refused("monitor-framing") from None
    need(type(receipt) is dict and receipt.get("exit") in (0, 1, 2) and
         type(receipt.get("output_bytes")) is int and receipt["output_bytes"] == len(body), "monitor-framing")
    return body, receipt


class ProfileContext:
    """Narrow adapter supplied by the fixed TaskStore integration.

    No callback has a default or alternate allocator. append_callback must
    include retained_input_bytes in the existing64MiB evidence admission,
    including plans, partials, orphan JSON and input slots before publication.
    """

    def __init__(self, *, source_callback, append_callback, run_callback,
                 inputs_callback, restart_callback, released_callback, budgets):
        self._source = source_callback
        self._append = append_callback
        self._run = run_callback
        self._inputs = inputs_callback
        self._restart = restart_callback
        self._released = released_callback
        self.budgets = budgets

    def source(self):
        return self._source()

    def append(self, kind, value, *, retained_input_bytes=0):
        need(type(retained_input_bytes) is int and 0 <= retained_input_bytes <= 1024**2,
             "retained-input-admission-bound")
        return self._append(kind, value, retained_input_bytes=retained_input_bytes)

    def inputs(self, mode, files, build):
        need(mode in MODES and type(files) is dict and sum(len(v) for v in files.values()) <= 1024**2,
             "fixed-input-bound")
        pins = {name: {"bytes": len(raw), "sha256": digest(raw)} for name, raw in files.items()}
        self.append("rustfs-input-admission", {"mode": mode, "pins": pins},
                    retained_input_bytes=sum(len(raw) for raw in files.values()))
        return self._inputs(mode, files, build)

    def run(self, mode):
        need(mode in MODES and self.child_released(), "profile-child-or-mode-unavailable")
        # The integration returns a known WorkloadFailed.result as data after
        # persisting the primary failure. It never retries a partial/unknown run.
        return self._run(mode)

    def child_released(self):
        return self._released() is True

    def restart(self):
        need(self.child_released(), "restart-with-live-client-refused")
        return self._restart()

    @staticmethod
    def public_result(result):
        return {key: result[key] for key in ("mode", "container_id", "exit_code", "truncated", "execution")}

    @staticmethod
    def output_binding(result):
        # TaskStore has already retained the complete bounded raw pipes. Avoid
        # a second JSON expansion of replacement-decoded malformed bytes here.
        streams = result.get("raw_streams")
        need(type(streams) is dict and set(streams) == {"stdout", "stderr"} and
             all(type(raw) is bytes for raw in streams.values()) and
             sum(map(len, streams.values())) <= 1024**2, "raw-output-binding-unavailable")
        return {"protocol": result.get("output_protocol"), "encoding": result.get("encoding"),
                "streams": {name: {"bytes": len(raw), "sha256": digest(raw)}
                            for name, raw in streams.items()}}

    def _body(self, result, mode):
        identity = self.source()
        raw = check_execution(result, image=identity["image"],
                              budget=self.budgets["PROBE" if mode in MODES[1:3] else "BUILD"], platform=identity["platform"])
        body, receipt = split_monitor(raw)
        need(receipt.get("mode") == mode and receipt.get("exit") == result["exit_code"], "monitor-mode-exit")
        expected_network = identity["namespace"]+"-net" if mode in MODES[1:3] else "none"
        need(result["execution"].get("network") == expected_network, "profile-network-changed")
        budget = self.budgets["PROBE" if mode in MODES[1:3] else "BUILD"]
        try:
            caps = receipt["effective_limits"]
            need(type(caps) is dict and all(type(caps[k]) is str and len(caps[k]) <= 64
                 for k in ("memory.max", "pids.max", "memory.swap.max", "cpu.max")), "effective-kernel-caps")
            memory, processes, swap = int(caps["memory.max"]), int(caps["pids.max"]), int(caps["memory.swap.max"])
            quota, period = (int(x) for x in caps["cpu.max"].split())
            need(0 < memory <= budget["memory_bytes"] and 0 < processes <= budget["pids"] and swap == 0 and
                 quota > 0 and period > 0 and quota*1000 <= period*budget["cpu_millis"], "effective-kernel-caps")
        except (KeyError, TypeError, ValueError):
            raise Refused("effective-kernel-caps") from None
        return body, receipt

    def capture(self, result, mode):
        need(mode in MODES[1:3], "capture-mode")
        # Preserve a known first outcome through ordinary evidence before any
        # dependent parse or eligibility operation, including failed partials.
        self.append("rustfs-primary-outcome", {"result": self.public_result(result),
                                               "output_binding": self.output_binding(result)})
        body, _receipt = self._body(result, mode)
        need(len(body) <= HISTORY["WIRE_BYTES"], "capture-byte-bound")
        return body

    def attest_checker(self, result):
        self.append("rustfs-build-outcome", {"result": self.public_result(result), "output_binding": self.output_binding(result)})
        _body, receipt = self._body(result, MODES[0])
        need(result["exit_code"] == 0 and type(receipt.get("checker")) is dict, "checker-build-failed")
        return receipt["checker"]

    def checker_report(self, result):
        self.append("rustfs-checker-outcome", {"result": self.public_result(result), "output_binding": self.output_binding(result)})
        body, _receipt = self._body(result, MODES[3])
        need(len(body) <= 4096, "checker-report-bound")
        report = json.loads(body)
        need(type(report) is dict and report.get("profile") == PROFILE and type(report.get("ready")) is bool and
             type(report.get("transitions")) is int and 0 <= report["transitions"] < 100000 and
             type(report.get("scenarios")) is list and len(report["scenarios"]) == 6 and
             all(v in {"supported", "failed", "inconclusive"} for v in report["scenarios"]), "checker-report-shape")
        return report


def qualify(context):
    """Full serial build/capture/restart/capture/check sequence.

    context is supplied by the separately reviewed ordinary TaskStore binding.
    It owns source()/append()/inputs()/run()/restart()/attest_checker() and
    assertion that each previous known client child has been released. These
    callbacks cannot be replaced with a new CLI/allocator/evidence-store route.
    """
    identity = context.source()
    need(type(identity) is dict and set(identity) >= {"claim", "owner", "packet_sha256", "source_sha256",
         "image", "server_image", "server_cid", "server_config_sha256", "volume_sha256", "lock_sha256",
         "config_sha256", "topology_sha256", "platform", "namespace", "bucket", "output_budget"},
         "source-binding-incomplete")
    need(identity["platform"] in (1, 2) and identity["output_budget"]//2 >= HISTORY["TOTAL_BYTES"]+4096,
         "packet-framing-too-small")
    # No product history is reconstructed from results. Freeze source, program,
    # deterministic seeds and complete matrix before any client capture.
    run = digest(canonical({k: identity[k] for k in ("claim", "owner", "packet_sha256", "source_sha256")}))[:32]
    expected = dict(identity)
    context.append("rustfs-probe-intent", {"profile": PROFILE, "identity": identity, "limits": LIMITS,
                                         "qualification": "no result yet"})
    context.inputs(MODES[0], {"config.json": canonical({"source_sha256": identity["source_sha256"],
                                                       "packet_sha256": identity["packet_sha256"]})}, None)
    build_result = context.run(MODES[0])
    need(context.child_released(), "build-child-still-held")
    build = context.attest_checker(build_result)
    need(set(build) >= {"sha256", "source_sha256", "lock_sha256", "toolchain", "architecture"}
         and build["source_sha256"] == identity["source_sha256"] and
         build["lock_sha256"] == identity["lock_sha256"] and
         build["toolchain"].startswith("rustc 1.97.1 ") and
         build["architecture"] == ("x86_64" if identity["platform"] == 1 else "aarch64") and
         re.fullmatch(r"[0-9a-f]{64}", build["sha256"]), "checker-build-binding")
    context.append("rustfs-checker-build", build)
    transport_hash = digest((ROOT / "dev/rustfs_probe_transport.py").read_bytes())
    fixed_program = digest(canonical({"profile": PROFILE, "race_witness": HISTORY["RACE_WITNESS"],
                                      "operations": HISTORY["workload"]()}))
    value = HISTORY["plan"]({"digests": [identity["source_sha256"], transport_hash, build["sha256"],
                              digest(identity["server_image"].encode()), identity["config_sha256"],
                              identity["topology_sha256"], fixed_program],
                              "run": run, "platform": identity["platform"], "bucket": identity["bucket"]})
    wire = HISTORY["plan_wire"](value)
    # The Linux host and its containers share CLOCK_MONOTONIC. Freeze one epoch
    # across both phases and enforce the whole120s window, including restart.
    origin = time.monotonic()
    config = {"plan": value, "plan_sha256": digest(wire), "clock_origin": origin,
              "source_sha256": identity["source_sha256"], "packet_sha256": identity["packet_sha256"],
              "checker_sha256": build["sha256"]}
    need(len(canonical(config)) <= 65536, "configuration-bound")
    context.append("rustfs-probe-plan", {"plan": value, "wire_sha256": digest(wire), "checker": build})
    context.inputs(MODES[1], {"config.json": canonical(config), "plan.wire": wire}, build)
    before_result = context.run(MODES[1])
    before_raw = context.capture(before_result, MODES[1])
    need(context.child_released(), "client-child-still-held")
    need(type(before_raw) is bytes and len(before_raw) <= HISTORY["WIRE_BYTES"], "partial-history-bound")
    # Persist primary partials before any restart or other dependent operation.
    context.append("rustfs-before-restart", {"sha256": digest(before_raw), "history": before_raw.decode("ascii"),
                                            "result": context.public_result(before_result)})
    need(before_result["exit_code"] == 0 and time.monotonic()-origin < 120, "before-phase-failed")
    before, after, storage_admission = context.restart()
    receipt = restart_receipt(before, after, storage_admission)
    context.append("rustfs-restart", receipt)
    config.update(prior_sha256=digest(before_raw), restart=receipt)
    context.inputs(MODES[2], {"config.json": canonical(config), "plan.wire": wire,
                              "prior.history": before_raw}, build)
    after_result = context.run(MODES[2])
    raw = context.capture(after_result, MODES[2])
    need(context.child_released(), "client-child-still-held")
    context.append("rustfs-capture", {"sha256": digest(raw), "history": raw.decode("ascii"),
                                    "result": context.public_result(after_result)})
    need(after_result["exit_code"] == 0 and time.monotonic()-origin <= 120 and
         type(raw) is bytes and len(raw) <= HISTORY["WIRE_BYTES"], "capture-failed-or-over-budget")
    context.inputs(MODES[3], {"plan.wire": wire, "capture.history": raw,
                              "restart.json": canonical(receipt), "admission.sha256": digest(wire).encode()}, build)
    checked = context.run(MODES[3])  # BUILD envelope, after known client release.
    need(context.child_released(), "validator-child-still-held")
    report = context.checker_report(checked)
    context.append("rustfs-eligibility", {"result": context.public_result(checked), "report": report,
                                         "plan_sha256": digest(wire), "capture_sha256": digest(raw)})
    need(context.source() == expected, "source-binding-changed")
    need((checked["exit_code"] == 0 and report["ready"] is True and
          report["scenarios"] == ["supported"]*6) or
         (checked["exit_code"] == 1 and report["ready"] is False and
          "failed" in report["scenarios"] and "inconclusive" not in report["scenarios"]),
         "required-capability-evidence-incomplete")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("contract", "qualify"))
    args = parser.parse_args()
    if args.command == "contract":
        need(LIMITS["wire_bytes"]+LIMITS["diagnostics"] == 768*1024 and len(HISTORY["workload"]()) <= 256,
             "fixed-profile-contract")
        print(json.dumps({"profile": PROFILE, "limits": LIMITS, "live_execution": False}))
        return 0
    task = runpy.run_path(str(ROOT / "tools/task.py"))
    dispatch = task.get("qualify_rustfs_capabilities")
    need(callable(dispatch), "normal-task-dispatch-integration-required")
    return dispatch()  # Normal CLI/source/evidence integration only.


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (Refused, ValueError, OSError, KeyError):
        print("rustfs-capability-controller-refused", file=__import__("sys").stderr)
        raise SystemExit(2) from None
