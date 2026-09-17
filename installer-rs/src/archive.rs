//! Self-contained payload container (solid LZMA2).
//!
//! The installer executable carries the whole bundle (scripts + driver + Mesa +
//! CLVK + loaders + certificate + manifest) appended to its own PE image, with a
//! fixed 64-byte footer. This module owns BOTH ends of that format, so the CI
//! packing step and the runtime unpacking step cannot drift.
//!
//! The payload is extracted in full every run, so there is no per-file random
//! access to preserve and the data is one solid LZMA2 stream. Measured on the
//! real bundle, that is ~30% smaller than per-file DEFLATE.
//!
//! Layout:
//!   [ PE image ][ header ][ xz stream ][ footer ]
//!
//! Footer (little-endian, last 64 bytes of the file):
//!   u64 container_offset   file offset where the container starts
//!   u64 container_size     container length in bytes (header + xz stream)
//!   u64 header_len         uncompressed header length
//!   [32] sha256(container)
//!   [8]  magic = b"HLIOSET2"
//!
//! Header (uncompressed, at the container start):
//!   u32 count
//!   count * { u16 name_len, name[..] (UTF-8, '/'-separated), u64 size }
//! The xz stream decodes to the concatenation of every entry's bytes, in header
//! order.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use xz2::read::XzDecoder;
use xz2::stream::{Check, Stream};
use xz2::write::XzEncoder;

const MAGIC: &[u8; 8] = b"HLIOSET2";
const FOOTER_LEN: u64 = 64;
// LZMA_PRESET_EXTREME, matching `xz -9e`.
const PRESET_EXTREME: u32 = 9 | (1 << 31);

fn corrupt(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

/// (container_offset, container_size, header_len, sha256)
fn read_footer(exe: &Path) -> io::Result<Option<(u64, u64, u64, [u8; 32])>> {
    let mut file = File::open(exe)?;
    let len = file.metadata()?.len();
    if len < FOOTER_LEN {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(len - FOOTER_LEN))?;
    let mut footer = [0u8; FOOTER_LEN as usize];
    file.read_exact(&mut footer)?;
    if &footer[56..64] != MAGIC {
        return Ok(None);
    }
    let container_offset = u64::from_le_bytes(footer[0..8].try_into().unwrap());
    let container_size = u64::from_le_bytes(footer[8..16].try_into().unwrap());
    let header_len = u64::from_le_bytes(footer[16..24].try_into().unwrap());
    let mut sha = [0u8; 32];
    sha.copy_from_slice(&footer[24..56]);
    if container_offset.checked_add(container_size).and_then(|v| v.checked_add(FOOTER_LEN))
        != Some(len)
    {
        return Err(corrupt("payload footer does not describe this file"));
    }
    Ok(Some((container_offset, container_size, header_len, sha)))
}

pub fn is_self_contained(exe: &Path) -> bool {
    matches!(read_footer(exe), Ok(Some(_)))
}

fn read_container(exe: &Path) -> io::Result<(Vec<u8>, u64)> {
    let (offset, size, header_len, expected) = read_footer(exe)?
        .ok_or_else(|| corrupt("the installer has no embedded payload"))?;
    let mut file = File::open(exe)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut container = vec![0u8; size as usize];
    file.read_exact(&mut container)?;
    if Sha256::digest(&container)[..] != expected[..] {
        return Err(corrupt("embedded payload failed its SHA-256 check"));
    }
    if header_len > size {
        return Err(corrupt("payload header length is invalid"));
    }
    Ok((container, header_len))
}

fn parse_header(header: &[u8]) -> io::Result<Vec<(String, u64)>> {
    let mut cursor = 0usize;
    let take = |cursor: &mut usize, n: usize| -> io::Result<&[u8]> {
        if cursor.checked_add(n).map_or(true, |end| end > header.len()) {
            return Err(corrupt("payload header is truncated"));
        }
        let slice = &header[*cursor..*cursor + n];
        *cursor += n;
        Ok(slice)
    };
    let count = u32::from_le_bytes(take(&mut cursor, 4)?.try_into().unwrap());
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name_len = u16::from_le_bytes(take(&mut cursor, 2)?.try_into().unwrap()) as usize;
        let name = String::from_utf8(take(&mut cursor, name_len)?.to_vec())
            .map_err(|_| corrupt("payload entry name is not UTF-8"))?;
        let size = u64::from_le_bytes(take(&mut cursor, 8)?.try_into().unwrap());
        entries.push((name, size));
    }
    Ok(entries)
}

fn safe_join(dest: &Path, name: &str) -> io::Result<PathBuf> {
    let mut out = dest.to_path_buf();
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
            return Err(corrupt("payload entry has an unsafe path"));
        }
        out.push(part);
    }
    if !out.starts_with(dest) {
        return Err(corrupt("payload entry escapes the destination"));
    }
    Ok(out)
}

/// Read a self-contained exe and extract its payload to `dest`.
pub fn extract_self(exe: &Path, dest: &Path) -> io::Result<()> {
    let (container, header_len) = read_container(exe)?;
    let entries = parse_header(&container[..header_len as usize])?;
    fs::create_dir_all(dest)?;
    let mut decoder = XzDecoder::new(&container[header_len as usize..]);
    let mut buffer = vec![0u8; 1 << 20];
    for (name, size) in entries {
        let target = safe_join(dest, &name)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&target)?;
        let mut remaining = size;
        while remaining > 0 {
            let want = remaining.min(buffer.len() as u64) as usize;
            decoder.read_exact(&mut buffer[..want]).map_err(|_| {
                corrupt("payload stream ended before the declared entry size")
            })?;
            out.write_all(&buffer[..want])?;
            remaining -= want as u64;
        }
    }
    Ok(())
}

/// Pack `payload_dir` and append it to a copy of `template`, writing `out`.
/// Used only by the CI packaging step (`HeliosSetup.exe --bundle`).
pub fn build_bundle(template: &Path, payload_dir: &Path, out: &Path) -> io::Result<()> {
    let mut names: Vec<(String, PathBuf)> = Vec::new();
    collect(payload_dir, payload_dir, &mut names)?;
    // Never embed the output or the template themselves: repacking into the
    // payload directory would otherwise fold the previous container back in.
    let out_canonical = fs::canonicalize(out).ok();
    let template_canonical = fs::canonicalize(template).ok();
    names.retain(|(_, path)| {
        let canonical = fs::canonicalize(path).ok();
        canonical.is_some() && canonical != out_canonical && canonical != template_canonical
    });
    if names.is_empty() {
        return Err(corrupt("refusing to build a self-contained payload from an empty directory"));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));

    let mut header: Vec<u8> = Vec::new();
    header.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for (name, path) in &names {
        let name = name.as_bytes();
        header.extend_from_slice(&(name.len() as u16).to_le_bytes());
        header.extend_from_slice(name);
        header.extend_from_slice(&fs::metadata(path)?.len().to_le_bytes());
    }

    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    // Read+write: the container is hashed by re-reading it after it is written
    // rather than buffering ~65 MB in memory.
    let mut writer = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(out)?;
    writer.write_all(&fs::read(template)?)?;
    let container_offset = writer.stream_position()?;
    writer.write_all(&header)?;
    let header_len = header.len() as u64;
    {
        let stream = Stream::new_easy_encoder(PRESET_EXTREME, Check::Crc64)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
        let mut encoder = XzEncoder::new_stream(&mut writer, stream);
        for (_, path) in &names {
            let mut file = File::open(path)?;
            io::copy(&mut file, &mut encoder)?;
        }
        encoder.finish()?;
    }
    let container_size = writer.stream_position()? - container_offset;

    // Hash the container as written (re-read rather than buffer it in memory).
    let mut container = vec![0u8; container_size as usize];
    writer.seek(SeekFrom::Start(container_offset))?;
    writer.read_exact(&mut container)?;
    let digest = Sha256::digest(&container);

    writer.seek(SeekFrom::End(0))?;
    writer.write_all(&container_offset.to_le_bytes())?;
    writer.write_all(&container_size.to_le_bytes())?;
    writer.write_all(&header_len.to_le_bytes())?;
    writer.write_all(&digest)?;
    writer.write_all(MAGIC)?;
    writer.flush()?;
    Ok(())
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    for item in fs::read_dir(dir)? {
        let item = item?;
        let path = item.path();
        let ty = item.file_type()?;
        if ty.is_dir() {
            collect(root, &path, out)?;
        } else if ty.is_file() {
            let rel = path.strip_prefix(root).unwrap();
            let name = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((name, path));
        }
    }
    Ok(())
}
