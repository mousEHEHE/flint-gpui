//! Minimal reader for V8's ValueSerializer format, enough to pull Termius's
//! records out of an IndexedDB value blob. Encrypted string fields are
//! decrypted inline, so the caller sees plaintext.
//!
//! Only the tags Termius emits are handled; an unknown tag makes the scan give
//! up on that blob rather than guess.

use std::collections::BTreeMap;

use super::secretbox::Decryptor;

/// A value read from a V8 blob. Objects keep their fields; arrays and scalars
/// are kept only as far as the record model needs.
#[derive(Debug, Clone, PartialEq)]
pub enum V8Value {
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    String(String),
    Object(BTreeMap<String, V8Value>),
    Array(Vec<V8Value>),
}

impl V8Value {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            V8Value::Int(n) => Some(*n),
            V8Value::Double(d) => Some(d.round() as i64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            V8Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&BTreeMap<String, V8Value>> {
        match self {
            V8Value::Object(map) => Some(map),
            _ => None,
        }
    }

    /// A value worth keeping when merging partial records: not null and not an
    /// empty string.
    pub fn is_meaningful(&self) -> bool {
        match self {
            V8Value::Null => false,
            V8Value::String(s) => !s.is_empty(),
            _ => true,
        }
    }
}

const MAX_DEPTH: u32 = 40;

/// Scan one IndexedDB value blob for the first record-like object. `local_id`
/// is set from `primary_key` when the blob does not carry one itself (groups,
/// tags and junctions keep the id only in the LevelDB key).
pub fn scan_value(
    value: &[u8],
    decryptor: &Decryptor,
    primary_key: Option<i64>,
) -> Option<BTreeMap<String, V8Value>> {
    for start in 0..value.len() {
        if value[start] != b'o' {
            continue;
        }
        let mut reader = Reader {
            bytes: value,
            pos: start,
            decryptor,
        };
        let Ok(parsed) = reader.read_value(0) else {
            continue;
        };
        let V8Value::Object(mut map) = parsed else {
            continue;
        };
        if !is_record_like(&map) {
            continue;
        }
        match primary_key {
            Some(pk) => {
                map.insert("local_id".to_string(), V8Value::Int(pk));
            }
            None if !map.contains_key("local_id") => continue,
            None => {}
        }
        return Some(map);
    }
    None
}

const RECORD_KEYS: [&str; 16] = [
    "local_id",
    "label",
    "address",
    "status",
    "host",
    "tag",
    "username",
    "private_key",
    "port",
    "identity",
    "ssh_key",
    "parent_group",
    "hostnames",
    "script",
    "public_key",
    "passphrase",
];

fn is_record_like(map: &BTreeMap<String, V8Value>) -> bool {
    map.keys().any(|k| RECORD_KEYS.contains(&k.as_str()))
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    decryptor: &'a Decryptor,
}

/// A parse that failed on this blob. The scanner just moves to the next `o`.
#[derive(Debug)]
struct ParseError;

type ParseResult<T> = Result<T, ParseError>;

impl<'a> Reader<'a> {
    fn byte(&mut self) -> ParseResult<u8> {
        let b = *self.bytes.get(self.pos).ok_or(ParseError)?;
        self.pos += 1;
        Ok(b)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn read_varint(&mut self) -> ParseResult<u64> {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            let c = self.byte()?;
            result |= ((c & 0x7F) as u64) << shift;
            if c & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift > 63 {
                return Err(ParseError);
            }
        }
    }

    fn zigzag(n: u64) -> i64 {
        ((n >> 1) as i64) ^ -((n & 1) as i64)
    }

    fn skip_padding(&mut self) {
        while self.peek() == Some(0) {
            self.pos += 1;
        }
    }

    fn read_value(&mut self, depth: u32) -> ParseResult<V8Value> {
        if depth > MAX_DEPTH {
            return Err(ParseError);
        }
        self.skip_padding();
        let mut tag = self.byte()?;
        // Skip version envelopes in a loop: recursing here would not raise
        // `depth`, so repeated envelopes could overflow the stack.
        while tag == 0xFF {
            self.read_varint()?;
            self.skip_padding();
            tag = self.byte()?;
        }
        match tag {
            b'_' | b'0' => Ok(V8Value::Null),
            b'T' => Ok(V8Value::Bool(true)),
            b'F' => Ok(V8Value::Bool(false)),
            b'I' => Ok(V8Value::Int(Self::zigzag(self.read_varint()?))),
            b'U' => Ok(V8Value::Int(self.read_varint()? as i64)),
            b'N' => self.read_double(),
            b'"' => Ok(V8Value::String(self.read_one_byte()?)),
            b'c' => Ok(V8Value::String(self.read_two_byte()?)),
            b'S' => Ok(V8Value::String(self.read_utf8()?)),
            b'o' => self.read_object(depth),
            b'A' => self.read_dense_array(depth),
            b'a' => self.read_sparse_array(depth),
            b'^' => {
                self.read_varint()?; // object reference
                Ok(V8Value::Null)
            }
            b'D' => {
                self.take(8)?; // date
                Ok(V8Value::Null)
            }
            b'R' => {
                self.read_string_raw()?;
                self.read_varint()?;
                Ok(V8Value::Null)
            }
            _ => Err(ParseError),
        }
    }

    fn read_double(&mut self) -> ParseResult<V8Value> {
        let raw = self.take(8)?;
        let bytes: [u8; 8] = raw.try_into().map_err(|_| ParseError)?;
        Ok(V8Value::Double(f64::from_le_bytes(bytes)))
    }

    fn take(&mut self, len: usize) -> ParseResult<&'a [u8]> {
        let end = self.pos.checked_add(len).ok_or(ParseError)?;
        let slice = self.bytes.get(self.pos..end).ok_or(ParseError)?;
        self.pos = end;
        Ok(slice)
    }

    fn read_string_raw(&mut self) -> ParseResult<String> {
        match self.byte()? {
            b'"' => self.read_one_byte(),
            b'c' => self.read_two_byte(),
            b'S' => self.read_utf8(),
            _ => Err(ParseError),
        }
    }

    fn read_one_byte(&mut self) -> ParseResult<String> {
        let len = self.read_varint()? as usize;
        let raw = self.take(len)?;
        // Latin-1: each byte is a code point.
        let text: String = raw.iter().map(|&b| b as char).collect();
        Ok(self.decryptor.decrypt_field(&text))
    }

    fn read_utf8(&mut self) -> ParseResult<String> {
        let len = self.read_varint()? as usize;
        let raw = self.take(len)?;
        let text = String::from_utf8(raw.to_vec()).map_err(|_| ParseError)?;
        Ok(self.decryptor.decrypt_field(&text))
    }

    fn read_two_byte(&mut self) -> ParseResult<String> {
        let len = self.read_varint()? as usize;
        let raw = self.take(len)?;
        if raw.len() % 2 != 0 {
            return Err(ParseError);
        }
        let units: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16(&units).map_err(|_| ParseError)
    }

    fn read_object(&mut self, depth: u32) -> ParseResult<V8Value> {
        let mut map = BTreeMap::new();
        loop {
            self.skip_padding();
            if self.peek() == Some(b'{') {
                self.pos += 1;
                self.read_varint()?; // property count
                break;
            }
            let key = self.read_value(depth + 1)?;
            let value = self.read_value(depth + 1)?;
            if let V8Value::String(name) = key {
                map.insert(name, value);
            }
        }
        Ok(V8Value::Object(map))
    }

    fn read_dense_array(&mut self, depth: u32) -> ParseResult<V8Value> {
        let count = self.read_varint()?;
        let mut items = Vec::new();
        for _ in 0..count {
            items.push(self.read_value(depth + 1)?);
        }
        // Trailing key/value properties until the '$' terminator.
        loop {
            self.skip_padding();
            if self.peek() == Some(b'$') {
                self.pos += 1;
                self.read_varint()?;
                self.read_varint()?;
                break;
            }
            self.read_value(depth + 1)?;
            self.read_value(depth + 1)?;
        }
        Ok(V8Value::Array(items))
    }

    fn read_sparse_array(&mut self, depth: u32) -> ParseResult<V8Value> {
        self.read_varint()?; // length
        loop {
            self.skip_padding();
            if self.peek() == Some(b'@') {
                self.pos += 1;
                self.read_varint()?;
                self.read_varint()?;
                break;
            }
            self.read_value(depth + 1)?;
            self.read_value(depth + 1)?;
        }
        Ok(V8Value::Array(Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_key() -> Decryptor {
        Decryptor::new(zeroize::Zeroizing::new(Vec::new()))
    }

    /// Builder for a V8 ValueSerializer blob, matching the tags the reader
    /// expects. Numbers use the signed `I` (zigzag varint) tag.
    #[derive(Default)]
    struct Blob {
        bytes: Vec<u8>,
    }

    impl Blob {
        fn varint(&mut self, mut n: u64) {
            loop {
                let mut byte = (n & 0x7F) as u8;
                n >>= 7;
                if n != 0 {
                    byte |= 0x80;
                }
                self.bytes.push(byte);
                if n == 0 {
                    break;
                }
            }
        }
        fn int(&mut self, n: i64) {
            self.bytes.push(b'I');
            let zigzag = ((n << 1) ^ (n >> 63)) as u64;
            self.varint(zigzag);
        }
        fn string(&mut self, s: &str) {
            self.bytes.push(b'S');
            self.varint(s.len() as u64);
            self.bytes.extend_from_slice(s.as_bytes());
        }
        fn begin_object(&mut self) {
            self.bytes.push(b'o');
        }
        fn end_object(&mut self, property_count: u64) {
            self.bytes.push(b'{');
            self.varint(property_count);
        }
        fn key(&mut self, name: &str) {
            self.string(name);
        }
        fn double(&mut self, d: f64) {
            self.bytes.push(b'N');
            self.bytes.extend_from_slice(&d.to_le_bytes());
        }
        fn begin_dense_array(&mut self, count: u64) {
            self.bytes.push(b'A');
            self.varint(count);
        }
        fn end_dense_array(&mut self) {
            self.bytes.push(b'$');
            self.varint(0);
            self.varint(0);
        }
    }

    #[test]
    fn reads_object_with_strings_ints_nested_object_and_array() {
        let mut blob = Blob::default();
        blob.begin_object();
        blob.key("local_id");
        blob.int(42);
        blob.key("label");
        blob.string("web-1");
        blob.key("port");
        blob.double(2222.0);
        blob.key("nested");
        {
            blob.begin_object();
            blob.key("username");
            blob.string("deploy");
            blob.end_object(1);
        }
        blob.key("list");
        {
            blob.begin_dense_array(2);
            blob.int(1);
            blob.int(2);
            blob.end_dense_array();
        }
        blob.end_object(5);

        let decryptor = no_key();
        let record = scan_value(&blob.bytes, &decryptor, None).unwrap();
        assert_eq!(record.get("local_id").unwrap().as_i64(), Some(42));
        assert_eq!(record.get("label").unwrap().as_str(), Some("web-1"));
        assert_eq!(record.get("port").unwrap().as_i64(), Some(2222));
        let nested = record.get("nested").unwrap().as_object().unwrap();
        assert_eq!(nested.get("username").unwrap().as_str(), Some("deploy"));
        assert!(matches!(record.get("list"), Some(V8Value::Array(a)) if a.len() == 2));
    }

    #[test]
    fn scan_value_sets_local_id_from_primary_key_when_absent() {
        let mut blob = Blob::default();
        blob.begin_object();
        blob.key("label");
        blob.string("prod");
        blob.key("parent_group");
        blob.bytes.push(b'0'); // null
        blob.end_object(2);

        let decryptor = no_key();
        let record = scan_value(&blob.bytes, &decryptor, Some(99)).unwrap();
        assert_eq!(record.get("local_id").unwrap().as_i64(), Some(99));
        assert_eq!(record.get("label").unwrap().as_str(), Some("prod"));
    }

    #[test]
    fn scan_value_skips_a_blob_without_record_keys() {
        let mut blob = Blob::default();
        blob.begin_object();
        blob.key("unrelated");
        blob.string("x");
        blob.end_object(1);
        assert!(scan_value(&blob.bytes, &no_key(), None).is_none());
    }

    #[test]
    fn many_version_envelopes_do_not_overflow_the_stack() {
        let mut bytes = vec![b'o'];
        for _ in 0..200_000 {
            bytes.extend_from_slice(&[0xFF, 0x0F]);
        }
        assert!(scan_value(&bytes, &no_key(), None).is_none());
    }
}
