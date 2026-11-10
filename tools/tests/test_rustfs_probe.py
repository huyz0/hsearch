"""HOST controls for the diagnostic protocol. No real provider qualification."""
from pathlib import Path
import datetime
import hashlib
import importlib.util
import io
import json
import runpy
import sys
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]


def module(name):
    path = ROOT / "dev" / (name + ".py")
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    sys.modules[name] = value
    spec.loader.exec_module(value)
    return value


TRANSPORT = module("rustfs_probe_transport")
HISTORY = module("rustfs_probe_history")
CLIENT = module("rustfs_probe")
CONTROL = runpy.run_path(str(ROOT / "tools/rustfs_probe.py"))


def plan():
    return HISTORY.plan({"digests": ["1"*64]*7, "run": "2"*32, "platform": 1, "bucket": "hsearch-probe-test"})


class Socket:
    def __init__(self, raw=b""):
        self.raw = raw

    def makefile(self, _mode):
        return io.BytesIO(self.raw)

    def settimeout(self, _timeout):
        pass


class Connection:
    response_raw = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc"
    instances = []

    def __init__(self, *args, **kwargs):
        self.sock, self.sent, self.headers = Socket(self.response_raw), bytearray(), {}
        self.instances.append(self)

    def connect(self):
        pass

    def putrequest(self, method, target, **_kwargs):
        self.method, self.target = method, target

    def putheader(self, name, value):
        self.headers[name] = value

    def endheaders(self):
        raw = (self.method+" "+self.target+" HTTP/1.1\r\n"+
               "".join(k+": "+v+"\r\n" for k,v in self.headers.items())+"\r\n").encode()
        self.send(raw)

    def send(self, data):
        self.sent.extend(data)

    def getresponse(self):
        result = self.response_class(self.sock, method=self.method)
        result.begin()
        return result

    def close(self):
        self.closed = True


class TransportControls(unittest.TestCase):
    def transport(self):
        with patch.object(TRANSPORT.ssl, "create_default_context") as context:
            value = TRANSPORT.Transport("https://rustfs:9000", "/owned/ca.crt", "ACCESSFIXTURE", "SECRET-FIXTURE",
                                        connection=Connection)
            context.assert_called_once_with(cafile="/owned/ca.crt")
        return value

    def call(self, raw, **options):
        with patch.object(Connection, "response_raw", raw):
            return self.transport().attempt("GET", "hsearch-probe-test", "key", [], {}, **options)

    def test_private_tls_endpoint_and_exact_query_and_conditions_are_signed(self):
        with patch.object(TRANSPORT.ssl, "create_default_context"):
            for endpoint in ("http://rustfs:9000", "https://evil:9000", "https://u@rustfs:9000", "https://rustfs:9000/?x=y"):
                with self.subTest(endpoint=endpoint), self.assertRaises(TRANSPORT.Refused):
                    TRANSPORT.Transport(endpoint, "unused", "access", "secret")
        self.assertEqual(TRANSPORT.canonical_query([("versionId", "a/+="), ("uploads", "")]), "uploads=&versionId=a%2F%2B%3D")
        stamp = datetime.datetime(2026, 11, 10, tzinfo=datetime.timezone.utc)
        kwargs = ("PUT", "/bucket/key", [("versionId", "opaque")])
        base = {"host": "rustfs:9000", "if-match": '"opaque token"'}
        signed = TRANSPORT.sign(*kwargs, base, "a"*64, "ACCESS", "SECRET", stamp)
        self.assertIn("if-match", signed["authorization"])
        changed = TRANSPORT.sign(*kwargs, {**base, "if-match": "different"}, "a"*64, "ACCESS", "SECRET", stamp)
        self.assertNotEqual(signed["authorization"], changed["authorization"])
        with self.assertRaises(TRANSPORT.Refused):
            TRANSPORT.sign(*kwargs, {"host": "rustfs:9000\r\nInjected: yes"}, "a"*64, "ACCESS", "SECRET", stamp)

    def test_streamed_selected_byte_oracle_differs_from_whole_body(self):
        self.assertEqual(b"".join(TRANSPORT.payload(0, 16)).hex(), "afcd1d7b39a820e2f465b9a16a9e786e")
        expected = b"".join(TRANSPORT.payload(7, 1000))
        self.assertEqual(b"".join(TRANSPORT.payload(7, 1000)), expected)
        self.assertEqual(TRANSPORT.digest_payload(7, 27, 255), hashlib.sha256(expected[255:282]).hexdigest())
        self.assertNotEqual(TRANSPORT.digest_payload(7, 27, 255), TRANSPORT.digest_payload(7, 1000))
        self.assertNotEqual(TRANSPORT.digest_payload(7, 257, 255), TRANSPORT.digest_payload(7, 257, 511))

    def test_complete_response_reports_body_and_http_wire_bytes_without_secrets(self):
        result = self.call(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nETag: opaque\r\n\r\nabc")
        self.assertEqual(result["boundary"], "reply")
        self.assertEqual(result["response_bytes"], 3)
        self.assertGreater(result["response_wire_bytes"], 3)
        self.assertEqual(result["digest"], hashlib.sha256(b"abc").hexdigest())
        self.assertNotIn("SECRET-FIXTURE", repr(result))

    def test_redirect_encoding_duplicate_headers_and_oversize_refuse_without_retry(self):
        for raw in [b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", b"HTTP/1.1 307 Redirect\r\nContent-Length: 0\r\n\r\n",
                    b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 0\r\n\r\n",
                    b"HTTP/1.1 200 OK\r\nETag: a\r\nETag: b\r\nContent-Length: 0\r\n\r\n",
                    b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n",
                    b"HTTP/1.1 200 OK\r\nX: "+b"a"*17000+b"\r\n\r\n"]:
            count = len(Connection.instances)
            result = self.call(raw)
            self.assertTrue(result["error"])
            self.assertEqual(len(Connection.instances), count+1)
            self.assertTrue(Connection.instances[-1].closed)

    def test_true_suppression_and_socket_close_have_distinct_boundaries(self):
        suppressed = self.call(Connection.response_raw, suppress=True)
        disconnected = self.call(Connection.response_raw, disconnect=True)
        self.assertEqual(suppressed["boundary"], "reply-suppressed")
        self.assertEqual(suppressed["status"], 200)
        self.assertEqual(disconnected["boundary"], "after-transmit-before-reply")
        self.assertEqual(disconnected["status"], 0)
        self.assertEqual(disconnected["response_bytes"], 0)

    def test_hex_secret_in_valid_envelope_is_refused_before_hex_recording(self):
        secret = "a"*64  # Same shape as dev/init.py token_hex(32).
        raw = ("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nETag: opaque\r\n"
               "x-amz-meta-request: admitted-request\r\nx-amz-meta-envelope: "+secret+
               "\r\nx-amz-meta-caller: probe-caller-v2\r\n\r\n").encode()
        value = self.transport()
        value.secret = secret
        with patch.object(Connection, "response_raw", raw):
            result = value.attempt("GET", "hsearch-probe-test", "key", [], {})
        self.assertEqual(result["error"], "secret-bearing-response")
        self.assertNotIn(secret, repr(result))
        self.assertEqual(result["metadata"], "")

    def test_xml_and_secret_bearing_values_are_refused_before_recording(self):
        for raw in (b'<!DOCTYPE r [<!ENTITY e "x">]><r>&e;</r>', b"<a>"*13+b"x"+b"</a>"*13, b"<a>"+b"x"*17000+b"</a>"):
            with self.subTest(size=len(raw)), self.assertRaises(TRANSPORT.Refused):
                TRANSPORT.bounded_xml(raw)
        result = self.call(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nETag: SECRET-FIXTURE\r\n\r\n")
        self.assertEqual(result["error"], "secret-bearing-response")
        self.assertNotIn("SECRET-FIXTURE", repr(result))


class HistoryControls(unittest.TestCase):
    def test_full_matrix_and_framing_reserve_fit_original_caps(self):
        value = plan()
        recorder = HISTORY.Recorder(value)
        for row in value["operations"]:
            recorder.reserve(row)
        self.assertEqual(len(value["operations"]), 154)
        self.assertLessEqual(recorder.reserved, 704*1024)
        self.assertEqual(HISTORY.TOTAL_BYTES, 768*1024)
        self.assertEqual(len([r for r in value["operations"] if r["race"] == 1]), 16)
        self.assertEqual(len([r for r in value["operations"] if r["race"] == 2]), 16)
        self.assertTrue({"replay", "stale-update", "reuse-id", "versions", "resolve-head", "resolve"} <=
                        {r["action"] for r in value["operations"]})
        changed = json.loads(json.dumps(value))
        changed["operations"].pop()
        with self.assertRaises(ValueError):
            HISTORY.validate_plan(changed)

    def test_intent_is_flushed_before_io_and_partial_has_no_fabricated_completion(self):
        emitted = []
        value = plan()
        recorder = HISTORY.Recorder(value, emit=emitted.append)
        spec = value["operations"][0]
        attempt = recorder.reserve(spec)
        self.assertEqual(emitted, [b"A\t1\t1\n"])
        partial = recorder.wire(complete=False)
        self.assertIn(b"A\t1\t1", partial)
        self.assertNotIn(b"E\t1", partial)
        recorder.record(spec, attempt, CLIENT.Client.empty("not-transmitted", "cancelled-before-send"), request="id")
        self.assertTrue(emitted[1].startswith(b"E\t1\t1\t"))
        self.assertLessEqual(384*1024+128+4096+64*1024, 2097152//2)

    def test_exhausted_record_pool_refuses_instead_of_silently_dropping_an_attempt(self):
        value = plan()
        recorder = HISTORY.Recorder(value)
        recorder.emitted = 384*1024
        with self.assertRaisesRegex(ValueError, "record-pool-exhausted"):
            recorder.reserve(value["operations"][0])
        self.assertEqual(recorder.intents, [(1, 1)])
        self.assertEqual(recorder.attempts, 1)

    def test_attempt_ids_partial_restore_and_exact_plan_binding_are_preserved(self):
        value = plan()
        recorder = HISTORY.Recorder(value)
        origin = recorder.started
        for row in value["operations"][:2]:
            result = CLIENT.Client.empty("not-transmitted", "local-refusal")
            recorder.record(row, recorder.reserve(row), result, request="id")
        raw = recorder.wire(complete=False)
        restored = HISTORY.Recorder(value, clock_origin=origin)
        restored.restore(raw)
        self.assertEqual(restored.wire(complete=False), raw)
        with self.assertRaises(ValueError):
            HISTORY.Recorder(value, clock_origin=origin).restore(raw.replace(b"END\t0", b"END\t1"))
        with self.assertRaises(ValueError):
            HISTORY.Recorder(value, clock_origin=origin).restore(raw.replace(b"P\t", b"P\tbad"))

    def test_unknown_resolution_never_falls_back_to_a_latest_read_or_retry(self):
        class NeverNetwork:
            def attempt(self, *_args, **_kwargs):
                raise AssertionError("unproven resolution must not send or read latest")
        client = CLIENT.Client(plan(), NeverNetwork())
        spec = next(r for r in client.plan["operations"] if r["action"] == "resolve")
        event = client.call(spec)
        self.assertEqual(event["boundary"], "not-transmitted")
        self.assertEqual(event["error"], "original-version-not-uniquely-resolved")
        self.assertEqual(client.recorder.attempts, 1)


class ControllerControls(unittest.TestCase):
    def test_monitor_footer_does_not_erase_untrusted_extra_child_data(self):
        body = b"HPCAP002\nEND\t0\t0\n"
        footer = {"exit": 0, "output_bytes": len(body)}
        raw = body+b"\nHPMONITOR2\t"+json.dumps(footer).encode()
        self.assertEqual(CONTROL["split_monitor"](raw)[0], body)
        forged = b"forged\nHPMONITOR2\t{}\n"+body
        footer["output_bytes"] = len(forged)
        self.assertEqual(CONTROL["split_monitor"](forged+b"\nHPMONITOR2\t"+json.dumps(footer).encode())[0], forged)
        with self.assertRaises(CONTROL["Refused"]):
            CONTROL["split_monitor"](raw+b"trailing")

    def test_invalid_raw_primary_binding_stays_bounded_before_parser_refusal(self):
        records = []
        result = {"mode": "rustfs-capabilities-before", "container_id": "a"*64,
                  "exit_code": 1, "truncated": False, "execution": {},
                  "raw_streams": {"stdout": b"\xff"*(1024**2), "stderr": b""},
                  "output": "\ufffd"*(1024**2), "output_protocol": "rustfs-raw-streams-v1",
                  "encoding": "invalid-utf8-retained", "stderr_bytes": 0}
        context = CONTROL["ProfileContext"](source_callback=lambda: {"image": "fixture", "platform": 1},
            append_callback=lambda kind, value, **_kw: records.append((kind, value)),
            run_callback=lambda _mode: None, inputs_callback=lambda *_args: None,
            restart_callback=lambda: None, released_callback=lambda: True, budgets={"PROBE": {}})
        with self.assertRaisesRegex(CONTROL["Refused"], "raw-output-protocol"):
            context.capture(result, result["mode"])
        self.assertEqual(records[0][0], "rustfs-primary-outcome")
        self.assertLess(len(json.dumps(records).encode()), 4096)
        self.assertEqual(records[0][1]["output_binding"]["streams"]["stdout"]["bytes"], 1024**2)

    def test_live_parser_rejects_legacy_replacement_or_nonempty_stderr_after_retention(self):
        for fields in ({}, {"output_protocol": "rustfs-raw-streams-v1", "encoding": "invalid-utf8-retained", "stderr_bytes": 0},
                       {"output_protocol": "rustfs-raw-streams-v1", "encoding": "utf-8-strict", "stderr_bytes": 1}):
            result = dict(truncated=False, exit_code=1, **fields)
            with self.subTest(fields=fields), self.assertRaisesRegex(CONTROL["Refused"], "raw-output-protocol"):
                CONTROL["check_execution"](result, image="fixture", budget={}, platform=1)

    def test_input_publication_follows_charged_admission_and_first_failure_stops(self):
        calls = []
        def append(*_args, **kwargs):
            calls.append(("admit", kwargs["retained_input_bytes"]))
            raise ValueError("evidence-full")
        context = CONTROL["ProfileContext"](source_callback=lambda: {}, append_callback=append,
                  run_callback=lambda mode: calls.append(("run", mode)),
                  inputs_callback=lambda *_: calls.append(("write", 0)), restart_callback=lambda: None,
                  released_callback=lambda: True, budgets={})
        with self.assertRaises(ValueError):
            context.inputs("probe-checker-build", {"config.json": b"{}"}, None)
        self.assertEqual(calls, [("admit", 2)])
        with self.assertRaises(CONTROL["Refused"]):
            context.run("arbitrary-command")


class DiagnosticTransportModel:
    """Counterfactual contract model for HOST routing only, never RustFS proof."""

    def __init__(self, completion_metadata=True):
        import threading
        self.lock = threading.Lock()
        self.objects, self.uploads, self.identities = {}, {}, {}
        self.calls, self.sequence, self.completion_metadata = [], 0, completion_metadata

    @staticmethod
    def metadata(headers):
        return "|".join(headers.get(k, "") for k in ("x-amz-meta-request", "x-amz-meta-envelope", "x-amz-meta-caller"))

    def accept(self, key, headers, segments):
        request, envelope = headers.get("x-amz-meta-request"), headers.get("x-amz-meta-envelope")
        if request in self.identities:
            original = self.identities[request]
            return (200, original) if original["envelope"] == envelope else (409, None)
        existing = self.objects.get(key, [])
        if headers.get("if-none-match") == "*" and existing:
            return 412, None
        if "if-match" in headers and (not existing or existing[-1]["etag"] != headers["if-match"]):
            return 412, None
        self.sequence += 1
        value = {"etag": '"opaque-tag-'+str(self.sequence)+'"', "version": "opaque/version+="+str(self.sequence),
                 "metadata": self.metadata(headers), "segments": segments, "envelope": envelope}
        self.objects.setdefault(key, []).append(value)
        self.identities[request] = value
        return 200, value

    @staticmethod
    def body_digest(segments, offset, length):
        digest = hashlib.sha256()
        cursor = 0
        for seed, size in segments:
            start, stop = max(offset, cursor), min(offset+length, cursor+size)
            if start < stop:
                for chunk in TRANSPORT.payload(seed, stop-start, start-cursor):
                    digest.update(chunk)
            cursor += size
        return digest.hexdigest()

    def attempt(self, method, bucket, key, query, headers, **options):
        from xml.etree import ElementTree
        from xml.sax.saxutils import escape
        barrier = options.get("barrier")
        if barrier is not None:
            barrier.wait(timeout=10)
        start = time.monotonic()
        body = b""
        selected = None
        status = 200
        head_length = None
        query = dict(query)
        with self.lock:
            self.calls.append((method, key, query, dict(headers)))
            if not key and method == "PUT":
                pass  # Fresh bucket/versioning setup, no provider claim.
            elif method == "GET" and not key and "versions" in query:
                versions = self.objects.get(query["prefix"], [])
                body = ("<ListVersionsResult><IsTruncated>false</IsTruncated>"+"".join(
                    "<Version><Key>"+escape(query["prefix"])+"</Key><VersionId>"+escape(v["version"])+"</VersionId></Version>"
                    for v in reversed(versions))+"</ListVersionsResult>").encode()
            elif method == "POST" and "uploads" in query:
                upload = "opaque-upload-"+str(len(self.calls))
                self.uploads[upload] = {"key": key, "parts": {}, "headers": dict(headers)}
                body = ("<InitiateMultipartUploadResult><UploadId>"+upload+"</UploadId></InitiateMultipartUploadResult>").encode()
            elif not key and "uploads" in query:
                body = ("<ListMultipartUploadsResult><IsTruncated>false</IsTruncated>"+"".join(
                    "<Upload><Key>"+escape(v["key"])+"</Key><UploadId>"+escape(ident)+"</UploadId></Upload>"
                    for ident,v in self.uploads.items() if v["key"].startswith(query["prefix"]))+"</ListMultipartUploadsResult>").encode()
            elif "uploadId" in query:
                upload = self.uploads.get(query["uploadId"])
                if upload is None:
                    status = 404
                elif method == "PUT":
                    number = int(query["partNumber"])
                    tag = '"part-'+str(len(self.calls))+'"'
                    upload["parts"][number] = (tag, options["seed"], options["length"])
                    selected = {"etag": tag, "version": "", "metadata": ""}
                elif method == "DELETE":
                    del self.uploads[query["uploadId"]]
                    status = 204
                elif method == "GET":
                    body = ("<ListPartsResult><IsTruncated>false</IsTruncated>"+"".join(
                        "<Part><PartNumber>"+str(n)+"</PartNumber><ETag>"+escape(v[0])+"</ETag><Size>"+str(v[2])+"</Size></Part>"
                        for n,v in sorted(upload["parts"].items()))+"</ListPartsResult>").encode()
                elif method == "POST":
                    descriptor = ElementTree.fromstring(options["body"])
                    parts = [(int(p.findtext("PartNumber")), p.findtext("ETag")) for p in descriptor.findall("Part")]
                    if [n for n,_ in parts] != sorted(upload["parts"]) or any(upload["parts"][n][0] != tag for n,tag in parts):
                        status = 400
                    else:
                        stored = headers if self.completion_metadata else upload["headers"]
                        status, selected = self.accept(key, stored, [(upload["parts"][n][1], upload["parts"][n][2]) for n,_ in parts])
                        if status == 200:
                            del self.uploads[query["uploadId"]]
                            body = ("<CompleteMultipartUploadResult><ETag>"+escape(selected["etag"])+"</ETag></CompleteMultipartUploadResult>").encode()
            elif method == "PUT":
                status, selected = self.accept(key, headers, [(options["seed"], options["length"])])
            elif method in ("HEAD", "GET"):
                values = self.objects.get(key, [])
                selected = next((v for v in values if v["version"] == query["versionId"]), None) if "versionId" in query else (values[-1] if values else None)
                if selected is None:
                    status = 404
                elif method == "HEAD":
                    head_length = sum(n for _,n in selected["segments"])
                else:
                    length = sum(n for _,n in selected["segments"])
                    offset = 0
                    if "range" in headers:
                        first,last = headers["range"][6:].split("-")
                        offset, length = int(first), int(last)-int(first)+1
                        status = 206 if offset+length <= sum(n for _,n in selected["segments"]) else 416
                    if status != 416:
                        response_digest = self.body_digest(selected["segments"], offset, length)
                    else:
                        length = 0
            else:
                raise AssertionError("unplanned model request")
        # Ensure synthetic request intervals overlap, while explicitly making
        # no claim that these are real network or provider measurements.
        if barrier is not None:
            time.sleep(.01)
        response_bytes = length if method == "GET" and selected is not None and "uploadId" not in query else len(body)
        digest = response_digest if method == "GET" and selected is not None and status != 416 and "uploadId" not in query else hashlib.sha256(body).hexdigest()
        result = {"status": status, "boundary": "reply-suppressed" if options.get("suppress") else "reply",
                  "start": start, "end": time.monotonic(), "request_bytes": 64+options.get("length", 0),
                  "response_bytes": response_bytes, "response_wire_bytes": response_bytes+64,
                  "etag": selected["etag"] if selected else "", "version": selected["version"] if selected else "",
                  "metadata": selected["metadata"] if selected else "", "digest": digest,
                  "head_length": head_length, "body": body, "error": ""}
        if options.get("disconnect"):
            result.update(status=0, boundary="after-transmit-before-reply", etag="", version="", metadata="",
                          response_bytes=0, response_wire_bytes=0, body=b"", digest=hashlib.sha256(b"").hexdigest(), error="response-not-observed")
        return result


class CompleteClientFlowControls(unittest.TestCase):
    def capture(self, *, completion_metadata):
        model = DiagnosticTransportModel(completion_metadata)
        first = CLIENT.Client(plan(), model)
        partial = first.run("before-restart")
        second = CLIENT.Client(plan(), model, recorder=HISTORY.Recorder(plan(), clock_origin=first.recorder.started))
        second.restore(partial)
        receipt = {side+"_"+name: ("1" if side == "before" and name == "boot" else "2" if name == "boot" else "3")*64
                   for side in ("before", "after") for name in ("boot", "cid", "config", "image", "volume")}
        final = second.run("after-restart", restart_receipt=receipt)
        return model, first, second, partial, final

    def test_complete_counterfactual_flow_records_every_operation_and_retains_original_resolution(self):
        model, first, second, partial, final = self.capture(completion_metadata=True)
        self.assertEqual(set(second.results), set(range(1, 155)))
        self.assertEqual(second.recorder.attempts, 154)
        self.assertLessEqual(len(final), 384*1024+128)
        self.assertTrue(final.endswith(b"END\t1\t154\n"))
        self.assertLess(len(partial), len(final))
        errors = [e["error"] for e in second.results.values() if e["error"] not in ("", "response-not-observed")]
        self.assertEqual(errors, [])
        for spec in second.plan["operations"]:
            if spec["action"] == "resolve":
                event = second.results[spec["id"]]
                self.assertEqual(event["status"], 200)
                self.assertEqual(event["request"], second.results[spec["parent"]]["request"])
                self.assertTrue(event["selected"])
        self.assertEqual(model.uploads, {})

    def test_init_only_metadata_is_a_completed_negative_not_completion_descriptor_proof(self):
        _model, _first, second, _partial, final = self.capture(completion_metadata=False)
        completion = next(s for s in second.plan["operations"] if s["action"] == "complete" and s["fault"] == "suppress")
        resolution = next(s for s in second.plan["operations"] if s["action"] == "resolve" and s["parent"] == completion["id"])
        event = second.results[resolution["id"]]
        self.assertEqual(event["status"], 200)
        self.assertGreater(event["received"], 0)
        self.assertEqual(event["error"], "completion-envelope-not-provider-bound")
        self.assertTrue(final.endswith(b"END\t1\t154\n"))


class ProspectiveProfileControls(unittest.TestCase):
    def test_plan_freezes_concurrency_criterion_and_rejects_substitution(self):
        value = plan()
        self.assertEqual(value["profile"], "rustfs-capabilities-v3")
        self.assertEqual(value["race_witness"], {"clients": 16, "minimum_started_before_first_completion": 8,
                                                "intervals": "half-open"})
        for field, changed in (("clients", 8), ("minimum_started_before_first_completion", 7),
                               ("intervals", "closed")):
            altered = dict(value, race_witness=dict(value["race_witness"]))
            altered["race_witness"][field] = changed
            with self.assertRaisesRegex(ValueError, "plan-not-fixed-matrix"):
                HISTORY.validate_plan(altered)
        altered = dict(value, profile="rustfs-capabilities-v2")
        with self.assertRaisesRegex(ValueError, "plan-not-fixed-matrix"):
            HISTORY.validate_plan(altered)


if __name__ == "__main__":
    unittest.main()
