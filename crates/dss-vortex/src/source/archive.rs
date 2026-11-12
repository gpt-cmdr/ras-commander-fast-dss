//! The containers PRISM and SNODAS are distributed in: ZIP (PRISM's `*_bil.zip`, `*_asc.zip` and
//! its 2025 grid packages), tar (SNODAS's daily `SNODAS_YYYYMMDD.tar`) and gzip (each SNODAS member).
//!
//! Everything is read in memory and nothing is extracted to disk, so reading never writes beside the
//! input. Vortex 1.1.0's `SnodasTarDataReader.updateTar` does: it makes `<stem>_untar` and
//! `<stem>_unzip` folders next to the tar and writes ENVI headers into the second.
//!
//! Each reader takes only the subset of its format these products use, and refuses the rest with a
//! typed error instead of guessing. Every size comes from the input and is checked against the bytes
//! present and the caller's limit before anything is allocated.
//!
//! Format sources: PKWARE `APPNOTE.TXT` 6.3.10 (sections 4.3.6 to 4.3.16 and 4.4), POSIX.1-2017
//! `pax` "ustar Interchange Format", and RFC 1952 (gzip) with RFC 1951 (deflate). The deflate
//! decoder is `flate2`'s pure-Rust backend, already in this crate's build graph.

use crate::error::VortexError;
use std::io::Read;

fn malformed(format: &'static str, message: impl Into<String>) -> VortexError {
    VortexError::Decode {
        format,
        message: message.into(),
    }
}

fn refused(what: impl Into<String>, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: what.into(),
        reason: reason.into(),
    }
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// How much one read takes, and the most a stated size reserves before any byte is decoded.
const READ_CHUNK: usize = 64 * 1024;

/// A whole file, refused once it holds more than `limit` bytes. The bound is on the bytes actually
/// read, not on the size the file system states: a pipe or device can state 0 and stream for ever
///.
pub(crate) fn read_file_bounded(
    path: &std::path::Path,
    limit: u64,
    what: &str,
) -> Result<Vec<u8>, VortexError> {
    let file = std::fs::File::open(path)?;
    read_stream_bounded(file, limit, what)
}

/// [`read_file_bounded`] on any reader.
pub(crate) fn read_stream_bounded(
    reader: impl Read,
    limit: u64,
    what: &str,
) -> Result<Vec<u8>, VortexError> {
    let mut out: Vec<u8> = Vec::new();
    let mut taken = reader.take(limit.saturating_add(1));
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let n = match taken.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            return Ok(out);
        }
        if out.len() as u64 + n as u64 > limit {
            return Err(VortexError::Refused {
                what: what.to_string(),
                reason: format!("it holds more than the limit of {limit} bytes"),
            });
        }
        out.try_reserve(n).map_err(|_| VortexError::Refused {
            what: what.to_string(),
            reason: "cannot grow the buffer".to_string(),
        })?;
        out.extend_from_slice(&chunk[..n]);
    }
}

/// Read at most `limit` bytes from a decoder, refusing anything longer. The buffer grows with what
/// is decoded, not with what a header claims, and every growth is fallible.
fn read_bounded(
    mut reader: impl Read,
    limit: u64,
    expected: Option<u64>,
    format: &'static str,
) -> Result<Vec<u8>, VortexError> {
    let mut out: Vec<u8> = Vec::new();
    // A stated size reserves at most one read chunk up front: a tiny archive can state a member of
    // up to the input limit, and the buffer must follow what is decoded.
    if let Some(n) = expected.filter(|n| *n <= limit) {
        let first = n.min(READ_CHUNK as u64) as usize;
        out.try_reserve_exact(first).map_err(|_| {
            refused(
                format!("{format} member"),
                format!("cannot hold {first} bytes"),
            )
        })?;
    }
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| malformed(format, e.to_string()))?;
        if n == 0 {
            return Ok(out);
        }
        if out.len() as u64 + n as u64 > limit {
            return Err(refused(
                format!("{format} member"),
                format!("it decodes to more than the limit of {limit} bytes"),
            ));
        }
        out.try_reserve(n)
            .map_err(|_| refused(format!("{format} member"), "cannot grow the buffer"))?;
        out.extend_from_slice(&chunk[..n]);
    }
}

// ---------------------------------------------------------------------------------------------
// ZIP
// ---------------------------------------------------------------------------------------------

const ZIP: &str = "ZIP";
/// APPNOTE 4.3.16: end of central directory record.
const EOCD_SIGNATURE: u32 = 0x0605_4b50;
const EOCD_LEN: usize = 22;
/// APPNOTE 4.3.12: central directory file header.
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const CENTRAL_LEN: usize = 46;
/// APPNOTE 4.3.7: local file header.
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const LOCAL_LEN: usize = 30;
/// More entries than any PRISM package holds (8) by far.
const MAX_ZIP_ENTRIES: usize = 4096;

/// Whether bytes start as a ZIP archive's first local header does.
pub(crate) fn is_zip(head: &[u8]) -> bool {
    head.starts_with(&LOCAL_SIGNATURE.to_le_bytes())
}

/// One member of a ZIP archive, from its central directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZipEntry {
    pub name: String,
    method: u16,
    crc32: u32,
    compressed: u64,
    pub size: u64,
    local_offset: u64,
}

impl ZipEntry {
    /// A member in the archive's root folder, as GDAL's `ReadDir` on `/vsizip/<archive>` lists it.
    pub fn in_root(&self) -> bool {
        !self.name.contains('/') && !self.name.contains('\\') && !self.name.is_empty()
    }
}

/// The central directory, in its stored order. Refused: archives split over disks, ZIP64,
/// encryption, and any compression but stored (0) and deflate (8). A member whose name is not UTF-8
/// and not flagged as such is read as CP437 bytes only where they are ASCII; otherwise refused.
pub(crate) fn zip_entries(data: &[u8]) -> Result<Vec<ZipEntry>, VortexError> {
    // APPNOTE 4.3.16: the record is the last thing in the file, followed by a comment of at most
    // 65,535 bytes. The last matching signature whose comment length reaches the end is the record.
    let lowest = data.len().saturating_sub(EOCD_LEN + u16::MAX as usize);
    let mut eocd = None;
    let mut at = data.len().checked_sub(EOCD_LEN);
    while let Some(i) = at {
        if i < lowest {
            break;
        }
        if u32_at(data, i) == Some(EOCD_SIGNATURE) {
            let comment = u16_at(data, i + 20).unwrap_or(u16::MAX) as usize;
            if i + EOCD_LEN + comment == data.len() {
                eocd = Some(i);
                break;
            }
        }
        at = i.checked_sub(1);
    }
    let eocd = eocd.ok_or_else(|| malformed(ZIP, "no end of central directory record"))?;
    let field16 = |o: usize| u16_at(data, eocd + o).unwrap_or(0);
    let field32 = |o: usize| u32_at(data, eocd + o).unwrap_or(0);
    let (disk, cd_disk) = (field16(4), field16(6));
    let (on_disk, total) = (field16(8), field16(10));
    let (cd_size, cd_offset) = (field32(12), field32(16));
    if disk != 0 || cd_disk != 0 || on_disk != total {
        return Err(refused("ZIP archive", "it is split over several disks"));
    }
    if total == u16::MAX || cd_size == u32::MAX || cd_offset == u32::MAX {
        return Err(refused("ZIP archive", "ZIP64 is not read"));
    }
    let total = total as usize;
    if total > MAX_ZIP_ENTRIES {
        return Err(refused(
            "ZIP archive",
            format!("{total} members is over the limit of {MAX_ZIP_ENTRIES}"),
        ));
    }
    let start = cd_offset as usize;
    let end = start
        .checked_add(cd_size as usize)
        .filter(|e| *e <= eocd)
        .ok_or_else(|| malformed(ZIP, "the central directory is outside the file"))?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(total)
        .map_err(|_| refused("ZIP archive", "cannot hold its directory"))?;
    let mut at = start;
    for index in 0..total {
        if at + CENTRAL_LEN > end || u32_at(data, at) != Some(CENTRAL_SIGNATURE) {
            return Err(malformed(
                ZIP,
                format!("central directory entry {index} is not where the directory says"),
            ));
        }
        let h16 = |o: usize| u16_at(data, at + o).unwrap_or(0);
        let h32 = |o: usize| u32_at(data, at + o).unwrap_or(0);
        let flags = h16(8);
        let method = h16(10);
        let crc32 = h32(16);
        let compressed = h32(20);
        let size = h32(24);
        let (name_len, extra_len, comment_len) = (h16(28) as usize, h16(30) as usize, h16(32));
        let disk_start = h16(34);
        let local_offset = h32(42);
        let name_at = at + CENTRAL_LEN;
        let next = name_at + name_len + extra_len + comment_len as usize;
        if next > end {
            return Err(malformed(
                ZIP,
                format!("central directory entry {index} runs past the directory"),
            ));
        }
        let raw = &data[name_at..name_at + name_len];
        let name = if flags & 0x0800 != 0 || raw.is_ascii() {
            String::from_utf8(raw.to_vec())
                .map_err(|_| malformed(ZIP, format!("member {index}'s name is not UTF-8")))?
        } else {
            return Err(refused(
                format!("ZIP member {index}"),
                "its name is not ASCII and not flagged UTF-8",
            ));
        };
        if flags & 0x0001 != 0 {
            return Err(refused(format!("ZIP member {name}"), "it is encrypted"));
        }
        if compressed == u32::MAX || size == u32::MAX || local_offset == u32::MAX {
            return Err(refused(format!("ZIP member {name}"), "ZIP64 is not read"));
        }
        if disk_start != 0 {
            return Err(refused(
                format!("ZIP member {name}"),
                "it starts on another disk",
            ));
        }
        if method != 0 && method != 8 {
            return Err(refused(
                format!("ZIP member {name}"),
                format!("compression method {method}; stored (0) and deflate (8) are read"),
            ));
        }
        if method == 0 && compressed != size {
            return Err(malformed(
                ZIP,
                format!("stored member {name} states {compressed} bytes stored and {size} read"),
            ));
        }
        entries.push(ZipEntry {
            name,
            method,
            crc32,
            compressed: u64::from(compressed),
            size: u64::from(size),
            local_offset: u64::from(local_offset),
        });
        at = next;
    }
    let mut seen = std::collections::HashSet::new();
    for e in &entries {
        if !seen.insert(e.name.to_ascii_lowercase()) {
            return Err(refused(
                "ZIP archive",
                format!("member {} is listed twice, in some letter case", e.name),
            ));
        }
    }
    Ok(entries)
}

/// One member's bytes, decompressed and checked against its CRC-32 and size.
pub(crate) fn zip_read(data: &[u8], entry: &ZipEntry, limit: u64) -> Result<Vec<u8>, VortexError> {
    if entry.size > limit {
        return Err(refused(
            format!("ZIP member {}", entry.name),
            format!("{} bytes is over the limit of {limit}", entry.size),
        ));
    }
    let at = usize::try_from(entry.local_offset)
        .ok()
        .filter(|at| at + LOCAL_LEN <= data.len())
        .ok_or_else(|| {
            malformed(
                ZIP,
                format!("member {}'s header is outside the file", entry.name),
            )
        })?;
    if u32_at(data, at) != Some(LOCAL_SIGNATURE) {
        return Err(malformed(
            ZIP,
            format!(
                "member {} has no local header where the directory says",
                entry.name
            ),
        ));
    }
    let l16 = |o: usize| u16_at(data, at + o).unwrap_or(0);
    let local_flags = l16(6);
    let local_method = l16(8);
    let (name_len, extra_len) = (l16(26) as usize, l16(28) as usize);
    if local_method != entry.method || local_flags & 0x0001 != 0 {
        return Err(malformed(
            ZIP,
            format!(
                "member {}'s local header disagrees with the directory",
                entry.name
            ),
        ));
    }
    let local_name = data.get(at + LOCAL_LEN..at + LOCAL_LEN + name_len);
    if local_name != Some(entry.name.as_bytes()) {
        return Err(malformed(
            ZIP,
            format!("member {}'s local header names another member", entry.name),
        ));
    }
    let body = at + LOCAL_LEN + name_len + extra_len;
    let stored = usize::try_from(entry.compressed)
        .ok()
        .and_then(|n| body.checked_add(n))
        .filter(|end| *end <= data.len())
        .ok_or_else(|| malformed(ZIP, format!("member {} runs past the file", entry.name)))?;
    let raw = &data[body..stored];
    let bytes = if entry.method == 0 {
        let mut out = Vec::new();
        out.try_reserve_exact(raw.len())
            .map_err(|_| refused(format!("ZIP member {}", entry.name), "cannot hold it"))?;
        out.extend_from_slice(raw);
        out
    } else {
        read_bounded(
            flate2::read::DeflateDecoder::new(raw),
            limit,
            Some(entry.size),
            ZIP,
        )?
    };
    if bytes.len() as u64 != entry.size {
        return Err(malformed(
            ZIP,
            format!(
                "member {} decodes to {} bytes and states {}",
                entry.name,
                bytes.len(),
                entry.size
            ),
        ));
    }
    let mut crc = flate2::Crc::new();
    crc.update(&bytes);
    if crc.sum() != entry.crc32 {
        return Err(malformed(
            ZIP,
            format!("member {} fails its CRC-32", entry.name),
        ));
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------------------------
// tar
// ---------------------------------------------------------------------------------------------

const TAR: &str = "tar";
const BLOCK: usize = 512;
/// More members than a SNODAS day holds (16) by far.
const MAX_TAR_ENTRIES: usize = 4096;

/// One regular file in a tar archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TarEntry {
    pub name: String,
    offset: usize,
    pub size: usize,
}

fn octal(field: &[u8], what: &str) -> Result<u64, VortexError> {
    // ustar: octal digits, padded with spaces or NULs at either end.
    let text = field
        .iter()
        .copied()
        .skip_while(|b| *b == b' ' || *b == 0)
        .take_while(|b| *b != b' ' && *b != 0)
        .collect::<Vec<u8>>();
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        return Err(refused(
            format!("tar header {what}"),
            "a base-256 number is not read",
        ));
    }
    let mut n: u64 = 0;
    for b in text {
        if !(b'0'..=b'7').contains(&b) {
            return Err(malformed(TAR, format!("{what} is not octal")));
        }
        n = n
            .checked_mul(8)
            .and_then(|n| n.checked_add(u64::from(b - b'0')))
            .ok_or_else(|| malformed(TAR, format!("{what} overflows")))?;
    }
    Ok(n)
}

fn cstr(field: &[u8]) -> &[u8] {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    &field[..end]
}

/// Whether a block is a ustar or GNU tar header with a valid checksum.
pub(crate) fn is_tar(head: &[u8]) -> bool {
    let Some(block) = head.get(..BLOCK) else {
        return false;
    };
    let magic = &block[257..263];
    (magic == b"ustar\0" || magic == b"ustar ") && checksum_ok(block)
}

fn checksum_ok(block: &[u8]) -> bool {
    let Ok(stored) = octal(&block[148..156], "checksum") else {
        return false;
    };
    let sum: u64 = block
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u64::from(*b)
            }
        })
        .sum();
    sum == stored
}

/// Regular files, in archive order. Directories are skipped. Refused: links, devices, and long-name
/// or pax extension headers, none of which a SNODAS archive holds, since each would change what a
/// later header means.
pub(crate) fn tar_entries(data: &[u8]) -> Result<Vec<TarEntry>, VortexError> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    loop {
        let Some(block) = data.get(at..at + BLOCK) else {
            // GNU tar ends with two zero blocks, but some writers stop at the last member.
            if at == data.len() {
                return Ok(entries);
            }
            return Err(malformed(TAR, "a header is cut short"));
        };
        if block.iter().all(|b| *b == 0) {
            return Ok(entries);
        }
        let magic = &block[257..263];
        if magic != b"ustar\0" && magic != b"ustar " {
            return Err(malformed(
                TAR,
                format!("no ustar magic in the header at {at}"),
            ));
        }
        if !checksum_ok(block) {
            return Err(malformed(
                TAR,
                format!("the header at {at} fails its checksum"),
            ));
        }
        let mut name = cstr(&block[0..100]).to_vec();
        if magic == b"ustar\0" {
            let prefix = cstr(&block[345..500]);
            if !prefix.is_empty() {
                let mut full = prefix.to_vec();
                full.push(b'/');
                full.extend_from_slice(&name);
                name = full;
            }
        }
        let name = String::from_utf8(name)
            .map_err(|_| malformed(TAR, format!("the name at {at} is not UTF-8")))?;
        let size = octal(&block[124..136], "size")?;
        let size = usize::try_from(size)
            .ok()
            .filter(|s| {
                (at + BLOCK)
                    .checked_add(*s)
                    .is_some_and(|end| end <= data.len())
            })
            .ok_or_else(|| malformed(TAR, format!("member {name} runs past the archive")))?;
        match block[156] {
            b'0' | 0 | b'7' => {
                if entries.len() == MAX_TAR_ENTRIES {
                    return Err(refused(
                        "tar archive",
                        format!("more than {MAX_TAR_ENTRIES} members"),
                    ));
                }
                entries.push(TarEntry {
                    name,
                    offset: at + BLOCK,
                    size,
                });
            }
            b'5' => {}
            other => {
                return Err(refused(
                    format!("tar member {name}"),
                    format!("type '{}' is not a regular file or folder", other as char),
                ))
            }
        }
        let padded = size.div_ceil(BLOCK) * BLOCK;
        at = at + BLOCK + padded;
    }
}

/// A member's bytes, borrowed from the archive.
pub(crate) fn tar_read<'a>(data: &'a [u8], entry: &TarEntry) -> &'a [u8] {
    &data[entry.offset..entry.offset + entry.size]
}

// ---------------------------------------------------------------------------------------------
// gzip
// ---------------------------------------------------------------------------------------------

const GZIP: &str = "gzip";

/// Whether bytes start as a gzip member does (RFC 1952 2.3.1, deflate).
pub(crate) fn is_gzip(head: &[u8]) -> bool {
    head.starts_with(&[0x1f, 0x8b, 0x08])
}

/// One gzip member, checked against its CRC-32 and length. Bytes after the member are refused:
/// `GzipCompressorInputStream`, which Vortex uses, would read a second member, and a reader that
/// stopped would see less.
pub(crate) fn gunzip(data: &[u8], limit: u64) -> Result<Vec<u8>, VortexError> {
    if !is_gzip(data) {
        return Err(malformed(GZIP, "no gzip header"));
    }
    let mut decoder = flate2::bufread::GzDecoder::new(data);
    let out = read_bounded(&mut decoder, limit, None, GZIP)?;
    let rest = decoder.into_inner();
    if !rest.is_empty() {
        return Err(refused(
            "gzip data",
            format!("{} bytes follow the first member", rest.len()),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn deflate(bytes: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn crc(bytes: &[u8]) -> u32 {
        let mut c = flate2::Crc::new();
        c.update(bytes);
        c.sum()
    }

    /// A ZIP of the given members, the second element saying whether to deflate.
    fn zip(members: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, bytes, deflated) in members {
            let stored = if *deflated {
                deflate(bytes)
            } else {
                bytes.to_vec()
            };
            let method: u16 = if *deflated { 8 } else { 0 };
            let offset = out.len() as u32;
            out.extend_from_slice(&LOCAL_SIGNATURE.to_le_bytes());
            out.extend_from_slice(&[20, 0, 0, 0]);
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&crc(bytes).to_le_bytes());
            out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&stored);
            central.extend_from_slice(&CENTRAL_SIGNATURE.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0; 4]);
            central.extend_from_slice(&crc(bytes).to_le_bytes());
            central.extend_from_slice(&(stored.len() as u32).to_le_bytes());
            central.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&EOCD_SIGNATURE.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        let n = members.len() as u16;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn zip_round_trip_stored_and_deflated() {
        let data = zip(&[
            ("a.hdr", b"NROWS 1\n", false),
            ("a.bil", &[7u8; 1000], true),
        ]);
        assert!(is_zip(&data));
        let entries = zip_entries(&data).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(zip_read(&data, &entries[0], 1 << 20).unwrap(), b"NROWS 1\n");
        assert_eq!(
            zip_read(&data, &entries[1], 1 << 20).unwrap(),
            vec![7u8; 1000]
        );
        assert!(zip_read(&data, &entries[1], 999).is_err());
    }

    #[test]
    fn zip_refuses_a_bad_crc_and_every_truncation() {
        let data = zip(&[("a.bil", &[1u8, 2, 3, 4], false)]);
        let mut bad = data.clone();
        bad[LOCAL_LEN + 5] ^= 1;
        let entries = zip_entries(&bad).unwrap();
        assert!(zip_read(&bad, &entries[0], 1 << 20).is_err());
        for cut in 0..data.len() {
            let short = &data[..cut];
            if let Ok(entries) = zip_entries(short) {
                for e in entries {
                    let _ = zip_read(short, &e, 1 << 20);
                }
            }
        }
    }

    #[test]
    fn zip_refuses_duplicate_names() {
        let data = zip(&[("A.bil", b"x", false), ("a.BIL", b"y", false)]);
        assert!(zip_entries(&data).is_err());
    }

    fn tar_header(name: &str, size: usize, kind: u8) -> Vec<u8> {
        let mut h = vec![0u8; BLOCK];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        h[156] = kind;
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        h
    }

    #[test]
    fn tar_lists_regular_files_and_stops_at_zeros() {
        let mut t = tar_header("dir/", 0, b'5');
        t.extend(tar_header("x.dat.gz", 3, b'0'));
        t.extend([1, 2, 3]);
        t.resize(t.len().div_ceil(BLOCK) * BLOCK, 0);
        t.extend(vec![0u8; 2 * BLOCK]);
        assert!(is_tar(&t));
        let e = tar_entries(&t).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].name, "x.dat.gz");
        assert_eq!(tar_read(&t, &e[0]), &[1, 2, 3]);
        let mut link = tar_header("l", 0, b'2');
        link.extend(vec![0u8; 2 * BLOCK]);
        assert!(tar_entries(&link).is_err());
        for cut in 0..t.len() {
            let _ = tar_entries(&t[..cut]);
        }
    }

    #[test]
    fn gunzip_checks_trailing_bytes_and_limit() {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(&[5u8; 100]).unwrap();
        let gz = e.finish().unwrap();
        assert_eq!(gunzip(&gz, 100).unwrap(), vec![5u8; 100]);
        assert!(gunzip(&gz, 99).is_err());
        let mut two = gz.clone();
        two.extend_from_slice(&gz);
        assert!(gunzip(&two, 1000).is_err());
        let mut bad = gz.clone();
        let n = bad.len();
        bad[n - 5] ^= 1;
        assert!(gunzip(&bad, 1000).is_err());
    }

    // A stream that never ends is refused at the limit, whatever size
    // its file states.
    #[test]
    fn an_endless_stream_is_refused_at_the_limit() {
        let endless = std::io::repeat(7);
        match read_stream_bounded(endless, 1000, "input") {
            Err(VortexError::Refused { reason, .. }) => {
                assert!(reason.contains("1000"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
        let exact = read_stream_bounded(&[7u8; 1000][..], 1000, "input").unwrap();
        assert_eq!(exact.len(), 1000);
        assert!(read_stream_bounded(&[7u8; 1001][..], 1000, "input").is_err());
        assert!(read_stream_bounded(&[][..], 0, "input").unwrap().is_empty());
    }

    // A member that states 4 GiB reserves one read chunk, not 4 GiB; the
    // buffer grows with what is decoded.
    #[test]
    fn a_stated_size_reserves_at_most_one_chunk() {
        let limit = 1u64 << 32;
        let out = read_bounded(&b"twelve bytes"[..], limit, Some(limit - 1), "test").unwrap();
        assert_eq!(out, b"twelve bytes");
        assert!(out.capacity() <= READ_CHUNK, "{}", out.capacity());
        // A deflated ZIP member stating 4 GiB - 1 and holding 3 bytes: refused for its size, and
        // nothing near the stated size was reserved on the way.
        let packed = deflate(b"abc");
        let entry = ZipEntry {
            name: "a.asc".into(),
            method: 8,
            crc32: crc(b"abc"),
            compressed: packed.len() as u64,
            size: limit - 1,
            local_offset: 0,
        };
        let mut data = Vec::new();
        data.extend_from_slice(&LOCAL_SIGNATURE.to_le_bytes());
        data.extend_from_slice(&[20, 0, 0, 0, 8, 0, 0, 0, 0, 0]);
        data.extend_from_slice(&crc(b"abc").to_le_bytes());
        data.extend_from_slice(&(packed.len() as u32).to_le_bytes());
        data.extend_from_slice(&u32::MAX.to_le_bytes());
        data.extend_from_slice(&5u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(b"a.asc");
        data.extend_from_slice(&packed);
        assert!(zip_read(&data, &entry, limit).is_err());
    }
}
