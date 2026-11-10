"""Bounded diagnostic SigV4 transport. No SDK, redirect or implicit retry.

This module never returns credentials, authorization or provider error text.
Each call performs one physical attempt and reports its actual send boundary.
"""
from __future__ import annotations

import datetime
import hashlib
import hmac
import http.client
import re
import socket
import ssl
import time
from urllib.parse import quote, urlsplit
from xml.parsers import expat

HEADER_BYTES = 16 * 1024
XML_BYTES = 16 * 1024
ERROR_BYTES = 4096
CHUNK = 16 * 1024
MAX_BODY = 16 * 1024 * 1024
MAX_ATTEMPTS = 512


class _HeaderReader:
    def __init__(self, source, wire_limit):
        self.source, self.used, self.lines, self.headers = source, 0, 0, True
        self.total, self.wire_limit = 0, wire_limit

    def read(self, size=-1):
        remaining = self.wire_limit - self.total
        need(remaining >= 0, "response-wire-bound")
        raw = self.source.read(min(size, remaining+1) if size >= 0 else remaining+1)
        self.total += len(raw)
        need(self.total <= self.wire_limit, "response-wire-bound")
        return raw

    def readinto(self, buffer):
        raw = self.read(len(buffer))
        buffer[:len(raw)] = raw
        return len(raw)

    def readline(self, limit=-1):
        if not self.headers:
            raw = self.source.readline(min(1024, limit) if limit >= 0 else 1024)
            self.total += len(raw)
            need(self.total <= self.wire_limit and raw.endswith(b"\n"), "response-wire-bound")
            return raw
        remaining = HEADER_BYTES - self.used
        need(remaining > 0 and self.lines < 66, "response-headers-bound")
        raw = self.source.readline(min(remaining+1, limit) if limit >= 0 else remaining+1)
        self.used += len(raw)
        self.total += len(raw)
        self.lines += 1
        need(self.used <= HEADER_BYTES and raw.endswith(b"\n"), "response-headers-bound")
        if raw in (b"\r\n", b"\n"):
            self.headers = False
        return raw

    def __getattr__(self, name):
        return getattr(self.source, name)


class _Response(http.client.HTTPResponse):
    def __init__(self, *args, wire_limit, **kwargs):
        super().__init__(*args, **kwargs)
        self.reader = _HeaderReader(self.fp, wire_limit)
        self.fp = self.reader

    def _read_status(self):
        version, status, reason = super()._read_status()
        need(status >= 200, "informational-response-not-admitted")
        return version, status, reason


class Refused(RuntimeError):
    """Stable redacted diagnostic refusal."""


def need(condition, code):
    if not condition:
        raise Refused(code)


def token(value, limit=256):
    need(isinstance(value, str) and 0 < len(value.encode()) <= limit and
         all(32 <= ord(c) < 127 for c in value), "opaque-token-invalid")
    need(not any(marker in value.lower() for marker in ("://", "authorization", "x-amz-credential", "x-amz-signature")),
         "secret-bearing-response")
    return value


def payload(seed, length, offset=0):
    """Generate a bounded chunk stream without allocating the whole object."""
    need(type(seed) is int and 0 <= seed < 2**64 and type(length) is int and
         0 <= length <= MAX_BODY and 0 <= offset <= MAX_BODY and offset+length <= MAX_BODY, "payload-bound")
    done = 0
    while done < length:
        size = min(CHUNK, length - done)
        output = bytearray()
        position, stop = offset+done, offset+done+size
        while position < stop:
            # SplitMix64 counter blocks, not a periodic byte ramp or security key.
            mask = 2**64-1
            value = (seed+(position//8+1)*0x9E3779B97F4A7C15) & mask
            value = ((value^(value>>30))*0xBF58476D1CE4E5B9) & mask
            value = ((value^(value>>27))*0x94D049BB133111EB) & mask
            value ^= value>>31
            start = position % 8
            take = min(8-start, stop-position)
            output.extend(value.to_bytes(8, "little")[start:start+take])
            position += take
        yield bytes(output)
        done += size


def digest_payload(seed, length, offset=0):
    result = hashlib.sha256()
    for chunk in payload(seed, length, offset):
        result.update(chunk)
    return result.hexdigest()


def bounded_xml(raw, *, parents=False):
    """Admit raw bytes, element count/depth and aggregate text before retaining."""
    need(type(raw) is bytes and len(raw) <= XML_BYTES, "xml-byte-bound")
    parser = expat.ParserCreate(namespace_separator="}")
    stack, fields = [], []
    count = total = 0

    def begin(name, attributes):
        nonlocal count
        count += 1
        need(count <= 256 and len(stack) < 12 and not attributes, "xml-shape-bound")
        need(len(name) <= 256, "xml-name-bound")
        stack.append([name.rsplit("}", 1)[-1], []])

    def data(text):
        nonlocal total
        total += len(text.encode())
        need(total <= XML_BYTES, "xml-text-bound")
        if stack:
            stack[-1][1].append(text)

    def end(_name):
        name, chunks = stack.pop()
        fields.append((name, "".join(chunks), stack[-1][0] if stack else "") if parents else (name, "".join(chunks)))

    def forbidden(*_args):
        raise Refused("xml-entity-or-dtd")

    parser.StartElementHandler, parser.CharacterDataHandler, parser.EndElementHandler = begin, data, end
    parser.StartDoctypeDeclHandler = forbidden
    parser.EntityDeclHandler = forbidden
    parser.ExternalEntityRefHandler = forbidden
    try:
        parser.Parse(raw, True)
    except expat.ExpatError:
        raise Refused("xml-malformed") from None
    return fields


def completion_reply(raw):
    """Classify bounded completion XML without retaining foreign error text."""
    fields = bounded_xml(raw, parents=True)
    need(bool(fields), "completion-result-shape")
    errors = [value for name, value, _ in fields if name == "Error"]
    etags = [value for name, value, _ in fields if name == "ETag"]
    codes = [value for name, value, _ in fields if name == "Code"]
    root = fields[-1][0]
    if root == "Error":
        need(len(raw) <= ERROR_BYTES, "completion-error-body-bound")
        need(len(errors) == 1 and len(codes) == 1 and not etags and
             all((name == "Error" and not parent) or
                 (parent == "Error" and name in {"Code", "Message", "Resource", "RequestId", "HostId", "Key", "BucketName"})
                 for name, _, parent in fields), "completion-error-shape")
        # Only predeclared invalid-descriptor errors can prove this negative.
        # Unknown errors remain unavailable proof, never accepted completion.
        need(codes[0] in ("InvalidPart", "InvalidPartOrder"), "completion-error-code-unsupported")
        return "error", codes[0]
    need(root == "CompleteMultipartUploadResult" and not errors and not codes and len(etags) == 1 and
         sum(name == root for name, _, _ in fields) == 1 and
         all(parent == root for name, _, parent in fields if name != root),
         "completion-result-shape")
    return "success", token(etags[0])


def canonical_query(query):
    need(type(query) is list and len(query) <= 16, "query-count")
    pairs = []
    for key, value in query:
        need(type(key) is str and type(value) is str and len(key) <= 64 and len(value) <= 1024,
             "query-bound")
        pairs.append((quote(key, safe="-_.~"), quote(value, safe="-_.~")))
    return "&".join(k + "=" + v for k, v in sorted(pairs))


def sign(method, path, query, headers, body_digest, access, secret, now):
    """Sign every actual header, including exact conditional headers and query."""
    need(re.fullmatch(r"[A-Za-z0-9]{1,128}", access) is not None and
         isinstance(secret, str) and 1 <= len(secret) <= 1024, "credential-shape")
    need(re.fullmatch(r"[0-9a-f]{64}", body_digest) is not None, "body-digest")
    date = now.strftime("%Y%m%d")
    stamp = now.strftime("%Y%m%dT%H%M%SZ")
    result = dict(headers)
    result.update({"x-amz-date": stamp, "x-amz-content-sha256": body_digest})
    keys = sorted(result)
    need(len(keys) <= 20 and all(k == k.lower() and re.fullmatch(r"[a-z0-9-]{1,64}", k)
                               for k in keys), "signed-header-name")
    need(all(type(v) is str and len(v) <= 2048 and "\r" not in v and "\n" not in v
             for v in result.values()), "signed-header-value")
    canonical = "\n".join((method, path, canonical_query(query),
                            "".join(k + ":" + " ".join(result[k].split()) + "\n" for k in keys),
                            ";".join(keys), body_digest))
    scope = date + "/us-east-1/s3/aws4_request"
    string = "AWS4-HMAC-SHA256\n" + stamp + "\n" + scope + "\n" + hashlib.sha256(canonical.encode()).hexdigest()
    key = ("AWS4" + secret).encode()
    for part in (date, "us-east-1", "s3", "aws4_request"):
        key = hmac.new(key, part.encode(), hashlib.sha256).digest()
    signature = hmac.new(key, string.encode(), hashlib.sha256).hexdigest()
    result["authorization"] = "AWS4-HMAC-SHA256 Credential=" + access + "/" + scope + ", SignedHeaders=" + ";".join(keys) + ", Signature=" + signature
    return result


class Transport:
    """One allowlisted TLS endpoint, one send per call, bounded streamed I/O."""

    def __init__(self, endpoint, ca, access, secret, *,
                 clock=time.monotonic, connection=http.client.HTTPSConnection):
        parsed = urlsplit(endpoint)
        need(parsed.scheme == "https" and parsed.hostname == "rustfs" and
             parsed.port == 9000 and parsed.path in ("", "/") and
             not parsed.query and not parsed.fragment and parsed.username is None,
             "endpoint-not-allowlisted")
        self.context = ssl.create_default_context(cafile=str(ca))
        self.context.minimum_version = ssl.TLSVersion.TLSv1_3
        self.access, self.secret = access, secret
        self.clock, self.connection = clock, connection

    def attempt(self, method, bucket, key, query, headers, *, body=None, seed=0, length=0,
                response_limit=XML_BYTES, timeout=15, barrier=None, suppress=False, disconnect=False, cancel=None):
        need(method in {"GET", "HEAD", "PUT", "POST", "DELETE"}, "method")
        need(re.fullmatch(r"[a-z0-9][a-z0-9-]{2,62}", bucket) is not None and
             type(key) is str and len(key.encode()) <= 1024 and not key.startswith("/"), "object-name")
        need(type(response_limit) is int and 0 <= response_limit <= MAX_BODY and
             0 < timeout <= 15 and type(length) is int and 0 <= length <= MAX_BODY, "io-budget")
        need(body is None or type(body) is bytes and len(body) <= XML_BYTES and len(body) == length, "request-body")
        need(type(headers) is dict and len(headers) <= 12 and
             set(headers) <= {"if-match", "if-none-match", "range", "content-type", "x-amz-meta-request",
                              "x-amz-meta-envelope", "x-amz-meta-caller"}, "header-allowlist")
        path = "/" + bucket + ("/" + quote(key, safe="/-_.~") if key else "")
        digest = hashlib.sha256(body).hexdigest() if body is not None else digest_payload(seed, length)
        supplied = {"host": "rustfs:9000", "content-length": str(length), **headers}
        signed = sign(method, path, query, supplied, digest, self.access, self.secret,
                      datetime.datetime.now(datetime.timezone.utc))
        target = path + ("?" + canonical_query(query) if query else "")
        need(sum(len(k) + len(v) + 4 for k, v in signed.items()) <= HEADER_BYTES, "request-headers-bound")
        connection = self.connection("rustfs", 9000, context=self.context, timeout=timeout)
        responses = []

        def response_factory(*args, **kwargs):
            item = _Response(*args, **kwargs, wire_limit=response_limit+HEADER_BYTES+65536)
            responses.append(item)
            return item

        connection.response_class = response_factory
        start = end = None
        sent = received = 0
        response = None
        status = 0
        boundary = "not-transmitted"
        deadline = self.clock() + timeout
        output = {"etag": "", "version": "", "metadata": "", "content_range": "", "head_length": None}
        collected = bytearray()
        digest_response = hashlib.sha256()

        original_send = connection.send

        def tracked_send(data):
            nonlocal start, sent, boundary
            if start is None:
                start = self.clock()  # Immediately before the first actual HTTP write.
            boundary = "during-transmit"
            original_send(data)
            # A failed sendall can have sent a partial prefix. In that case this
            # remains only a known-written lower bound, with an unknown boundary.
            sent += len(data)

        connection.send = tracked_send
        try:
            connection.connect()  # TLS is established before race release.
            connection.auto_open = 0  # Never reconnect implicitly after admission.
            if barrier is not None:
                barrier.wait(timeout=timeout)
            need(cancel is None or not cancel.is_set(), "cancelled-before-send")
            need(self.clock() < deadline, "deadline-before-send")
            connection.putrequest(method, target, skip_host=True, skip_accept_encoding=True)
            for name, value in signed.items():
                connection.putheader(name, value)
            connection.endheaders()
            for chunk in ([body] if body is not None else payload(seed, length)):
                need(self.clock() < deadline and (cancel is None or not cancel.is_set()), "cancelled-or-deadline")
                connection.sock.settimeout(max(.001, deadline - self.clock()))
                connection.send(chunk)
            boundary = "after-transmit-before-reply"
            if disconnect:
                return {**output, "status": 0, "boundary": boundary, "start": start, "end": self.clock(),
                        "request_bytes": sent, "response_bytes": 0, "response_wire_bytes": 0, "digest": digest_response.hexdigest(),
                        "body": b"", "error": "response-not-observed"}
            connection.sock.settimeout(max(.001, deadline - self.clock()))
            response = connection.getresponse()
            received_headers_at = self.clock()
            status = response.status
            pairs = response.getheaders()
            need(len(pairs) <= 64 and sum(len(k) + len(v) + 4 for k, v in pairs) <= HEADER_BYTES,
                 "response-headers-bound")
            lower = {}
            for name, value in pairs:
                name = name.lower()
                need(name not in lower, "duplicate-response-header")
                lower[name] = value
            need(not 300 <= status < 400 and not lower.get("content-encoding") and
                 lower.get("transfer-encoding", "").lower() in ("", "chunked"), "redirect-or-encoding")
            cap = ERROR_BYTES if status >= 400 else response_limit
            declared = lower.get("content-length")
            if declared is not None:
                need(declared.isdecimal() and (int(declared) <= cap or method == "HEAD" and int(declared) <= MAX_BODY), "response-size")
            if method == "HEAD" and declared is not None:
                output["head_length"] = int(declared)
            end = received_headers_at
            while True:
                if method == "HEAD" or declared is not None and received == int(declared):
                    break
                need(self.clock() < deadline and (cancel is None or not cancel.is_set()), "cancelled-or-deadline")
                if connection.sock is not None:
                    connection.sock.settimeout(max(.001, deadline - self.clock()))
                chunk = response.read(min(CHUNK, cap - received + 1))
                end = self.clock()
                if not chunk:
                    break
                received += len(chunk)
                need(received <= cap, "response-overflow")
                digest_response.update(chunk)
                if cap <= XML_BYTES:
                    collected.extend(chunk)
            if method != "HEAD" and declared is not None:
                need(received == int(declared), "response-short-read")
            boundary = "reply-suppressed" if suppress else "reply"
            sensitive = (self.access.encode(), self.secret.encode(), b"Authorization", b"X-Amz-Credential")
            need(not any(value in bytes(collected) for value in sensitive), "secret-bearing-response")
            recorded_headers = ("etag", "x-amz-version-id", "x-amz-meta-request",
                                "x-amz-meta-envelope", "x-amz-meta-caller", "content-range")
            need(not any(value.decode() in lower.get(name, "") for name in recorded_headers for value in sensitive),
                 "secret-bearing-response")
            for name, dest in [("etag", "etag"), ("x-amz-version-id", "version"),
                               ("x-amz-meta-request", "metadata"), ("content-range", "content_range")]:
                if name in lower:
                    need(not any(value.decode() in lower[name] for value in sensitive), "secret-bearing-response")
                    output[dest] = token(lower[name], 1024 if dest == "content_range" else 256)
            if output["metadata"]:
                envelope = lower.get("x-amz-meta-envelope", "")
                caller = lower.get("x-amz-meta-caller", "")
                need(re.fullmatch(r"[0-9a-f]{64}", envelope) and caller == "probe-caller-v2", "provider-envelope-missing")
                output["metadata"] += "|" + envelope + "|" + caller
            boundary = "reply-suppressed" if suppress else "reply"
            # Suppression is after receiving a complete response. The trusted
            # recorder retains the boundary; callers must still resolve identity.
            return {**output, "status": status, "boundary": boundary, "start": start, "end": end,
                    "request_bytes": sent, "response_bytes": received, "response_wire_bytes": response.reader.total,
                    "digest": digest_response.hexdigest(), "body": bytes(collected), "error": ""}
        except (OSError, http.client.HTTPException, Refused, TimeoutError) as error:
            return {**output, "status": status, "boundary": boundary, "start": start,
                    "end": self.clock(), "request_bytes": sent, "response_bytes": received,
                    "response_wire_bytes": responses[-1].reader.total if responses else 0,
                    "digest": digest_response.hexdigest(), "body": b"",
                    "error": str(error) if isinstance(error, Refused) else "transport-unavailable"}
        finally:
            connection.close()
