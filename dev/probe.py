#!/usr/bin/env python3
"""Bounded SigV4/TLS harness smoke, not an object-backend conformance suite."""
from datetime import datetime, timezone
import hashlib
import hmac
import json
from pathlib import Path
import re
import ssl
import sys
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen
from execute import verify_limits

ENDPOINT = "https://rustfs:9000"


def sign(method, path, payload, access, secret, stamp):
    day = stamp[:8]
    digest = hashlib.sha256(payload).hexdigest()
    headers = {"Host": "rustfs:9000", "x-amz-content-sha256": digest, "x-amz-date": stamp}
    canonical_headers = f"host:rustfs:9000\nx-amz-content-sha256:{digest}\nx-amz-date:{stamp}\n"
    signed_headers = "host;x-amz-content-sha256;x-amz-date"
    canonical = "\n".join((method, path, "", canonical_headers, signed_headers, digest))
    scope = f"{day}/us-east-1/s3/aws4_request"
    message = f"AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{hashlib.sha256(canonical.encode()).hexdigest()}"
    key = ("AWS4" + secret).encode()
    for value in (day, "us-east-1", "s3", "aws4_request"):
        key = hmac.new(key, value.encode(), hashlib.sha256).digest()
    signature = hmac.new(key, message.encode(), hashlib.sha256).hexdigest()
    headers["Authorization"] = f"AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    return headers


def request(method, path, body=b"", *, wrong_ca=False, wrong_secret=False):
    access = Path("/run/secrets/access").read_text().strip()
    secret = "wrong-development-key" if wrong_secret else Path("/run/secrets/secret").read_text().strip()
    context = ssl.create_default_context(cafile="/certs/wrong-ca.crt" if wrong_ca else "/certs/ca.crt")
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    req = Request(ENDPOINT + path, data=body if method in ("PUT", "POST") else None, method=method,
                  headers=sign(method, path, body, access, secret, stamp))
    with urlopen(req, context=context, timeout=10) as response:
        result = response.read(4097)
        if len(result) > 4096:
            raise RuntimeError("Smoke response exceeded the byte bound")
        return result


def main(bucket):
    verify_limits(500, 256 * 1024**2, 64)
    if not re.fullmatch(r"hsearch-[a-z0-9-]{1,54}", bucket):
        raise ValueError("Invalid isolated bucket name")
    try:
        request("GET", "/health/ready", wrong_ca=True)
    except URLError as error:
        if not isinstance(error.reason, ssl.SSLCertVerificationError):
            raise RuntimeError("Wrong-CA check failed for a non-certificate reason") from error
    else:
        raise RuntimeError("Untrusted CA was accepted")
    try:
        request("GET", "/", wrong_secret=True)
    except HTTPError as error:
        if error.code != 403:
            raise RuntimeError("Unexpected credential rejection status") from error
    else:
        raise RuntimeError("Invalid development credential was accepted")
    try:
        request("PUT", "/" + bucket)
    except HTTPError as error:
        from xml.etree import ElementTree
        body = error.read(4097)
        if error.code != 409 or len(body) > 4096 or ElementTree.fromstring(body).findtext("Code") != "BucketAlreadyOwnedByYou":
            raise RuntimeError("Unexpected bucket creation result") from error
    payload = ("isolated:" + bucket).encode()
    request("PUT", "/" + bucket + "/smoke", payload)
    if request("GET", "/" + bucket + "/smoke") != payload:
        raise RuntimeError("S3 smoke round trip mismatch")
    print(json.dumps({"bucket": bucket, "tls_verified": True, "wrong_ca_rejected": True,
                      "bad_credential_rejected": True, "round_trip": "pass", "scope": "local harness smoke only"}))


if __name__ == "__main__":
    main(sys.argv[1])
