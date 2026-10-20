//! Minimal LevelDB reader: SSTable tables (`.ldb`/`.sst`) and the write-ahead
//! log (`.log`), enough to recover the newest live value for every key. Snappy
//! blocks go through the `snap` crate; the rest is parsed here.
//!
//! Tied to LevelDB's current on-disk framing, so it is version sensitive.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context as _, Result};

use super::secretbox::Decryptor;
use super::v8;
use super::Record;

/// The newest write seen for one user key.
struct Entry {
    seq: u64,
    value: Option<Vec<u8>>,
    is_live: bool,
}

/// Read every table and log in `dir`, keep the newest live value per key, then
/// parse each into a record.
pub fn extract_records(dir: &Path, decryptor: &Decryptor) -> Result<Vec<Record>> {
    let mut map: BTreeMap<Vec<u8>, Entry> = BTreeMap::new();
    let entries = std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        // Rust opens files with shared read/write/delete, so a running Termius
        // holding the file open does not block the read.
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        match extension.as_deref() {
            Some("ldb") | Some("sst") => {
                if let Err(e) = read_table(&data, &mut map) {
                    tracing::debug!(?e, path = %path.display(), "skipping unreadable table");
                }
            }
            Some("log") => read_log(&data, &mut map),
            _ => {}
        }
    }

    let mut records = Vec::new();
    for (user_key, entry) in map {
        if !entry.is_live {
            continue;
        }
        let Some(value) = entry.value else { continue };
        let primary_key = recover_primary_key(&user_key);
        if let Some(record) = v8::scan_value(&value, decryptor, primary_key) {
            records.push(record);
        }
    }
    Ok(records)
}

/// IndexedDB integer object-store keys end with `[0x03][8-byte LE double]`.
fn recover_primary_key(user_key: &[u8]) -> Option<i64> {
    let len = user_key.len();
    if len < 9 || user_key[len - 9] != 0x03 {
        return None;
    }
    let bytes: [u8; 8] = user_key[len - 8..].try_into().ok()?;
    Some(f64::from_le_bytes(bytes).round() as i64)
}

fn remember(
    map: &mut BTreeMap<Vec<u8>, Entry>,
    user_key: &[u8],
    seq: u64,
    value: Option<Vec<u8>>,
    is_live: bool,
) {
    if let Some(existing) = map.get(user_key) {
        if seq < existing.seq {
            return;
        }
    }
    map.insert(
        user_key.to_vec(),
        Entry {
            seq,
            value,
            is_live,
        },
    );
}

// ---- varints (LEB128) ----

fn read_varint(bytes: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let c = *bytes.get(*pos).context("varint past end")?;
        *pos += 1;
        result |= ((c & 0x7F) as u64) << shift;
        if c & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift > 63 {
            bail!("varint too long");
        }
    }
}

/// `bytes[start..start + len]`, or `None` when it runs past the end or the
/// length from the file overflows.
fn slice_at(bytes: &[u8], start: usize, len: usize) -> Option<&[u8]> {
    bytes.get(start..start.checked_add(len)?)
}

fn u32_le(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes.get(offset..offset + 4).context("u32 past end")?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

// ---- SSTable ----

/// A decompressed block and the handle it came from.
fn read_block(file: &[u8], offset: u64, size: u64) -> Result<Vec<u8>> {
    let offset = usize::try_from(offset).context("block offset too large")?;
    let size = usize::try_from(size).context("block size too large")?;
    let block = slice_at(file, offset, size).context("block past end")?;
    let type_byte = *file.get(offset + size).context("block trailer past end")?;
    match type_byte {
        0 => Ok(block.to_vec()),
        1 => snap::raw::Decoder::new()
            .decompress_vec(block)
            .context("snappy decompress"),
        other => bail!("unknown block compression {other}"),
    }
}

/// One entry of a data/index block: full key, then value offset and length
/// into the block.
struct BlockEntry {
    key: Vec<u8>,
    value_offset: usize,
    value_len: usize,
}

/// Parse a block's entries (restart prefix compression).
fn block_entries(block: &[u8]) -> Result<Vec<BlockEntry>> {
    if block.len() < 4 {
        bail!("block too small");
    }
    let num_restarts = u32_le(block, block.len() - 4)? as usize;
    let restart_start = block
        .len()
        .checked_sub(4 + 4 * num_restarts)
        .context("bad restart count")?;

    let mut entries = Vec::new();
    let mut pos = 0;
    let mut previous: Vec<u8> = Vec::new();
    while pos < restart_start {
        let shared = read_varint(block, &mut pos)? as usize;
        let non_shared = read_varint(block, &mut pos)? as usize;
        let value_len = read_varint(block, &mut pos)? as usize;
        if shared > previous.len() {
            bail!("shared prefix longer than previous key");
        }
        // Check lengths against the block before allocating: a corrupt varint
        // can ask for exabytes, and a failed allocation aborts the process.
        let non_shared_bytes =
            slice_at(&block[..restart_start], pos, non_shared).context("key past end")?;
        let mut key = previous[..shared].to_vec();
        key.extend_from_slice(non_shared_bytes);
        pos += non_shared;
        let value_offset = pos;
        pos = pos
            .checked_add(value_len)
            .filter(|&end| end <= restart_start)
            .context("value past end")?;
        entries.push(BlockEntry {
            key: key.clone(),
            value_offset,
            value_len,
        });
        previous = key;
    }
    Ok(entries)
}

fn read_table(file: &[u8], map: &mut BTreeMap<Vec<u8>, Entry>) -> Result<()> {
    if file.len() < 48 {
        bail!("file too small for a table footer");
    }
    let mut fp = file.len() - 48;
    let _meta_offset = read_varint(file, &mut fp)?;
    let _meta_size = read_varint(file, &mut fp)?;
    let index_offset = read_varint(file, &mut fp)?;
    let index_size = read_varint(file, &mut fp)?;

    let index = read_block(file, index_offset, index_size)?;
    for entry in block_entries(&index)? {
        let mut bp = entry.value_offset;
        let data_offset = read_varint(&index, &mut bp)?;
        let data_size = read_varint(&index, &mut bp)?;
        let Ok(data_block) = read_block(file, data_offset, data_size) else {
            continue;
        };
        let Ok(data_entries) = block_entries(&data_block) else {
            continue;
        };
        for data in data_entries {
            read_table_entry(&data, &data_block, map);
        }
    }
    Ok(())
}

/// One data-block entry: the key's 8-byte trailer carries the sequence and the
/// value type (1 = value, 0 = deletion).
fn read_table_entry(entry: &BlockEntry, block: &[u8], map: &mut BTreeMap<Vec<u8>, Entry>) {
    if entry.key.len() < 8 {
        return;
    }
    let trailer_start = entry.key.len() - 8;
    let mut trailer: u64 = 0;
    for (i, &byte) in entry.key[trailer_start..].iter().enumerate() {
        trailer |= (byte as u64) << (8 * i);
    }
    let seq = trailer >> 8;
    let value_type = (trailer & 0xFF) as u8;
    let user_key = &entry.key[..trailer_start];
    let Some(value) = slice_at(block, entry.value_offset, entry.value_len) else {
        return;
    };
    remember(map, user_key, seq, Some(value.to_vec()), value_type == 1);
}

// ---- write-ahead log ----

const LOG_BLOCK: usize = 32768;

fn read_log(file: &[u8], map: &mut BTreeMap<Vec<u8>, Entry>) {
    let mut record: Vec<u8> = Vec::new();
    let mut is_in_batch = false;
    let mut pos = 0;
    while pos + 7 <= file.len() {
        let block_remain = LOG_BLOCK - (pos % LOG_BLOCK);
        if block_remain < 7 {
            pos += block_remain;
            continue;
        }
        let len = file[pos + 4] as usize | ((file[pos + 5] as usize) << 8);
        let record_type = file[pos + 6];
        let data_start = pos + 7;
        if record_type == 0 {
            pos += block_remain; // zero padding to the next block
            record.clear();
            is_in_batch = false;
            continue;
        }
        if data_start + len > file.len() {
            break;
        }
        let data = &file[data_start..data_start + len];
        pos = data_start + len;
        match record_type {
            // FULL. A FIRST/MIDDLE left before it by an interrupted write is dropped.
            1 => parse_batch(data, map),
            // FIRST starts a new batch, dropping any unfinished one.
            2 => {
                record.clear();
                record.extend_from_slice(data);
                is_in_batch = true;
            }
            3 if is_in_batch => record.extend_from_slice(data),
            4 if is_in_batch => {
                record.extend_from_slice(data);
                parse_batch(&record, map);
                record.clear();
                is_in_batch = false;
            }
            // A MIDDLE or LAST with no FIRST before it, or an unknown type.
            _ => {
                record.clear();
                is_in_batch = false;
            }
        }
    }
}

fn parse_batch(batch: &[u8], map: &mut BTreeMap<Vec<u8>, Entry>) {
    if batch.len() < 12 {
        return;
    }
    let mut seq: u64 = 0;
    for (i, &byte) in batch[..8].iter().enumerate() {
        seq |= (byte as u64) << (8 * i);
    }
    let Ok(count) = u32_le(batch, 8) else { return };
    let mut pos = 12;
    for i in 0..count {
        if pos >= batch.len() {
            break;
        }
        let record_type = batch[pos];
        pos += 1;
        let Ok(key_len) = read_varint(batch, &mut pos) else {
            return;
        };
        let Some(user_key) = slice_at(batch, pos, key_len as usize) else {
            return;
        };
        let user_key = user_key.to_vec();
        pos += user_key.len();
        let op_seq = seq.saturating_add(i as u64);
        if record_type == 1 {
            let Ok(value_len) = read_varint(batch, &mut pos) else {
                return;
            };
            let Some(value) = slice_at(batch, pos, value_len as usize) else {
                return;
            };
            let value = value.to_vec();
            pos += value.len();
            remember(map, &user_key, op_seq, Some(value), true);
        } else {
            remember(map, &user_key, op_seq, None, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(buf: &mut Vec<u8>, mut n: u64) {
        loop {
            let mut byte = (n & 0x7F) as u8;
            n >>= 7;
            if n != 0 {
                byte |= 0x80;
            }
            buf.push(byte);
            if n == 0 {
                break;
            }
        }
    }

    /// Build one log record holding a write batch with a single PUT, and check
    /// the parser recovers the value.
    #[test]
    fn parses_a_log_put_record() {
        let user_key = b"user-key";
        let value = b"the value blob";

        let mut batch = Vec::new();
        batch.extend_from_slice(&5u64.to_le_bytes()); // sequence
        batch.extend_from_slice(&1u32.to_le_bytes()); // one op
        batch.push(1); // PUT
        varint(&mut batch, user_key.len() as u64);
        batch.extend_from_slice(user_key);
        varint(&mut batch, value.len() as u64);
        batch.extend_from_slice(value);

        // Log record header: 4-byte crc (ignored), 2-byte len, 1-byte type=FULL.
        let mut log = vec![0u8; 4];
        log.extend_from_slice(&(batch.len() as u16).to_le_bytes());
        log.push(1); // FULL
        log.extend_from_slice(&batch);

        let mut map = BTreeMap::new();
        read_log(&log, &mut map);

        let entry = map.get(user_key.as_slice()).unwrap();
        assert!(entry.is_live);
        assert_eq!(entry.value.as_deref(), Some(value.as_slice()));
        assert_eq!(entry.seq, 5);
    }

    #[test]
    fn a_later_deletion_wins_over_an_earlier_put() {
        let user_key = b"k";
        let mut map = BTreeMap::new();
        remember(&mut map, user_key, 1, Some(b"v".to_vec()), true);
        remember(&mut map, user_key, 2, None, false);
        assert!(!map.get(user_key.as_slice()).unwrap().is_live);
    }

    #[test]
    fn an_older_write_does_not_overwrite_a_newer_one() {
        let user_key = b"k";
        let mut map = BTreeMap::new();
        remember(&mut map, user_key, 9, Some(b"new".to_vec()), true);
        remember(&mut map, user_key, 2, Some(b"old".to_vec()), true);
        assert_eq!(
            map.get(user_key.as_slice()).unwrap().value.as_deref(),
            Some(b"new".as_slice())
        );
    }

    #[test]
    fn recovers_integer_primary_key_from_key_tail() {
        let mut user_key = vec![0xAA, 0xBB];
        user_key.push(0x03);
        user_key.extend_from_slice(&(1234.0f64).to_le_bytes());
        assert_eq!(recover_primary_key(&user_key), Some(1234));
    }

    #[test]
    fn no_primary_key_without_the_marker() {
        let user_key = vec![1, 2, 3, 4, 5, 6, 7, 8, 9];
        assert_eq!(recover_primary_key(&user_key), None);
    }

    /// A block with one entry and one restart point at offset 0.
    fn block_with_entry(entry: &[u8]) -> Vec<u8> {
        let mut block = entry.to_vec();
        block.extend_from_slice(&0u32.to_le_bytes()); // restart offset
        block.extend_from_slice(&1u32.to_le_bytes()); // restart count
        block
    }

    #[test]
    fn a_huge_key_length_is_an_error_not_an_allocation() {
        let mut entry = Vec::new();
        varint(&mut entry, 0); // shared
        varint(&mut entry, 1 << 62); // non_shared
        varint(&mut entry, 0); // value_len
        assert!(block_entries(&block_with_entry(&entry)).is_err());
    }

    #[test]
    fn a_value_length_that_wraps_is_an_error_not_a_loop() {
        let mut entry = Vec::new();
        varint(&mut entry, 0);
        varint(&mut entry, 0);
        varint(&mut entry, u64::MAX - 11);
        assert!(block_entries(&block_with_entry(&entry)).is_err());
    }

    fn log_record(record_type: u8, data: &[u8]) -> Vec<u8> {
        let mut log = vec![0u8; 4];
        log.extend_from_slice(&(data.len() as u16).to_le_bytes());
        log.push(record_type);
        log.extend_from_slice(data);
        log
    }

    #[test]
    fn a_dangling_first_fragment_is_dropped_before_a_full_record() {
        let user_key = b"k";
        let mut batch = Vec::new();
        batch.extend_from_slice(&7u64.to_le_bytes());
        batch.extend_from_slice(&1u32.to_le_bytes());
        batch.push(1);
        varint(&mut batch, 1);
        batch.extend_from_slice(user_key);
        varint(&mut batch, 1);
        batch.push(b'v');

        let mut log = log_record(2, b"half of an older batch");
        log.extend_from_slice(&log_record(1, &batch));

        let mut map = BTreeMap::new();
        read_log(&log, &mut map);
        let entry = map.get(user_key.as_slice()).unwrap();
        assert_eq!(entry.seq, 7);
        assert_eq!(entry.value.as_deref(), Some(b"v".as_slice()));
    }

    /// Snappy path through the crate: compress a value, decompress via
    /// `read_block` with a type-1 trailer.
    #[test]
    fn read_block_decompresses_snappy() {
        let original = b"snappy block payload snappy block payload";
        let compressed = snap::raw::Encoder::new().compress_vec(original).unwrap();
        let mut file = compressed.clone();
        file.push(1); // compression type = snappy
        let block = read_block(&file, 0, compressed.len() as u64).unwrap();
        assert_eq!(block, original);
    }
}
