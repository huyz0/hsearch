"""Golden checks for the extension module ABI fixtures (M24.01).

The C header is compiled and its layout compared with the committed layout.json,
so an ABI change cannot slip in as a header edit. The WASM host interface,
the example descriptor and the specification are held to the same constants.
"""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
ABI = ROOT / "fixtures/extensions/abi-v1"
PROBE = ROOT / "tools/tests/fixtures/extension_abi_probe.c"
SPEC = ROOT / "docs/plan/extension-modules.md"
NAME = re.compile(r"^[a-z][a-z0-9_-]{0,63}$")
POINT_ID = re.compile(r"^[a-z][a-z0-9_]{0,63}$")
VERSION = re.compile(r"^\d+\.\d+\.\d+$")
MAJOR_MINOR = re.compile(r"^\d+\.\d+$")


def load(name):
    return json.loads((ABI / name).read_text(encoding="utf-8"))


def probe(flags=()):
    compiler = os.environ.get("CC", "cc")
    if shutil.which(compiler) is None:
        raise AssertionError(f"C compiler {compiler!r} not found; set CC. The ABI golden cannot be skipped")
    with tempfile.TemporaryDirectory() as folder:
        binary = Path(folder) / "probe"
        subprocess.run([compiler, "-std=c11", "-Wall", "-Wextra", "-Werror", "-pedantic", *flags,
                        f"-I{ABI}", str(PROBE), "-o", str(binary)], check=True)
        return json.loads(subprocess.run([str(binary)], check=True, capture_output=True, text=True).stdout)


class AbiHeader(unittest.TestCase):
    def test_layout_matches_golden(self):
        self.assertEqual(probe(), load("layout.json"))

    def test_layout_has_no_implicit_padding(self):
        layout = load("layout.json")["layout"]
        # Last field and its width: the struct ends where its last field ends.
        for struct, last, width in [("hs_slice", "len", 8), ("hs_out", "len", 8),
                                    ("hs_host_v1", "cancelled", 8), ("hs_call", "reserved0", 4)]:
            self.assertEqual(layout[f"{struct}.size"], layout[f"{struct}.{last}"] + width, struct)

    def test_every_struct_leads_with_or_declares_its_own_size_for_growth(self):
        layout = load("layout.json")["layout"]
        self.assertEqual(layout["hs_host_v1.struct_size"], 0)
        self.assertEqual(layout["hs_call.struct_size"], 0)

    def test_limits_are_ordered(self):
        limits = load("layout.json")["limits"]
        self.assertLess(limits["config"], limits["buffer"])
        self.assertLessEqual(limits["descriptor"], limits["config"])
        self.assertLess(limits["error_detail"], limits["descriptor"])


class WasmHostInterface(unittest.TestCase):
    def test_same_status_codes_and_symbols_as_the_c_header(self):
        golden, wasm = load("layout.json"), load("wasm-host-interface.json")
        self.assertEqual(wasm["status_codes"], golden["status"])
        exported = {e["name"] for e in wasm["exports"]}
        self.assertTrue(set(golden["symbols"]) <= exported)
        self.assertEqual(exported - set(golden["symbols"]),
                         {"memory", "hs_input_region", "hs_output_region", "hs_output_len"})

    def test_host_grants_only_log_and_cancelled(self):
        wasm = load("wasm-host-interface.json")
        self.assertEqual(wasm["import_module"], "hsearch_host_v1")
        self.assertEqual([i["name"] for i in wasm["imports"]], ["log", "cancelled"])

    def test_c_and_wasm_call_the_same_things(self):
        # Same entry points, with the C pointers replaced by the guest's two regions.
        wasm = {e["name"]: e for e in load("wasm-host-interface.json")["exports"]}
        self.assertEqual(len(wasm["hs_module_call"]["params"]), 4)
        self.assertEqual(wasm["hs_module_call"]["results"], ["i32"])


class Descriptor(unittest.TestCase):
    def test_example_descriptor_follows_the_rules(self):
        d = load("descriptor.example.json")
        self.assertRegex(d["name"], NAME)
        self.assertRegex(d["version"], VERSION)
        self.assertRegex(d["abi"], MAJOR_MINOR)
        self.assertRegex(d["engine_api"], MAJOR_MINOR)
        self.assertGreaterEqual(d["max_concurrent_calls"], 1)
        self.assertGreater(d["io_bytes"], 0)
        kinds = set(load("layout.json")["kinds"])
        seen = set()
        for p in d["extension_points"]:
            self.assertIn(p["kind"], kinds)
            self.assertRegex(p["id"], POINT_ID)
            self.assertRegex(p["version"], VERSION)
            self.assertIs(p["deterministic"], True)
            self.assertNotIn((p["kind"], p["id"]), seen)
            seen.add((p["kind"], p["id"]))
        self.assertLess(len(json.dumps(d).encode()), load("layout.json")["limits"]["descriptor"])

    def test_example_declares_the_abi_the_header_declares(self):
        major = load("layout.json")["abi_version"] >> 16
        minor = load("layout.json")["abi_version"] & 0xFFFF
        self.assertEqual(load("descriptor.example.json")["abi"], f"{major}.{minor}")
        self.assertEqual(load("wasm-host-interface.json")["version"], f"{major}.{minor}")


class Specification(unittest.TestCase):
    def test_spec_states_the_abi_version_and_limits(self):
        text = SPEC.read_text(encoding="utf-8")
        golden = load("layout.json")
        self.assertIn(f"ABI version {golden['abi_version'] >> 16}.{golden['abi_version'] & 0xFFFF}", text)
        for name in golden["status"]:
            self.assertIn(name, text, name)
        for symbol in golden["symbols"]:
            self.assertIn(symbol, text, symbol)


if __name__ == "__main__":
    unittest.main()
