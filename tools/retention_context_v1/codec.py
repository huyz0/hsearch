"""Bounded canonical JSON for the component, independent of review envelopes."""
import hashlib
import json

MAX_BYTES = 1048576
MAX_DEPTH = 24
MAX_NODES = 50000


class Refused(ValueError):
    """No further effect or accepted completion is permitted."""


def need(ok, reason):
    if not ok:
        raise Refused(reason)


def integer(value, low=0, high=2**63-1):
    need(type(value) is int and low <= value <= high, 'integer-bound')
    return value


def fields(value, names):
    need(type(value) is dict and set(value) == set(names.split()), 'schema-fields')


def pin(value):
    need(type(value) is str and len(value) == 64 and
         all(c in '0123456789abcdef' for c in value), 'identity-pin')
    return value


def bounded(value):
    nodes = 0
    string_characters = 0

    def walk(item, depth):
        nonlocal nodes, string_characters
        nodes += 1
        need(nodes <= MAX_NODES and depth <= MAX_DEPTH, 'object-bound')
        if type(item) is dict:
            for key, child in item.items():
                need(type(key) is str, 'object-key')
                walk(key, depth+1)
                walk(child, depth+1)
        elif type(item) is list:
            for child in item:
                walk(child, depth+1)
        elif type(item) is str:
            # Every canonical string contributes at least its character count
            # in UTF-8 bytes. Refuse aggregate oversized graphs before scanning
            # each character or asking the encoder to allocate their output.
            string_characters += len(item)
            need(string_characters <= MAX_BYTES, 'aggregate-string-bound')
            need(len(item) <= MAX_BYTES and not any(0xD800 <= ord(c) <= 0xDFFF for c in item), 'string-bound')
        elif type(item) is int:
            integer(item, -(2**63))
        else:
            need(item is None or type(item) is bool, 'object-type')
    walk(value, 0)


def encode(value):
    bounded(value)
    chunks, used = [], 0
    for part in json.JSONEncoder(sort_keys=True, separators=(',', ':'), ensure_ascii=False).iterencode(value):
        raw = part.encode('utf-8')
        used += len(raw)
        need(used <= MAX_BYTES, 'encoded-byte-bound')
        chunks.append(raw)
    return b''.join(chunks)


def decode(raw):
    need(type(raw) is bytes and 0 < len(raw) <= MAX_BYTES, 'decode-byte-bound')
    # Bound lexical container nesting BEFORE json.loads allocates its graph.
    depth, quoted, escape = 0, False, False
    for byte in raw:
        if quoted:
            if escape:
                escape = False
            elif byte == 92:
                escape = True
            elif byte == 34:
                quoted = False
        elif byte == 34:
            quoted = True
        elif byte in (91, 123):
            depth += 1
            need(depth <= MAX_DEPTH, 'lexical-depth')
        elif byte in (93, 125):
            depth -= 1
            need(depth >= 0, 'lexical-depth')
    need(depth == 0 and not quoted, 'lexical-framing')

    def pairs(items):
        result = {}
        for key, value in items:
            need(key not in result, 'duplicate-key')
            result[key] = value
        return result

    def no_float(_):
        raise Refused('numeric-type')

    def number(text):
        need(len(text) <= 20, 'integer-width')
        return integer(int(text), -(2**63))

    try:
        value = json.loads(raw.decode('utf-8'), object_pairs_hook=pairs,
                           parse_int=number, parse_float=no_float, parse_constant=no_float)
    except (UnicodeError, ValueError, RecursionError) as exc:
        raise Refused('invalid-json') from exc
    need(encode(value) == raw, 'noncanonical-json')
    return value


def digest(value):
    return hashlib.sha256(encode(value)).hexdigest()
