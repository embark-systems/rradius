//! NDR (DCE 1.1 RPC chapter 14), little-endian, as the few Netlogon calls need it: aligned
//! integers, unique pointers, conformant and varying arrays, and UTF-16 strings. Alignment is
//! counted from the start of the stub.

/// Why a stub could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NdrError(pub String);

impl std::fmt::Display for NdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "malformed NDR data: {}", self.0)
    }
}

/// The most elements an array may claim, so that a count from the wire cannot ask for more
/// memory than any answer needs.
pub const MAX_ELEMENTS: u32 = 4096;

#[derive(Default)]
pub struct Writer {
    buffer: Vec<u8>,
    /// The next unique pointer's referent identifier.
    referent: u32,
}

impl Writer {
    pub fn new() -> Self {
        Self { buffer: Vec::new(), referent: 0x0002_0000 }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buffer
    }

    pub fn align(&mut self, to: usize) {
        while !self.buffer.len().is_multiple_of(to) {
            self.buffer.push(0);
        }
    }

    pub fn u8(&mut self, value: u8) {
        self.buffer.push(value);
    }

    pub fn u16(&mut self, value: u16) {
        self.align(2);
        self.buffer.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u32(&mut self, value: u32) {
        self.align(4);
        self.buffer.extend_from_slice(&value.to_le_bytes());
    }

    pub fn bytes(&mut self, value: &[u8]) {
        self.buffer.extend_from_slice(value);
    }

    /// A non-null unique pointer: its referent comes later, where the caller writes it.
    pub fn pointer(&mut self) {
        let referent = self.referent;
        self.referent += 4;
        self.u32(referent);
    }

    pub fn null_pointer(&mut self) {
        self.u32(0);
    }

    /// A `[string] wchar_t *` referent: conformant and varying, with its terminating NUL.
    pub fn wide_string(&mut self, value: &str) {
        let units: Vec<u16> = value.encode_utf16().chain([0]).collect();
        let count = units.len() as u32;
        self.u32(count);
        self.u32(0);
        self.u32(count);
        for unit in units {
            self.buffer.extend_from_slice(&unit.to_le_bytes());
        }
    }

    /// The fixed part of an RPC_UNICODE_STRING: lengths in octets, then a pointer to the
    /// characters (null for an empty string). The characters go later, with `unicode_chars`.
    pub fn unicode_string_header(&mut self, value: &str) {
        let octets = (value.encode_utf16().count() * 2) as u16;
        self.u16(octets);
        self.u16(octets);
        if octets == 0 {
            self.null_pointer();
        } else {
            self.pointer();
        }
    }

    /// The deferred characters of an RPC_UNICODE_STRING written with `unicode_string_header`.
    pub fn unicode_chars(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        let units: Vec<u16> = value.encode_utf16().collect();
        let count = units.len() as u32;
        self.u32(count);
        self.u32(0);
        self.u32(count);
        for unit in units {
            self.buffer.extend_from_slice(&unit.to_le_bytes());
        }
    }

    /// The fixed part of a STRING (counted octets): lengths, then a pointer.
    pub fn octet_string_header(&mut self, value: &[u8]) {
        self.u16(value.len() as u16);
        self.u16(value.len() as u16);
        if value.is_empty() {
            self.null_pointer();
        } else {
            self.pointer();
        }
    }

    /// The deferred octets of a STRING written with `octet_string_header`.
    pub fn octet_chars(&mut self, value: &[u8]) {
        if value.is_empty() {
            return;
        }
        self.u32(value.len() as u32);
        self.u32(0);
        self.u32(value.len() as u32);
        self.bytes(value);
    }
}

pub struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    pub fn position(&self) -> usize {
        self.at
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.at
    }

    fn error<T>(&self, what: &str) -> Result<T, NdrError> {
        Err(NdrError(format!("{what} at offset {}", self.at)))
    }

    pub fn align(&mut self, to: usize) -> Result<(), NdrError> {
        let aligned = self.at.next_multiple_of(to);
        if aligned > self.data.len() {
            return self.error("truncated alignment");
        }
        self.at = aligned;
        Ok(())
    }

    pub fn bytes(&mut self, count: usize) -> Result<&'a [u8], NdrError> {
        let Some(end) = self.at.checked_add(count).filter(|end| *end <= self.data.len()) else {
            return self.error(&format!("{count} octets past the end"));
        };
        let value = &self.data[self.at..end];
        self.at = end;
        Ok(value)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], NdrError> {
        let mut value = [0u8; N];
        value.copy_from_slice(self.bytes(N)?);
        Ok(value)
    }

    pub fn u8(&mut self) -> Result<u8, NdrError> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, NdrError> {
        self.align(2)?;
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32, NdrError> {
        self.align(4)?;
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64, NdrError> {
        self.align(8)?;
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// A unique pointer: whether its referent follows later.
    pub fn pointer(&mut self) -> Result<bool, NdrError> {
        Ok(self.u32()? != 0)
    }

    /// A count from the wire, refused above `MAX_ELEMENTS`.
    pub fn count(&mut self) -> Result<u32, NdrError> {
        let count = self.u32()?;
        if count > MAX_ELEMENTS {
            return self.error(&format!("a count of {count}"));
        }
        Ok(count)
    }

    /// The header of a conformant varying array: the number of elements present, checked
    /// against the maximum and against `expected` when the containing structure gave one.
    pub fn varying(&mut self, expected: Option<u32>) -> Result<u32, NdrError> {
        let maximum = self.count()?;
        let offset = self.u32()?;
        let actual = self.count()?;
        if offset != 0 || actual > maximum || expected.is_some_and(|expected| expected != actual) {
            return self.error("inconsistent array lengths");
        }
        Ok(actual)
    }

    /// The deferred characters of an RPC_UNICODE_STRING whose length in octets was `octets`.
    pub fn unicode_chars(&mut self, octets: u16) -> Result<String, NdrError> {
        if !octets.is_multiple_of(2) {
            return self.error("an odd string length");
        }
        let count = self.varying(Some(u32::from(octets / 2)))?;
        let raw = self.bytes(count as usize * 2)?;
        let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        Ok(String::from_utf16_lossy(&units))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alignment_and_strings() -> Result<(), NdrError> {
        let mut writer = Writer::new();
        writer.u8(1);
        writer.u32(0x0403_0201);
        writer.wide_string("ab");
        let bytes = writer.into_bytes();
        assert_eq!(bytes, [1, 0, 0, 0, 1, 2, 3, 4, 3, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, b'a', 0, b'b', 0, 0, 0]);
        let mut reader = Reader::new(&bytes);
        assert_eq!((reader.u8()?, reader.u32()?), (1, 0x0403_0201));
        assert_eq!(reader.varying(None)?, 3);
        Ok(())
    }

    #[test]
    fn test_unicode_string_round_trip() -> Result<(), NdrError> {
        let mut writer = Writer::new();
        writer.unicode_string_header("CORP");
        writer.unicode_string_header("");
        writer.unicode_chars("CORP");
        writer.unicode_chars("");
        let bytes = writer.into_bytes();
        let mut reader = Reader::new(&bytes);
        let (length, _, present) = (reader.u16()?, reader.u16()?, reader.pointer()?);
        assert_eq!((length, present), (8, true));
        let (empty, _, absent) = (reader.u16()?, reader.u16()?, reader.pointer()?);
        assert_eq!((empty, absent), (0, false));
        assert_eq!(reader.unicode_chars(length)?, "CORP");
        assert_eq!(reader.remaining(), 0);
        Ok(())
    }

    #[test]
    fn test_reader_refuses() {
        assert!(Reader::new(&[1, 2]).u32().is_err());
        // A count above the limit.
        assert!(Reader::new(&[0, 0, 1, 0]).count().is_err());
        // Actual above maximum.
        let mut bytes = Vec::new();
        for value in [2u32, 0, 3] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        assert!(Reader::new(&bytes).varying(None).is_err());
        // A length that does not match the structure's.
        let mut bytes = Vec::new();
        for value in [2u32, 0, 2] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&[b'a', 0, b'b', 0]);
        assert!(Reader::new(&bytes).unicode_chars(2).is_err());
        assert!(Reader::new(&bytes).unicode_chars(3).is_err());
        assert!(Reader::new(&[1, 2, 3]).bytes(usize::MAX).is_err());
    }
}
