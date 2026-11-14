"""Reference encoder for the extension wire format (docs/plan/extension-modules.md section 6)."""
import struct


def u32(n):
    return struct.pack("<I", n)


def i32(n):
    return struct.pack("<i", n)


def i64(n):
    return struct.pack("<q", n)


def string(s):
    b = s.encode("utf-8")
    return u32(len(b)) + b


def value(v):
    tag, payload = v
    if tag == "string":
        return b"\x01" + string(payload)
    if tag == "integer":
        return b"\x02" + i64(payload)
    if tag == "boolean":
        return b"\x03" + (b"\x01" if payload else b"\x00")
    if tag == "date":
        return b"\x04" + i64(payload)
    raise ValueError(tag)


def tokens(items):
    return u32(len(items)) + b"".join(string(t) + i32(p) + i32(s) + i32(e) for t, p, s, e in items)


def query(node):
    kind = node["op"]
    if kind == "all":
        return b"\x01"
    if kind == "id":
        return b"\x02" + string(node["id"])
    if kind == "term":
        return b"\x03" + string(node["field"]) + value(node["value"])
    if kind == "terms":
        return b"\x04" + string(node["field"]) + u32(len(node["values"])) + b"".join(value(v) for v in node["values"])
    if kind == "range":
        return b"\x05" + string(node["field"]) + value(node["lower"]) + value(node["upper"])
    if kind == "match":
        return b"\x06" + string(node["field"]) + string(node["text"]) + (b"\x01" if node["phrase"] else b"\x00")
    if kind == "bool":
        out = b"\x07" + u32(node["minimum_should_match"])
        for role in ("must", "filter", "should", "must_not"):
            out += u32(len(node[role])) + b"".join(query(c) for c in node[role])
        return out
    raise ValueError(kind)


def query_input(body, schema):
    return string(body) + u32(len(schema)) + b"".join(string(n) + bytes([k]) for n, k in schema)


def ingest_docs(docs):
    out = u32(len(docs))
    for fields in docs:
        out += u32(len(fields)) + b"".join(string(n) + value(v) for n, v in fields)
    return out


def ingest_result(results):
    out = u32(len(results))
    for r in results:
        if r[0] == "keep":
            out += b"\x00" + u32(len(r[1])) + b"".join(string(n) + value(v) for n, v in r[1])
        elif r[0] == "drop":
            out += b"\x01"
        else:
            out += b"\x02" + string(r[1])
    return out


def scoring_input(rows):
    f = len(rows[0]) if rows else 0
    return u32(len(rows)) + u32(f) + b"".join(struct.pack("<f", x) for r in rows for x in r)


def scoring_output(scores):
    return b"".join(struct.pack("<f", x) for x in scores)


def encode_vector(entry):
    s, d = entry["structure"], entry["data"]
    if s == "tokens":
        return tokens([tuple(t) for t in d])
    if s == "query_input":
        return query_input(d["body"], [tuple(x) for x in d["schema"]])
    if s == "query":
        return query(d)
    if s == "ingest_docs":
        return ingest_docs([[(n, tuple(val)) for n, val in doc] for doc in d])
    if s == "ingest_result":
        return ingest_result([(r[0], [(n, tuple(val)) for n, val in r[1]]) if r[0] == "keep" else tuple(r) for r in d])
    if s == "scoring_input":
        return scoring_input(d)
    if s == "scoring_output":
        return scoring_output(d)
    raise ValueError(s)
