//! Minimal protobuf wire-format reader and writer for vector tiles.
//!
//! The reader never panics and never loops without consuming input: every
//! read is bounds-checked and returns [`DecodeError`] on truncated or
//! malformed data, so it is safe on untrusted bytes.

use thiserror::Error;

/// Malformed protobuf input.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DecodeError {
    #[error("unexpected end of data at byte {0}")]
    Truncated(usize),
    #[error("varint longer than 10 bytes at byte {0}")]
    VarintTooLong(usize),
    #[error("unsupported wire type {wire} at byte {at}")]
    WireType { wire: u8, at: usize },
    #[error("invalid {what}: {detail}")]
    Invalid { what: &'static str, detail: String },
}

/// Bounds-checked cursor over a protobuf message.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

/// A decoded field value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Field<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn varint(&mut self) -> Result<u64, DecodeError> {
        let start = self.pos;
        let mut result = 0u64;
        for i in 0..10 {
            let b = *self
                .data
                .get(self.pos)
                .ok_or(DecodeError::Truncated(self.pos))?;
            self.pos += 1;
            result |= u64::from(b & 0x7f) << (7 * i);
            if b < 0x80 {
                return Ok(result);
            }
        }
        Err(DecodeError::VarintTooLong(start))
    }

    /// Read exactly `n` raw bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        self.take(n)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or(DecodeError::Truncated(self.pos))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    /// Next `(field_number, value)`, or `None` at the end of the message.
    pub fn next_field(&mut self) -> Result<Option<(u64, Field<'a>)>, DecodeError> {
        if self.is_empty() {
            return Ok(None);
        }
        let at = self.pos;
        let key = self.varint()?;
        let field = key >> 3;
        let value = match (key & 7) as u8 {
            0 => Field::Varint(self.varint()?),
            1 => {
                let b = self.take(8)?;
                Field::Fixed64(u64::from_le_bytes(
                    b.try_into().map_err(|_| DecodeError::Truncated(at))?,
                ))
            }
            2 => {
                let len =
                    usize::try_from(self.varint()?).map_err(|_| DecodeError::Truncated(at))?;
                Field::Bytes(self.take(len)?)
            }
            5 => {
                let b = self.take(4)?;
                Field::Fixed32(u32::from_le_bytes(
                    b.try_into().map_err(|_| DecodeError::Truncated(at))?,
                ))
            }
            wire => return Err(DecodeError::WireType { wire, at }),
        };
        Ok(Some((field, value)))
    }
}

/// Iterate a packed repeated varint field.
pub fn packed_varints(bytes: &[u8]) -> impl Iterator<Item = Result<u64, DecodeError>> + '_ {
    let mut r = Reader::new(bytes);
    std::iter::from_fn(move || (!r.is_empty()).then(|| r.varint()))
}

pub fn zigzag_decode32(n: u32) -> i32 {
    ((n >> 1) as i32) ^ -((n & 1) as i32)
}

pub fn zigzag_encode32(n: i32) -> u32 {
    ((n << 1) ^ (n >> 31)) as u32
}

// ── Writer ──────────────────────────────────────────────────────────────────

pub fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn put_key(buf: &mut Vec<u8>, field: u32, wire: u8) {
    put_varint(buf, (u64::from(field) << 3) | u64::from(wire));
}

pub fn put_varint_field(buf: &mut Vec<u8>, field: u32, v: u64) {
    put_key(buf, field, 0);
    put_varint(buf, v);
}

pub fn put_bytes_field(buf: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_key(buf, field, 2);
    put_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

pub fn put_fixed64_field(buf: &mut Vec<u8>, field: u32, v: u64) {
    put_key(buf, field, 1);
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Write a length-delimited field whose body is produced by `body`.
pub fn put_message(buf: &mut Vec<u8>, field: u32, body: impl FnOnce(&mut Vec<u8>)) {
    let mut inner = Vec::new();
    body(&mut inner);
    put_bytes_field(buf, field, &inner);
}

pub fn put_packed_varints(buf: &mut Vec<u8>, field: u32, values: impl IntoIterator<Item = u64>) {
    let mut packed = Vec::new();
    for v in values {
        put_varint(&mut packed, v);
    }
    if !packed.is_empty() {
        put_bytes_field(buf, field, &packed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trip_and_limits() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            assert_eq!(Reader::new(&buf).varint(), Ok(v));
        }
        // 11 continuation bytes: rejected, not shifted out of range.
        let long = [0xffu8; 11];
        assert_eq!(
            Reader::new(&long).varint(),
            Err(DecodeError::VarintTooLong(0))
        );
        assert_eq!(
            Reader::new(&[0x80]).varint(),
            Err(DecodeError::Truncated(1))
        );
    }

    #[test]
    fn length_beyond_buffer_is_an_error() {
        // Field 1, wire 2, length 100, but only 2 bytes follow.
        let data = [0x0a, 100, 1, 2];
        assert!(matches!(
            Reader::new(&data).next_field(),
            Err(DecodeError::Truncated(_))
        ));
        // Length that overflows usize arithmetic.
        let mut huge = vec![0x0a];
        put_varint(&mut huge, u64::MAX);
        assert!(Reader::new(&huge).next_field().is_err());
    }

    #[test]
    fn zigzag_round_trip() {
        for v in [0, 1, -1, i32::MAX, i32::MIN, 12345, -98765] {
            assert_eq!(zigzag_decode32(zigzag_encode32(v)), v);
        }
    }

    #[test]
    fn unknown_wire_type_is_rejected() {
        let data = [0x0b]; // wire type 3 (deprecated group start)
        assert!(matches!(
            Reader::new(&data).next_field(),
            Err(DecodeError::WireType { .. })
        ));
    }
}
