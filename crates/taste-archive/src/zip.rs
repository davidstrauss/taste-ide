//! Just enough of a zip file for a WACZ: its entries' names, and their
//! data, stored or deflated. Zip64 is not read: a WACZ is a page or a
//! session, under the viewer's limit, not four gigabytes.

use std::io::Read as _;

/// An entry of the central directory.
#[derive(Debug)]
pub(crate) struct Entry {
    pub name: String,
    method: u16,
    compressed: usize,
    size: usize,
    local: usize,
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, String> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| "the zip ends inside a header".to_string())
}

fn u32_at(bytes: &[u8], at: usize) -> Result<usize, String> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        .ok_or_else(|| "the zip ends inside a header".to_string())
}

/// The entries the central directory lists.
pub(crate) fn entries(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    // The end of the central directory: its signature, within the last
    // 64 KiB and its own 22 bytes, which a trailing comment may push back.
    let floor = bytes.len().saturating_sub(65_557);
    let end = (floor..bytes.len().saturating_sub(21))
        .rev()
        .find(|&at| bytes[at..at + 4] == [0x50, 0x4b, 0x05, 0x06])
        .ok_or("this is not a zip file: it has no central directory")?;
    let count = u16_at(bytes, end + 10)? as usize;
    let mut at = u32_at(bytes, end + 16)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if bytes.get(at..at + 4) != Some(&[0x50, 0x4b, 0x01, 0x02]) {
            return Err("the zip's central directory is damaged".into());
        }
        let name_len = u16_at(bytes, at + 28)? as usize;
        let extra_len = u16_at(bytes, at + 30)? as usize;
        let comment_len = u16_at(bytes, at + 32)? as usize;
        let name = bytes
            .get(at + 46..at + 46 + name_len)
            .ok_or("the zip ends inside a name")?;
        entries.push(Entry {
            name: String::from_utf8_lossy(name).into_owned(),
            method: u16_at(bytes, at + 10)?,
            compressed: u32_at(bytes, at + 20)?,
            size: u32_at(bytes, at + 24)?,
            local: u32_at(bytes, at + 42)?,
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

/// An entry's data, unpacked.
pub(crate) fn unpack(bytes: &[u8], entry: &Entry) -> Result<Vec<u8>, String> {
    let at = entry.local;
    if bytes.get(at..at + 4) != Some(&[0x50, 0x4b, 0x03, 0x04]) {
        return Err(format!("{}: its local header is damaged", entry.name));
    }
    let start = at + 30 + u16_at(bytes, at + 26)? as usize + u16_at(bytes, at + 28)? as usize;
    let data = bytes
        .get(start..start + entry.compressed)
        .ok_or_else(|| format!("{}: the zip ends inside it", entry.name))?;
    if entry.size > crate::MAX_UNPACKED {
        return Err(format!("{} is larger than the viewer unpacks", entry.name));
    }
    match entry.method {
        0 => Ok(data.to_vec()),
        8 => {
            let mut out = Vec::with_capacity(entry.size);
            flate2::read::DeflateDecoder::new(data)
                .take(crate::MAX_UNPACKED as u64)
                .read_to_end(&mut out)
                .map_err(|e| format!("{}: {e}", entry.name))?;
            Ok(out)
        }
        method => Err(format!(
            "{}: compression method {method} is not read",
            entry.name
        )),
    }
}
