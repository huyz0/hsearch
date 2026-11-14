//! Safe, bounded reader of an ELF image's `DT_NEEDED` entries.
//!
//! The hash pins the module file only (section 3), so the loader also refuses a
//! library that pulls in anything beyond libc, libm and the compiler runtime:
//! a needed library resolved from the system search path is code the hash does
//! not cover. The scan reads the staged image through [`ReadAt`] in small fixed
//! reads and allocates by constants here, never by a length in the file.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

/// Positioned read, so the scan runs over a sealed file or a test buffer alike.
pub trait ReadAt {
    /// Fills `buf` from `offset` or fails if the image is shorter.
    fn read_exact_at_offset(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
}

impl ReadAt for File {
    fn read_exact_at_offset(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.read_exact_at(buf, offset)
    }
}

impl ReadAt for [u8] {
    fn read_exact_at_offset(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let short = || io::Error::from(io::ErrorKind::UnexpectedEof);
        let start = usize::try_from(offset).map_err(|_| short())?;
        let end = start.checked_add(buf.len()).ok_or_else(short)?;
        buf.copy_from_slice(self.get(start..end).ok_or_else(short)?);
        Ok(())
    }
}

const MAX_PROGRAM_HEADERS: usize = 128;
const MAX_DYNAMIC_BYTES: u64 = 64 * 1024;
const MAX_NEEDED: usize = 64;
const MAX_NAME_BYTES: usize = 256;
const PHDR_SIZE: usize = 56;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_STRTAB: i64 = 5;
const DT_STRSZ: i64 = 10;
const DT_RPATH: i64 = 15;
const DT_RUNPATH: i64 = 29;
const DT_DEPAUDIT: i64 = 0x6000_000c;
const DT_AUDIT: i64 = 0x6000_000d;
const DT_AUXILIARY: i64 = 0x7fff_fffd;
const DT_FILTER: i64 = 0x7fff_ffff;

/// Why an image was not scanned to the end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfError {
    /// Not a 64-bit little-endian shared object.
    NotSharedObject,
    /// A table is out of range, truncated or over a fixed bound.
    Malformed,
    /// The image names a library search path, audit library or filter, all of
    /// which resolve code the pinned hash does not cover.
    SearchPath,
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap_or([0; 4]))
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or([0; 8]))
}

struct Load {
    vaddr: u64,
    offset: u64,
    filesz: u64,
}

/// The `DT_NEEDED` names of a 64-bit little-endian shared object, in order.
///
/// # Errors
/// [`ElfError`] when the image is not that, or any table lies outside the image
/// or over its bound (128 program headers, 64 KiB of dynamic section, 64 names
/// of at most 256 bytes).
pub fn needed<R: ReadAt + ?Sized>(image: &R) -> Result<Vec<String>, ElfError> {
    let mut eh = [0u8; 64];
    image
        .read_exact_at_offset(&mut eh, 0)
        .map_err(|_| ElfError::NotSharedObject)?;
    // ELFCLASS64, ELFDATA2LSB, EV_CURRENT, ET_DYN.
    if eh[..4] != *b"\x7fELF" || eh[4] != 2 || eh[5] != 1 || eh[6] != 1 || le16(&eh, 16) != 3 {
        return Err(ElfError::NotSharedObject);
    }
    let (phoff, phentsize, phnum) = (
        le64(&eh, 32),
        usize::from(le16(&eh, 54)),
        usize::from(le16(&eh, 56)),
    );
    if phentsize != PHDR_SIZE || phnum == 0 || phnum > MAX_PROGRAM_HEADERS {
        return Err(ElfError::Malformed);
    }
    let mut table = vec![0u8; phnum * PHDR_SIZE];
    image
        .read_exact_at_offset(&mut table, phoff)
        .map_err(|_| ElfError::Malformed)?;
    let (mut loads, mut dynamic) = (Vec::new(), None);
    for ph in table.chunks_exact(PHDR_SIZE) {
        match le32(ph, 0) {
            PT_LOAD => loads.push(Load {
                vaddr: le64(ph, 16),
                offset: le64(ph, 8),
                filesz: le64(ph, 32),
            }),
            PT_DYNAMIC if dynamic.is_none() => dynamic = Some((le64(ph, 8), le64(ph, 32))),
            _ => {}
        }
    }
    let (dyn_off, dyn_size) = dynamic.ok_or(ElfError::Malformed)?;
    if dyn_size > MAX_DYNAMIC_BYTES {
        return Err(ElfError::Malformed);
    }
    let mut section = vec![0u8; usize::try_from(dyn_size).map_err(|_| ElfError::Malformed)?];
    image
        .read_exact_at_offset(&mut section, dyn_off)
        .map_err(|_| ElfError::Malformed)?;
    let (mut offsets, mut strtab, mut strsz) = (Vec::new(), None, 0u64);
    for entry in section.chunks_exact(16) {
        let (tag, val) = (
            i64::from_le_bytes(entry[..8].try_into().unwrap_or([0; 8])),
            le64(entry, 8),
        );
        match tag {
            DT_NULL => break,
            DT_NEEDED => offsets.push(val),
            DT_STRTAB => strtab = Some(val),
            DT_STRSZ => strsz = val,
            DT_RPATH | DT_RUNPATH | DT_DEPAUDIT | DT_AUDIT | DT_AUXILIARY | DT_FILTER => {
                return Err(ElfError::SearchPath);
            }
            _ => {}
        }
    }
    if offsets.len() > MAX_NEEDED {
        return Err(ElfError::Malformed);
    }
    if offsets.is_empty() {
        return Ok(Vec::new());
    }
    let strtab = strtab.ok_or(ElfError::Malformed)?;
    let base = loads
        .iter()
        .find(|l| strtab >= l.vaddr && strtab - l.vaddr < l.filesz)
        .and_then(|l| l.offset.checked_add(strtab - l.vaddr))
        .ok_or(ElfError::Malformed)?;
    offsets
        .into_iter()
        .map(|rel| name_at(image, base, strsz, rel))
        .collect()
}

fn name_at<R: ReadAt + ?Sized>(
    image: &R,
    base: u64,
    strsz: u64,
    rel: u64,
) -> Result<String, ElfError> {
    if rel >= strsz {
        return Err(ElfError::Malformed);
    }
    let room = usize::try_from((strsz - rel).min(MAX_NAME_BYTES as u64))
        .map_err(|_| ElfError::Malformed)?;
    let at = base.checked_add(rel).ok_or(ElfError::Malformed)?;
    let mut buf = [0u8; MAX_NAME_BYTES];
    // The string table may end the file, so read the whole window or fewer bytes.
    let mut got = room;
    while got > 0 && image.read_exact_at_offset(&mut buf[..got], at).is_err() {
        got -= 1;
    }
    let end = buf[..got]
        .iter()
        .position(|b| *b == 0)
        .ok_or(ElfError::Malformed)?;
    String::from_utf8(buf[..end].to_vec()).map_err(|_| ElfError::Malformed)
}

/// True for a library the loader allows a module to need: libc, libm, the
/// glibc split libraries older toolchains still list, the compiler runtime and
/// the dynamic loader. Everything else is code the pinned hash does not cover.
/// A name with a slash is a path to the dynamic loader, never a library name.
#[must_use]
pub fn allowed_dependency(name: &str) -> bool {
    const EXACT: [&str; 7] = [
        "libc.so.6",
        "libm.so.6",
        "libgcc_s.so.1",
        "libdl.so.2",
        "libpthread.so.0",
        "librt.so.1",
        "libutil.so.1",
    ];
    const LOADERS: [&str; 5] = [
        "ld-linux-x86-64.so.2",
        "ld-linux-aarch64.so.1",
        "ld-linux-armhf.so.3",
        "ld-musl-x86_64.so.1",
        "ld-musl-aarch64.so.1",
    ];
    const MUSL_LIBC: [&str; 2] = ["libc.musl-x86_64.so.1", "libc.musl-aarch64.so.1"];
    EXACT.contains(&name) || LOADERS.contains(&name) || MUSL_LIBC.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid image: header, three program headers (LOAD, DYNAMIC and a
    /// filler), a dynamic section and a string table, laid out by hand so every
    /// field a test mutates is one this builder wrote.
    pub(crate) fn image(names: &[&str]) -> Vec<u8> {
        let mut strtab = vec![0u8];
        let mut name_offsets = Vec::new();
        for n in names {
            name_offsets.push(strtab.len() as u64);
            strtab.extend_from_slice(n.as_bytes());
            strtab.push(0);
        }
        let ph_off = 64usize;
        let dyn_off = ph_off + 2 * PHDR_SIZE;
        let dyn_len = (names.len() + 3) * 16;
        let str_off = dyn_off + dyn_len;
        let total = str_off + strtab.len();
        let mut b = vec![0u8; total];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[6] = 1;
        b[16..18].copy_from_slice(&3u16.to_le_bytes());
        b[32..40].copy_from_slice(&(ph_off as u64).to_le_bytes());
        b[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
        b[56..58].copy_from_slice(&2u16.to_le_bytes());
        // PT_LOAD maps the whole file at virtual address 0x1000.
        let ph = &mut b[ph_off..ph_off + PHDR_SIZE];
        ph[..4].copy_from_slice(&PT_LOAD.to_le_bytes());
        ph[8..16].copy_from_slice(&0u64.to_le_bytes());
        ph[16..24].copy_from_slice(&0x1000u64.to_le_bytes());
        ph[32..40].copy_from_slice(&(total as u64).to_le_bytes());
        let ph = &mut b[ph_off + PHDR_SIZE..ph_off + 2 * PHDR_SIZE];
        ph[..4].copy_from_slice(&PT_DYNAMIC.to_le_bytes());
        ph[8..16].copy_from_slice(&(dyn_off as u64).to_le_bytes());
        ph[32..40].copy_from_slice(&(dyn_len as u64).to_le_bytes());
        let mut entries: Vec<(i64, u64)> = name_offsets.iter().map(|o| (DT_NEEDED, *o)).collect();
        entries.push((DT_STRTAB, 0x1000 + str_off as u64));
        entries.push((DT_STRSZ, strtab.len() as u64));
        entries.push((DT_NULL, 0));
        for (i, (tag, val)) in entries.iter().enumerate() {
            let at = dyn_off + i * 16;
            b[at..at + 8].copy_from_slice(&tag.to_le_bytes());
            b[at + 8..at + 16].copy_from_slice(&val.to_le_bytes());
        }
        b[str_off..].copy_from_slice(&strtab);
        b
    }

    #[test]
    fn reads_needed_names_in_order() {
        let bytes = image(&["libc.so.6", "libm.so.6"]);
        assert_eq!(
            needed(bytes.as_slice()).expect("scan"),
            ["libc.so.6", "libm.so.6"]
        );
        assert_eq!(
            needed(image(&[]).as_slice()).expect("scan"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn refuses_what_is_not_a_64_bit_le_shared_object() {
        let good = image(&["libc.so.6"]);
        for (at, value) in [(0usize, 0u8), (4, 1), (5, 2), (6, 2), (16, 2)] {
            let mut bad = good.clone();
            bad[at] = value;
            assert_eq!(
                needed(bad.as_slice()),
                Err(ElfError::NotSharedObject),
                "byte {at}"
            );
        }
        assert_eq!(needed(&[0u8; 10][..]), Err(ElfError::NotSharedObject));
    }

    #[test]
    fn refuses_tables_out_of_range_or_over_a_bound() {
        let good = image(&["libc.so.6"]);
        let patch = |at: usize, bytes: &[u8]| {
            let mut v = good.clone();
            v[at..at + bytes.len()].copy_from_slice(bytes);
            v
        };
        // phentsize, phnum zero and over the bound, phoff past the end.
        for bad in [
            patch(54, &[0, 0]),
            patch(56, &[0, 0]),
            patch(56, &[129, 0]),
            patch(32, &u64::MAX.to_le_bytes()),
        ] {
            assert_eq!(needed(bad.as_slice()), Err(ElfError::Malformed));
        }
        let dyn_ph = 64 + PHDR_SIZE;
        // dynamic section over its bound, and past the end of the file.
        assert_eq!(
            needed(patch(dyn_ph + 32, &(MAX_DYNAMIC_BYTES + 1).to_le_bytes()).as_slice()),
            Err(ElfError::Malformed)
        );
        assert_eq!(
            needed(patch(dyn_ph + 8, &u64::MAX.to_le_bytes()).as_slice()),
            Err(ElfError::Malformed)
        );
        // no dynamic header at all: turn it into a second PT_LOAD.
        assert_eq!(
            needed(patch(dyn_ph, &PT_LOAD.to_le_bytes()).as_slice()),
            Err(ElfError::Malformed)
        );
        // string table address outside every PT_LOAD, and a name offset outside the table.
        let dyn_off = 64 + 2 * PHDR_SIZE;
        assert_eq!(
            needed(patch(dyn_off + 16 + 8, &0x9_0000u64.to_le_bytes()).as_slice()),
            Err(ElfError::Malformed)
        );
        assert_eq!(
            needed(patch(dyn_off + 8, &1000u64.to_le_bytes()).as_slice()),
            Err(ElfError::Malformed)
        );
    }

    #[test]
    fn refuses_more_names_than_the_bound_and_unterminated_names() {
        let many: Vec<String> = (0..=MAX_NEEDED).map(|i| format!("lib{i}.so")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(needed(image(&refs).as_slice()), Err(ElfError::Malformed));
        let mut bytes = image(&["libc.so.6"]);
        // Drop the final NUL so the name runs to the end of the table.
        let last = bytes.len() - 1;
        bytes[last] = b'x';
        assert_eq!(needed(bytes.as_slice()), Err(ElfError::Malformed));
        let long = "a".repeat(MAX_NAME_BYTES + 5);
        assert_eq!(
            needed(image(&[long.as_str()]).as_slice()),
            Err(ElfError::Malformed)
        );
    }

    #[test]
    fn refuses_search_paths_audit_libraries_and_filters() {
        // Append a dynamic entry of the given tag in place of DT_NULL's slot: the
        // builder reserves three trailing entries (STRTAB, STRSZ, NULL), so patch STRSZ's tag.
        for tag in [
            DT_RPATH,
            DT_RUNPATH,
            DT_AUDIT,
            DT_DEPAUDIT,
            DT_AUXILIARY,
            DT_FILTER,
        ] {
            let mut bytes = image(&["libc.so.6"]);
            let dyn_off = 64 + 2 * PHDR_SIZE;
            // entries: NEEDED, STRTAB, STRSZ, NULL. Turn the NULL into the tag and add no terminator
            // after it by relying on the section ending there.
            bytes[dyn_off + 3 * 16..dyn_off + 3 * 16 + 8].copy_from_slice(&tag.to_le_bytes());
            assert_eq!(
                needed(bytes.as_slice()),
                Err(ElfError::SearchPath),
                "tag {tag:#x}"
            );
        }
    }

    #[test]
    fn dependency_allow_list() {
        for ok in [
            "libc.so.6",
            "libm.so.6",
            "libgcc_s.so.1",
            "ld-linux-x86-64.so.2",
            "ld-linux-aarch64.so.1",
            "ld-musl-x86_64.so.1",
            "libc.musl-x86_64.so.1",
        ] {
            assert!(allowed_dependency(ok), "{ok}");
        }
        for bad in [
            "libssl.so.3",
            "libstdc++.so.6",
            "libz.so.1",
            "/tmp/libevil.so",
            "libc.so",
            "",
            "libcx.so.6",
        ] {
            assert!(!allowed_dependency(bad), "{bad}");
        }
    }
}
