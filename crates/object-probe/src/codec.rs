//! Bounded binary record codec, not an object format or integrity attestation.
//!
//! Every mandatory tag, extent, count and trailing byte is checked. Artifact
//! hashes, secret redaction and actual recorder truth are separate obligations.
use crate::model::*;
use std::fmt;

const MAGIC: &[u8; 8] = b"HPROBE01";
const END: &[u8; 8] = b"HEND0001";

/// Bounded codec refusal without opaque input in diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Missing bytes, including a missing final marker.
    Truncated,
    /// Byte/count limit exceeded before allocation/copy.
    Bound,
    /// Mandatory version/tag/framing invalid.
    Format,
    /// Capture binding differs from independently admitted identities.
    Binding,
    /// Extra bytes/records after the final marker.
    Trailing,
    /// Record shape invalid.
    Structural(Invalid),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

struct Writer {
    bytes: Vec<u8>,
    cap: usize,
}
impl Writer {
    fn new(cap: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(cap),
            cap,
        }
    }
    fn put(&mut self, raw: &[u8]) -> Result<(), Error> {
        if self
            .bytes
            .len()
            .checked_add(raw.len())
            .is_none_or(|n| n > self.cap)
        {
            return Err(Error::Bound);
        }
        self.bytes.extend_from_slice(raw);
        Ok(())
    }
    fn u8(&mut self, x: u8) -> Result<(), Error> {
        self.put(&[x])
    }
    fn flag(&mut self, x: bool) -> Result<(), Error> {
        self.u8(u8::from(x))
    }
    fn u16(&mut self, x: u16) -> Result<(), Error> {
        self.put(&x.to_le_bytes())
    }
    fn count(&mut self, x: usize) -> Result<(), Error> {
        self.u16(u16::try_from(x).map_err(|_| Error::Bound)?)
    }
    fn u32(&mut self, x: u32) -> Result<(), Error> {
        self.put(&x.to_le_bytes())
    }
    fn u64(&mut self, x: u64) -> Result<(), Error> {
        self.put(&x.to_le_bytes())
    }
    fn option(&mut self, x: Option<u16>) -> Result<(), Error> {
        self.flag(x.is_some())?;
        if let Some(x) = x {
            self.u16(x)?;
        }
        Ok(())
    }
    fn stamp(&mut self, x: Stamp) -> Result<(), Error> {
        self.u32(x.order)?;
        self.u64(x.micros)
    }
    fn span(&mut self, x: Span) -> Result<(), Error> {
        self.stamp(x.start)?;
        self.flag(x.end.is_some())?;
        if let Some(end) = x.end {
            self.stamp(end)?;
        }
        Ok(())
    }
    fn object(&mut self, x: ObjectId) -> Result<(), Error> {
        self.u16(x.bucket)?;
        self.u16(x.key)
    }
    fn payload(&mut self, x: Payload) -> Result<(), Error> {
        self.u64(x.length)?;
        self.put(&x.digest)
    }
    fn version(&mut self, x: Version) -> Result<(), Error> {
        self.u16(x.token)?;
        self.payload(x.payload)?;
        self.u16(x.metadata)
    }
    fn mutation(&mut self, x: Mutation) -> Result<(), Error> {
        self.u16(x.request)?;
        self.object(x.object)?;
        self.payload(x.payload)?;
        self.u16(x.metadata)?;
        match x.precondition {
            Precondition::Absent => self.u8(0),
            Precondition::Matches(t) => {
                self.u8(1)?;
                self.u16(t)
            }
        }
    }
    fn outcome(&mut self, x: Outcome) -> Result<(), Error> {
        match x {
            Outcome::Receipt(r) => {
                self.u8(0)?;
                self.u16(r.request)?;
                self.version(r.version)
            }
            Outcome::Head(v) => {
                self.u8(1)?;
                self.version(v)
            }
            Outcome::Range {
                token,
                length,
                digest,
            } => {
                self.u8(2)?;
                self.u16(token)?;
                self.u64(length)?;
                self.put(&digest)
            }
            Outcome::Conflict => self.u8(3),
            Outcome::NotFound => self.u8(4),
            Outcome::InvalidRange => self.u8(5),
            Outcome::IdentityConflict => self.u8(6),
            Outcome::UnknownOutcome => self.u8(7),
            Outcome::ResolutionUnavailable => self.u8(8),
            Outcome::Unsupported => self.u8(9),
            Outcome::BeforeSendRefusal => self.u8(10),
        }
    }
    fn operation(&mut self, o: Operation) -> Result<(), Error> {
        self.u16(o.id)?;
        self.u8(o.client)?;
        self.u32(o.generation)?;
        self.option(o.race)?;
        self.option(o.continuation_of)?;
        self.span(o.span)?;
        match o.action {
            Action::Write(m) => {
                self.u8(0)?;
                self.mutation(m)?;
            }
            Action::Head(k) => {
                self.u8(1)?;
                self.object(k)?;
            }
            Action::ReadRange {
                object,
                token,
                offset,
                length,
            } => {
                self.u8(2)?;
                self.object(object)?;
                self.u16(token)?;
                self.u64(offset)?;
                self.u64(length)?;
            }
            Action::Resolve(m) => {
                self.u8(3)?;
                self.mutation(m)?;
            }
        }
        self.flag(o.outcome.is_some())?;
        if let Some(x) = o.outcome {
            self.outcome(x)?;
        }
        Ok(())
    }
    fn attempt(&mut self, a: Attempt) -> Result<(), Error> {
        self.u16(a.id)?;
        self.u16(a.operation)?;
        self.span(a.span)?;
        self.u8(match a.boundary {
            Boundary::NotTransmitted => 0,
            Boundary::ReplyReceived => 1,
            Boundary::AfterTransmitBeforeReply => 2,
            Boundary::ReplySuppressed => 3,
            Boundary::DuringTransmit => 4,
        })?;
        self.option(a.status)?;
        self.u64(a.request_bytes)?;
        self.u64(a.response_bytes)?;
        self.option(a.error)
    }
}

struct Reader<'a> {
    raw: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.offset.checked_add(n).ok_or(Error::Bound)?;
        let value = self.raw.get(self.offset..end).ok_or(Error::Truncated)?;
        self.offset = end;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn flag(&mut self) -> Result<bool, Error> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Format),
        }
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().map_err(|_| Error::Format)?,
        ))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| Error::Format)?,
        ))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().map_err(|_| Error::Format)?,
        ))
    }
    fn count(&mut self, cap: usize) -> Result<usize, Error> {
        let n = usize::from(self.u16()?);
        if n > cap { Err(Error::Bound) } else { Ok(n) }
    }
    fn option(&mut self) -> Result<Option<u16>, Error> {
        if self.flag()? {
            Ok(Some(self.u16()?))
        } else {
            Ok(None)
        }
    }
    fn digest(&mut self) -> Result<Digest, Error> {
        self.take(32)?.try_into().map_err(|_| Error::Format)
    }
    fn stamp(&mut self) -> Result<Stamp, Error> {
        Ok(Stamp {
            order: self.u32()?,
            micros: self.u64()?,
        })
    }
    fn span(&mut self) -> Result<Span, Error> {
        Ok(Span {
            start: self.stamp()?,
            end: if self.flag()? {
                Some(self.stamp()?)
            } else {
                None
            },
        })
    }
    fn object(&mut self) -> Result<ObjectId, Error> {
        Ok(ObjectId {
            bucket: self.u16()?,
            key: self.u16()?,
        })
    }
    fn payload(&mut self) -> Result<Payload, Error> {
        Ok(Payload {
            length: self.u64()?,
            digest: self.digest()?,
        })
    }
    fn version(&mut self) -> Result<Version, Error> {
        Ok(Version {
            token: self.u16()?,
            payload: self.payload()?,
            metadata: self.u16()?,
        })
    }
    fn mutation(&mut self) -> Result<Mutation, Error> {
        let request = self.u16()?;
        let object = self.object()?;
        let payload = self.payload()?;
        let metadata = self.u16()?;
        let precondition = match self.u8()? {
            0 => Precondition::Absent,
            1 => Precondition::Matches(self.u16()?),
            _ => return Err(Error::Format),
        };
        Ok(Mutation {
            request,
            object,
            payload,
            metadata,
            precondition,
        })
    }
    fn outcome(&mut self) -> Result<Outcome, Error> {
        Ok(match self.u8()? {
            0 => Outcome::Receipt(Receipt {
                request: self.u16()?,
                version: self.version()?,
            }),
            1 => Outcome::Head(self.version()?),
            2 => Outcome::Range {
                token: self.u16()?,
                length: self.u64()?,
                digest: self.digest()?,
            },
            3 => Outcome::Conflict,
            4 => Outcome::NotFound,
            5 => Outcome::InvalidRange,
            6 => Outcome::IdentityConflict,
            7 => Outcome::UnknownOutcome,
            8 => Outcome::ResolutionUnavailable,
            9 => Outcome::Unsupported,
            10 => Outcome::BeforeSendRefusal,
            _ => return Err(Error::Format),
        })
    }
    fn operation(&mut self) -> Result<Operation, Error> {
        let id = self.u16()?;
        let client = self.u8()?;
        let generation = self.u32()?;
        let race = self.option()?;
        let continuation_of = self.option()?;
        let span = self.span()?;
        let action = match self.u8()? {
            0 => Action::Write(self.mutation()?),
            1 => Action::Head(self.object()?),
            2 => Action::ReadRange {
                object: self.object()?,
                token: self.u16()?,
                offset: self.u64()?,
                length: self.u64()?,
            },
            3 => Action::Resolve(self.mutation()?),
            _ => return Err(Error::Format),
        };
        let outcome = if self.flag()? {
            Some(self.outcome()?)
        } else {
            None
        };
        Ok(Operation {
            id,
            client,
            generation,
            race,
            continuation_of,
            span,
            action,
            outcome,
        })
    }
    fn attempt(&mut self) -> Result<Attempt, Error> {
        let id = self.u16()?;
        let operation = self.u16()?;
        let span = self.span()?;
        let boundary = match self.u8()? {
            0 => Boundary::NotTransmitted,
            1 => Boundary::ReplyReceived,
            2 => Boundary::AfterTransmitBeforeReply,
            3 => Boundary::ReplySuppressed,
            4 => Boundary::DuringTransmit,
            _ => return Err(Error::Format),
        };
        Ok(Attempt {
            id,
            operation,
            span,
            boundary,
            status: self.option()?,
            request_bytes: self.u64()?,
            response_bytes: self.u64()?,
            error: self.option()?,
        })
    }
    fn record(&mut self, cap: usize) -> Result<Self, Error> {
        let size = self.count(cap - 2)?;
        Ok(Self {
            raw: self.take(size)?,
            offset: 0,
        })
    }
    fn finish(self) -> Result<(), Error> {
        if self.offset == self.raw.len() {
            Ok(())
        } else {
            Err(Error::Trailing)
        }
    }
}

/// Encode bounded records; no hashing, semantic pass or provider evidence implied.
pub fn encode(history: &History) -> Result<Vec<u8>, Error> {
    history.validate().map_err(Error::Structural)?;
    let mut w = Writer::new(WIRE_BYTES);
    w.put(MAGIC)?;
    w.u16(1)?;
    for digest in &history.binding.digests {
        w.put(digest)?;
    }
    w.put(&history.binding.run)?;
    w.u8(history.binding.platform)?;
    w.u8(match history.scenario {
        Scenario::Create => 0,
        Scenario::Update => 1,
        Scenario::ResponseLoss => 2,
        Scenario::Range => 3,
        Scenario::Restart => 4,
        Scenario::Multipart => 5,
    })?;
    w.flag(history.complete)?;
    w.u16(history.declared_operations)?;
    w.u16(history.declared_attempts)?;
    w.u16(history.physical_attempts)?;
    w.count(history.atoms.len())?;
    w.count(history.operations.len())?;
    w.count(history.attempts.len())?;
    w.count(history.errors.len())?;
    w.count(history.races.len())?;
    w.u32(u32::try_from(history.scenario_receipt.len()).map_err(|_| Error::Bound)?)?;
    w.put(&history.scenario_receipt)?;
    for race in &history.races {
        w.u16(race.id)?;
        w.stamp(race.released)?;
    }
    if w.bytes.len() + END.len() > RECEIPT_BYTES {
        return Err(Error::Bound);
    }
    for atom in &history.atoms {
        w.u8(match atom.kind {
            AtomKind::Bucket => 0,
            AtomKind::Key => 1,
            AtomKind::Request => 2,
            AtomKind::Token => 3,
            AtomKind::Metadata => 4,
        })?;
        w.count(atom.bytes.len())?;
        w.put(&atom.bytes)?;
    }
    let records = w.bytes.len();
    for op in &history.operations {
        let mut record = Writer::new(766);
        record.operation(*op)?;
        w.count(record.bytes.len())?;
        w.put(&record.bytes)?;
    }
    for attempt in &history.attempts {
        let mut record = Writer::new(382);
        record.attempt(*attempt)?;
        w.count(record.bytes.len())?;
        w.put(&record.bytes)?;
    }
    if w.bytes.len() - records > RECORD_BYTES {
        return Err(Error::Bound);
    }
    for error in &history.errors {
        w.count(error.len())?;
        w.put(error)?;
    }
    w.put(END)?;
    Ok(w.bytes)
}

/// Decode only a bounded complete wire extent with an independently known binding.
/// Incomplete logical captures can decode, but cannot pass the semantic checker.
pub fn decode(raw: &[u8], expected: &Binding) -> Result<History, Error> {
    if raw.len() > WIRE_BYTES {
        return Err(Error::Bound);
    }
    let mut r = Reader { raw, offset: 0 };
    if r.take(8)? != MAGIC || r.u16()? != 1 {
        return Err(Error::Format);
    }
    let mut digests = [[0; 32]; 7];
    for digest in &mut digests {
        *digest = r.digest()?;
    }
    let run = r.take(16)?.try_into().map_err(|_| Error::Format)?;
    let binding = Binding {
        digests,
        run,
        platform: r.u8()?,
    };
    if &binding != expected {
        return Err(Error::Binding);
    }
    let scenario = match r.u8()? {
        0 => Scenario::Create,
        1 => Scenario::Update,
        2 => Scenario::ResponseLoss,
        3 => Scenario::Range,
        4 => Scenario::Restart,
        5 => Scenario::Multipart,
        _ => return Err(Error::Format),
    };
    let complete = r.flag()?;
    let declared_operations = r.u16()?;
    let declared_attempts = r.u16()?;
    let physical_attempts = r.u16()?;
    if usize::from(declared_operations) > MAX_OPERATIONS
        || usize::from(declared_attempts) > MAX_ATTEMPTS
        || physical_attempts > declared_attempts
    {
        return Err(Error::Bound);
    }
    let atoms_count = r.count(MAX_ATOMS)?;
    let ops_count = r.count(MAX_OPERATIONS)?;
    let attempts_count = r.count(MAX_ATTEMPTS)?;
    let errors_count = r.count(MAX_ATTEMPTS)?;
    let races_count = r.count(16)?;
    let receipt_size = usize::try_from(r.u32()?).map_err(|_| Error::Bound)?;
    if receipt_size > RECEIPT_BYTES - 512
        || r.offset
            .checked_add(receipt_size + races_count * 14 + 8)
            .is_none_or(|n| n > RECEIPT_BYTES)
    {
        return Err(Error::Bound);
    }
    let scenario_receipt = r.take(receipt_size)?.to_vec();
    let mut races = Vec::with_capacity(races_count);
    for _ in 0..races_count {
        races.push(Race {
            id: r.u16()?,
            released: r.stamp()?,
        });
    }
    let mut atoms = Vec::with_capacity(atoms_count);
    let mut bytes = 0usize;
    for _ in 0..atoms_count {
        let kind = match r.u8()? {
            0 => AtomKind::Bucket,
            1 => AtomKind::Key,
            2 => AtomKind::Request,
            3 => AtomKind::Token,
            4 => AtomKind::Metadata,
            _ => return Err(Error::Format),
        };
        let size = r.count(kind.bound())?;
        bytes = bytes.checked_add(size + 3).ok_or(Error::Bound)?;
        if bytes > DICTIONARY_BYTES {
            return Err(Error::Bound);
        }
        atoms.push(Atom {
            kind,
            bytes: r.take(size)?.to_vec(),
        });
    }
    let records_start = r.offset;
    let mut operations = Vec::with_capacity(ops_count);
    for _ in 0..ops_count {
        let mut record = r.record(768)?;
        let op = record.operation()?;
        record.finish()?;
        operations.push(op);
        if r.offset - records_start > RECORD_BYTES {
            return Err(Error::Bound);
        }
    }
    let mut attempts = Vec::with_capacity(attempts_count);
    for _ in 0..attempts_count {
        let mut record = r.record(384)?;
        let attempt = record.attempt()?;
        record.finish()?;
        attempts.push(attempt);
        if r.offset - records_start > RECORD_BYTES {
            return Err(Error::Bound);
        }
    }
    let mut errors = Vec::with_capacity(errors_count);
    let mut bytes = 0usize;
    for _ in 0..errors_count {
        let size = r.count(4096)?;
        bytes = bytes.checked_add(size + 2).ok_or(Error::Bound)?;
        if bytes > ERROR_BYTES {
            return Err(Error::Bound);
        }
        errors.push(r.take(size)?.to_vec());
    }
    if r.take(8)? != END {
        return Err(Error::Format);
    }
    r.finish()?;
    let history = History {
        binding,
        scenario,
        complete,
        declared_operations,
        declared_attempts,
        physical_attempts,
        atoms,
        operations,
        attempts,
        races,
        errors,
        scenario_receipt,
    };
    history.validate().map_err(Error::Structural)?;
    Ok(history)
}

/// Diagnostic SHA-256 (FIPS180-4), used only for independent recorded-byte
/// oracles. This is not a production cryptographic provider.
pub fn sha256(raw: &[u8]) -> Digest {
    let mut h = Hash::new();
    h.update(raw);
    h.finish()
}
struct Hash {
    state: [u32; 8],
    block: [u8; 64],
    used: usize,
    bytes: u64,
}
impl Hash {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            used: 0,
            bytes: 0,
        }
    }
    fn compress(&mut self) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut w = [0u32; 64];
        for (i, chunk) in self.block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let x = w[i - 15];
            let y = w[i - 2];
            w[i] = w[i - 16]
                .wrapping_add(x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3))
                .wrapping_add(w[i - 7])
                .wrapping_add(y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10));
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let t = h
                .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
                .wrapping_add((e & f) ^ (!e & g))
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let u = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                .wrapping_add((a & b) ^ (a & c) ^ (b & c));
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t);
            d = c;
            c = b;
            b = a;
            a = t.wrapping_add(u);
        }
        for (x, y) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *x = x.wrapping_add(y);
        }
    }
    fn update(&mut self, raw: &[u8]) {
        self.bytes += raw.len() as u64;
        for byte in raw {
            self.block[self.used] = *byte;
            self.used += 1;
            if self.used == 64 {
                self.compress();
                self.used = 0;
            }
        }
    }
    fn finish(mut self) -> Digest {
        let bits = self.bytes * 8;
        self.update(&[128]);
        while self.used != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut result = [0; 32];
        for (target, value) in result.chunks_exact_mut(4).zip(self.state) {
            target.copy_from_slice(&value.to_be_bytes());
        }
        result
    }
}
/// Independent selected-byte oracle; each segment is (seed, length, offset).
/// Multipart segments are hashed in declared completion order, never via ETag.
pub fn payload_digest(segments: &[(u64, u64, u64)]) -> Result<Digest, Error> {
    if segments.len() > 16 {
        return Err(Error::Bound);
    }
    let mut h = Hash::new();
    let mut total = 0u64;
    for &(seed, length, offset) in segments {
        total = total.checked_add(length).ok_or(Error::Bound)?;
        if total > 16 * 1024 * 1024
            || offset
                .checked_add(length)
                .is_none_or(|n| n > 16 * 1024 * 1024)
        {
            return Err(Error::Bound);
        }
        let mut chunk = [0u8; 16384];
        let mut done = 0;
        while done < length {
            let n = (length - done).min(chunk.len() as u64) as usize;
            let mut block = [0u8; 8];
            for (i, b) in chunk[..n].iter_mut().enumerate() {
                let position = offset + done + i as u64;
                if i == 0 || position.is_multiple_of(8) {
                    let mut value =
                        seed.wrapping_add((position / 8 + 1).wrapping_mul(0x9e3779b97f4a7c15));
                    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
                    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
                    value ^= value >> 31;
                    block = value.to_le_bytes();
                }
                *b = block[(position % 8) as usize];
            }
            h.update(&chunk[..n]);
            done += n as u64;
        }
    }
    Ok(h.finish())
}
/// Bounded lowercase hex admission before allocation.
pub fn unhex(raw: &str, cap: usize) -> Result<Vec<u8>, Error> {
    if !raw.len().is_multiple_of(2)
        || raw.len() / 2 > cap
        || !raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Format);
    }
    let digit = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
    Ok(raw
        .as_bytes()
        .chunks_exact(2)
        .map(|p| digit(p[0]) * 16 + digit(p[1]))
        .collect())
}
fn digest_hex(raw: &str) -> Result<Digest, Error> {
    unhex(raw, 32)?.try_into().map_err(|_| Error::Format)
}
fn text_hex(raw: &str, cap: usize) -> Result<String, Error> {
    String::from_utf8(unhex(raw, cap)?).map_err(|_| Error::Format)
}
fn number<T: std::str::FromStr>(raw: &str) -> Result<T, Error> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Format);
    }
    raw.parse().map_err(|_| Error::Bound)
}
fn lines(raw: &[u8], cap: usize) -> Result<Vec<&str>, Error> {
    if raw.len() > cap || !raw.is_ascii() || !raw.ends_with(b"\n") {
        return Err(Error::Bound);
    }
    let value = std::str::from_utf8(raw).map_err(|_| Error::Format)?;
    let mut result = Vec::new();
    for line in value.lines() {
        if line.len() > 20000 || result.len() > 1024 {
            return Err(Error::Bound);
        }
        result.push(line);
    }
    Ok(result)
}
/// Decode version two independently frozen requests. Legacy binary decoding is
/// unchanged and never interprets this extension as old restart eligibility.
pub fn decode_plan_v2(raw: &[u8]) -> Result<CapturePlan, Error> {
    let lines = lines(raw, 64 * 1024)?;
    if lines.first() != Some(&"HPPLAN02") || lines.last() != Some(&"END") || lines.len() < 4 {
        return Err(Error::Format);
    }
    let b: Vec<_> = lines[1].split('\t').collect();
    if b.len() != 11 || b[0] != "B" {
        return Err(Error::Format);
    }
    let mut digests = [[0; 32]; 7];
    for (out, value) in digests.iter_mut().zip(&b[1..8]) {
        *out = digest_hex(value)?;
    }
    let run = unhex(b[8], 16)?.try_into().map_err(|_| Error::Format)?;
    let platform = number(b[9])?;
    if !matches!(platform, 1 | 2) {
        return Err(Error::Binding);
    }
    let bucket = text_hex(b[10], 63)?;
    if bucket.len() < 3
        || !bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(Error::Format);
    }
    let mut specifications = Vec::new();
    for line in &lines[2..lines.len() - 1] {
        let f: Vec<_> = line.split('\t').collect();
        if f.len() != 13 || f[0] != "S" || specifications.len() == MAX_OPERATIONS {
            return Err(Error::Format);
        }
        let s = Specification {
            id: number(f[1])?,
            scenario: number(f[2])?,
            client: number(f[3])?,
            action: f[4].into(),
            key: text_hex(f[5], 1024)?,
            parent: number(f[6])?,
            seed: number(f[7])?,
            length: number(f[8])?,
            offset: number(f[9])?,
            range_length: number(f[10])?,
            race: number(f[11])?,
            fault: f[12].into(),
        };
        if usize::from(s.id) != specifications.len() + 1
            || s.parent >= s.id
            || s.client >= 16
            || s.scenario > 5
            || s.offset > 16 * 1024 * 1024
            || s.length > 16 * 1024 * 1024
            || s.range_length > 16 * 1024 * 1024
            || s.race > 16
            || !["none", "suppress", "disconnect"].contains(&s.fault.as_str())
            || ![
                "create",
                "update",
                "resolve",
                "range",
                "head",
                "restart",
                "init",
                "part",
                "complete",
                "abort",
                "parts",
                "uploads",
                "bucket",
                "versioning",
                "versions",
                "resolve-head",
                "replay",
                "stale-update",
                "reuse-id",
                "complete-invalid",
                "race-head",
                "race-range",
            ]
            .contains(&s.action.as_str())
        {
            return Err(Error::Format);
        }
        specifications.push(s);
    }
    Ok(CapturePlan {
        binding: Binding {
            digests,
            run,
            platform,
        },
        bucket,
        specifications,
    })
}
/// Decode captured attempts with finite field/count/aggregate extents.
pub fn decode_capture_v2(raw: &[u8], expected_plan: Digest) -> Result<Capture, Error> {
    let lines = lines(raw, WIRE_BYTES)?;
    if lines.first() != Some(&"HPCAP002") || lines.len() < 3 {
        return Err(Error::Format);
    }
    let p: Vec<_> = lines[1].split('\t').collect();
    if p.len() != 2 || p[0] != "P" || digest_hex(p[1])? != expected_plan {
        return Err(Error::Binding);
    }
    let end: Vec<_> = lines.last().ok_or(Error::Truncated)?.split('\t').collect();
    if end.len() != 3 || end[0] != "END" || !["0", "1"].contains(&end[1]) {
        return Err(Error::Truncated);
    }
    let attempts = number(end[2])?;
    if usize::from(attempts) > MAX_ATTEMPTS {
        return Err(Error::Bound);
    }
    let mut events = Vec::new();
    let mut intents = Vec::new();
    let mut bytes = 0;
    for line in &lines[2..lines.len() - 1] {
        bytes += line.len();
        if bytes > RECORD_BYTES || events.len() == MAX_OPERATIONS {
            return Err(Error::Bound);
        }
        let f: Vec<_> = line.split('\t').collect();
        if f.len() == 3 && f[0] == "A" {
            let id: u16 = number(f[1])?;
            let attempt: u16 = number(f[2])?;
            if id == 0
                || usize::from(id) > MAX_OPERATIONS
                || attempt == 0
                || attempt > attempts
                || intents.contains(&(id, attempt))
                || intents.len() == MAX_ATTEMPTS
            {
                return Err(Error::Format);
            }
            intents.push((id, attempt));
            continue;
        }
        if f.len() != 20 || f[0] != "E" {
            return Err(Error::Format);
        }
        let e = CaptureEvent {
            id: number(f[1])?,
            attempt: number(f[2])?,
            start: number(f[3])?,
            end: number(f[4])?,
            status: number(f[5])?,
            boundary: f[6].into(),
            sent: number(f[7])?,
            received: number(f[8])?,
            wire_received: number(f[9])?,
            etag: text_hex(f[10], 256)?,
            version: text_hex(f[11], 256)?,
            metadata: text_hex(f[12], 256)?,
            request: text_hex(f[13], 128)?,
            condition: text_hex(f[14], 256)?,
            selected: text_hex(f[15], 256)?,
            auxiliary: text_hex(f[16], 8192)?,
            digest: digest_hex(f[17])?,
            error: text_hex(f[18], 64)?,
            head_length: if f[19] == "-" {
                None
            } else {
                Some(number(f[19])?)
            },
        };
        if e.id == 0
            || e.start > e.end
            || e.end > HISTORY_MICROS
            || e.attempt == 0
            || e.sent > 17 * 1024 * 1024
            || e.received > 16 * 1024 * 1024
            || e.wire_received > 17 * 1024 * 1024
            || e.wire_received < e.received
            || ![
                "not-transmitted",
                "during-transmit",
                "after-transmit-before-reply",
                "reply",
                "reply-suppressed",
                "not-applicable",
                "unimplemented",
                "host-restart",
            ]
            .contains(&e.boundary.as_str())
        {
            return Err(Error::Format);
        }
        events.push(e);
    }
    if intents.len() != usize::from(attempts)
        || events.iter().any(|e| !intents.contains(&(e.id, e.attempt)))
    {
        return Err(Error::Format);
    }
    Ok(Capture {
        plan_digest: expected_plan,
        events,
        complete: end[1] == "1",
        attempts,
    })
}
