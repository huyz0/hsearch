"""Read-only evidence binding cannot confer task authority or hide source edits."""
from pathlib import Path
import runpy
import copy
import json
import shutil
import subprocess
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
TASK = runpy.run_path(str(ROOT / "tools/task.py"))
REF = runpy.run_path(str(ROOT / "tools/reference.py"))


class OracleControls(unittest.TestCase):
    def test_type_changes_missing_fields_and_incomplete_exact_counts_fail(self):
        for expected, body in (({"status": 200, "fields": {"/found": True}}, {"found": 1}),
                               ({"status": 200, "fields": {"/_source": {"price": 10}}}, {"_source": {"price": "10"}}),
                               ({"status": 200, "exact_total": 1}, {"hits": {"total": {"value": True, "relation": "eq"}}}),
                               ({"status": 200, "exact_total": 11}, {"hits": {"total": {"value": 2, "relation": "gte"}}}),
                               ({"status": 200, "fields": {"/found": True}}, {})):
            with self.subTest(expected=expected), self.assertRaises(REF["Refused"]):
                REF["verify_step"](expected, 200, body, {})
        with self.assertRaises(REF["Refused"]):
            REF["verify_step"]({"status": 409, "error_required": True}, 200, {"result": "created"}, {})

    def test_search_verification_rejects_boolean_and_float_counts(self):
        case = {"id": "one-result", "reference_oracle": {"kind": "hits", "ids": ["011"], "exact_total": 1}}
        body = {"hits": {"hits": [{"_id": "011", "_source": {"uid": "011"}}], "total": {"value": 1, "relation": "eq"}}}
        REF["verify_search"](case, 200, body)
        for bad in (True, 1.0):
            changed = copy.deepcopy(body)
            changed["hits"]["total"]["value"] = bad
            with self.subTest(value=bad), self.assertRaises(REF["Refused"]):
                REF["verify_search"](case, 200, changed)

    def test_bucket_verification_requires_integer_counts(self):
        case = {"id": "bucket-types", "reference_oracle": {"kind": "aggregations", "values": {"categories": [{"key": "book", "doc_count": 1}]}}}
        body = {"aggregations": {"categories": {"buckets": [{"key": "book", "doc_count": 1}]}}}
        REF["verify_search"](case, 200, body)
        for bad in (True, 1.0):
            changed = copy.deepcopy(body)
            changed["aggregations"]["categories"]["buckets"][0]["doc_count"] = bad
            with self.subTest(value=bad), self.assertRaises(REF["Refused"]):
                REF["verify_search"](case, 200, changed)

    def test_metrics_allow_finite_numeric_equivalence_but_never_booleans(self):
        case = {"id": "metric-types", "reference_oracle": {"kind": "aggregations", "values": {"minimum": 0}}}
        for value in (0, 0.0):
            REF["verify_search"](case, 200, {"aggregations": {"minimum": {"value": value}}})
        for bad in (False, True, "0", float("nan"), float("inf"), -float("inf")):
            with self.subTest(value=bad), self.assertRaises(REF["Refused"]):
                REF["verify_search"](case, 200, {"aggregations": {"minimum": {"value": bad}}})

    def test_analyzer_verification_requires_integer_positions_and_offsets(self):
        tokens = [{"token": "hello", "position": 0, "start_offset": 0, "end_offset": 5},
                  {"token": "world", "position": 1, "start_offset": 6, "end_offset": 11}]
        case = {"id": "analyzer-types", "tokens": tokens}
        REF["verify_analyzer"](case, 200, {"tokens": copy.deepcopy(tokens)})
        for index, field, bad in [(0, "position", False), (0, "start_offset", False), (1, "position", True),
                                  *[(i, key, float(token[key])) for i, token in enumerate(tokens)
                                    for key in ("position", "start_offset", "end_offset")]]:
            changed = copy.deepcopy(tokens)
            changed[index][field] = bad
            with self.subTest(index=index, field=field, value=bad), self.assertRaises(REF["Refused"]):
                REF["verify_analyzer"](case, 200, {"tokens": changed})

    def test_truthful_count_uses_independent_total_not_the_reported_relation_alone(self):
        expected = {"status": 200, "truthful_total": {"exact": 11, "threshold": 2}}
        for value, relation in ((11, "eq"), (2, "gte"), (7, "gte")):
            REF["verify_step"](expected, 200, {"hits": {"total": {"value": value, "relation": relation}}}, {})
        for value, relation in ((2, "eq"), (12, "gte"), (1, "gte"), (True, "eq"), (11, "unknown")):
            with self.subTest(value=value, relation=relation), self.assertRaises(REF["Refused"]):
                REF["verify_step"](expected, 200, {"hits": {"total": {"value": value, "relation": relation}}}, {})

    def test_dynamic_tokens_remain_typed_and_paths_encode_captured_data(self):
        self.assertEqual(REF["bind_value"]({"search_after": "{last}"}, {"last": ["003", 7]}), {"search_after": ["003", 7]})
        self.assertEqual(REF["bind_path"]("/{index}/_doc/{id}", {"index": "owned", "id": "x/y?z"}), "/owned/_doc/x%2Fy%3Fz")
        for value in (True, {}, "x" * 20000, ["x"] * 17):
            with self.subTest(value=type(value).__name__), self.assertRaises(REF["Refused"]):
                REF["capture_value"](value)


class ResourceControls(unittest.TestCase):
    def fixture(self):
        namespace, cid, image, owner = "hsearch-api-reference-0123456789ab", "a" * 64, "sha256:" + "b" * 64, "owner"
        info = {"Id": cid, "Image": image, "Config": {"User": "1000:1000", "Tty": True,
                "Labels": {REF["SANDBOX"]["LABEL"]: owner, "io.hsearch.namespace": namespace}}, "Mounts": [],
                "State": {"Running": False, "Status": "exited"},
                "HostConfig": {"NetworkMode": "none", "PortBindings": {}, "PublishAllPorts": False, "NanoCpus": 2000000000,
                "Memory": 4 * 1024**3, "MemorySwap": 4 * 1024**3, "PidsLimit": 512, "Privileged": False, "PidMode": "",
                "CapDrop": ["ALL"], "SecurityOpt": ["no-new-privileges"]}}
        record = {"kind": "api-reference", "source": "source", "engine": "engine", "owner": owner, "pid": 123,
                  "process_identity": "old-process", "container_id": cid, "create_pending": False,
                  "evidence_captured": True, "reference_image": image}
        calls = []
        def command(*args, **_kwargs):
            calls.append(args)
            return json.dumps([info]) if args[:2] == ("container", "inspect") else ""
        sandbox = SimpleNamespace(source_root="source", docker=SimpleNamespace(engine="engine", command=command),
                                  registry=SimpleNamespace(get=lambda _namespace: record, release=lambda _namespace: calls.append(("released",))))
        return namespace, cid, image, owner, info, record, calls, sandbox

    def test_privilege_network_mount_or_budget_changes_refuse(self):
        original = self.fixture()[4]
        REF["check_caps"](original)
        for key, value in (("NetworkMode", "host"), ("Privileged", True), ("MemorySwap", -1), ("PidMode", "host"),
                           ("PidsLimit", 1024), ("CapDrop", []), ("SecurityOpt", [])):
            info = copy.deepcopy(original)
            info["HostConfig"][key] = value
            with self.subTest(key=key), self.assertRaises(REF["Refused"]):
                REF["check_caps"](info)
        info = copy.deepcopy(original)
        info["Mounts"] = [{"Type": "bind", "Source": "/private"}]
        with self.assertRaises(REF["Refused"]):
            REF["check_caps"](info)

    def test_cleanup_never_removes_foreign_live_unknown_or_uncaptured_state(self):
        for change in ("owner", "live", "unknown_create", "evidence", "source", "owner_live", "owner_unknown"):
            namespace, _, _, _, info, record, calls, sandbox = self.fixture()
            owner_identity = None
            if change == "owner":info["Config"]["Labels"][REF["SANDBOX"]["LABEL"]] = "foreign"
            elif change == "live":info["State"].update(Running=True, Status="running")
            elif change == "unknown_create":record.update(create_pending=True, container_id=None)
            elif change == "evidence":record["evidence_captured"] = False
            elif change == "source":record["source"] = "other-source"
            elif change == "owner_live":owner_identity = "old-process"
            else:owner_identity = "unknown"
            with self.subTest(change=change), patch.dict(REF["SANDBOX"], owner_identity=lambda _pid: owner_identity), self.assertRaises(REF["Refused"]):
                REF["cleanup_failed"](sandbox, namespace)
            self.assertFalse(any(item[:2] == ("container", "rm") or item == ("released",) for item in calls))

    def test_cleanup_removes_only_the_known_stopped_owned_id_without_force(self):
        namespace, cid, _, _, _, _, calls, sandbox = self.fixture()
        with patch.dict(REF["SANDBOX"], owner_identity=lambda _pid: None):
            result = REF["cleanup_failed"](sandbox, namespace)
        self.assertFalse(result["force"])
        self.assertIn(("container", "rm", cid), calls)
        self.assertEqual(calls[-1], ("released",))


@unittest.skipUnless(shutil.which("git"), "Real source binding requires Git")
class ReadOnlySourceControls(unittest.TestCase):
    def test_detached_evidence_remains_clean_but_cannot_acquire_task_authority(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder) / "source"
            root.mkdir()
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, stderr=subprocess.STDOUT, text=True, timeout=10).strip()
            git("init", "-q", "-b", "main")
            git("config", "user.name", "Readonly fixture")
            git("config", "user.email", "fixture@example.invalid")
            git("config", "core.autocrlf", "false")
            (root / "tracked.txt").write_bytes(b"committed\n")
            git("add", ".")
            git("commit", "-qm", "fixture")
            head = git("rev-parse", "HEAD")
            git("checkout", "-q", "--detach", head)
            result = TASK["source_fingerprint"](root, head, [])
            self.assertTrue(result["clean"])
            self.assertEqual(result["head"], head)
            store = TASK["TaskStore"](Path(folder) / "state", root)
            record = {"worktree": str(root), "branch": "main", "packet": {"base_revision": head, "owned_paths": []}}
            with self.assertRaisesRegex(TASK["Refused"], "registration/branch changed"):
                store.fingerprint(record)
            git("update-index", "--skip-worktree", "tracked.txt")
            (root / "tracked.txt").write_bytes(b"hidden changed bytes\n")
            with self.assertRaisesRegex(TASK["Refused"], "Hidden index"):
                TASK["source_fingerprint"](root, head, [])




class DifferentialReferenceEvidence(unittest.TestCase):
    def test_actual_reference_projection_retains_typed_count_and_projection(self):
        d = REF["DIFFERENTIAL"]
        bundle = d["load_fixture"](ROOT)
        case = bundle["inputs"]["fixtures/api/core-v1/search-cases.json"][0]
        oracle = case["reference_oracle"]
        body = {"hits": {"hits": [{"_id": identity, "_source": {"uid": identity}} for identity in oracle["ids"]],
                         "total": {"value": oracle["exact_total"], "relation": "eq"}}}
        REF["verify_search"](case, 200, body)
        record = d["observation"](bundle, case, 200, body)
        self.assertEqual(record["value"]["ids"], oracle["ids"])
        for bad in (True, float(oracle["exact_total"])):
            changed = copy.deepcopy(body)
            changed["hits"]["total"]["value"] = bad
            with self.assertRaises(ValueError):
                d["observation"](bundle, case, 200, changed)
        changed = copy.deepcopy(body)
        changed["hits"]["hits"][0]["_source"]["unselected"] = 1
        with self.assertRaises(ValueError):
            d["observation"](bundle, case, 200, changed)

    def test_finite_reference_metrics_normalize_only_numeric_values(self):
        d = REF["DIFFERENTIAL"]
        bundle = d["load_fixture"](ROOT)
        case = next(x for x in bundle["inputs"]["fixtures/api/core-v1/search-cases.json"] if x["id"] == "finite-metrics")
        body = {"aggregations": {name: {"value": value} for name, value in case["reference_oracle"]["values"].items()}}
        REF["verify_search"](case, 200, body)
        self.assertEqual(d["observation"](bundle, case, 200, body)["outcome"], "pass")
        zero = copy.deepcopy(body)
        zero["aggregations"]["min_price"]["value"] = -0.0
        REF["verify_search"](case, 200, zero)
        self.assertEqual(d["observation"](bundle, case, 200, zero)["outcome"], "pass")
        for bad in (True, float("inf")):
            changed = copy.deepcopy(body)
            changed["aggregations"]["sum_price"]["value"] = bad
            with self.assertRaises(ValueError):
                d["observation"](bundle, case, 200, changed)

if __name__ == "__main__":
    unittest.main()
