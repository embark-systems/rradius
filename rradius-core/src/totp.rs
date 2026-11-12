//! Time-based one-time passwords per RFC 6238, with what authenticator apps use:
//! HMAC-SHA-1, six digits, a 30-second step and a Base32 secret.
use crate::passwords::constant_time_eq;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

pub const STEP_SECONDS: u64 = 30;
/// Steps accepted on either side of the current one, for clocks that differ.
pub const WINDOW: u64 = 1;

/// Decodes RFC 4648 Base32, in either case and with or without padding.
pub fn decode_base32(text: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() * 5 / 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in text.trim_end_matches('=').bytes() {
        let value = match c.to_ascii_uppercase() {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | u32::from(value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(bytes)
}

/// RFC 4648 Base32 in capitals, without padding, as authenticator apps take a secret.
pub fn encode_base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut text = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            text.push(char::from(ALPHABET[((buffer >> bits) & 31) as usize]));
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        text.push(char::from(ALPHABET[((buffer << (5 - bits)) & 31) as usize]));
    }
    text
}

/// The six digits of HOTP (RFC 4226) for `counter`; TOTP's counter is the time step. `None`
/// only for a key HMAC cannot take, which no key length is.
pub fn hotp(key: &[u8], counter: u64) -> Option<String> {
    code_for_step(key, counter)
}

/// The six digits for a counter value, per RFC 4226 section 5.3.
fn code_for_step(key: &[u8], step: u64) -> Option<String> {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).ok()?;
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[digest.len() - 1] & 0x0f);
    let word = u32::from_be_bytes([digest[offset], digest[offset + 1], digest[offset + 2], digest[offset + 3]]);
    Some(format!("{:06}", (word & 0x7fff_ffff) % 1_000_000))
}

/// The step `code` belongs to, at `now` seconds after the epoch, or `None`. Every step in
/// the window is compared, so the time taken says nothing about which one matched.
pub fn verify(secret: &str, code: &[u8], now: u64) -> Result<Option<u64>, String> {
    // An empty key would let anyone compute the code, so it is refused like a malformed one.
    let key = decode_base32(secret).filter(|key| !key.is_empty()).ok_or_else(|| "totp_verify: the secret is not valid Base32".to_string())?;
    let current = now / STEP_SECONDS;
    let mut matched = None;
    for step in current.saturating_sub(WINDOW)..=current + WINDOW {
        let expected = code_for_step(&key, step).ok_or_else(|| "totp_verify: unusable secret".to_string())?;
        if constant_time_eq(expected.as_bytes(), code) {
            matched = Some(step);
        }
    }
    Ok(matched)
}

/// The code at `now`, for tests of what scripts do with one.
#[cfg(test)]
pub(crate) fn code_at(secret: &str, now: u64) -> String {
    code_for_step(&decode_base32(secret).unwrap(), now / STEP_SECONDS).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Base32 of the RFC 6238 appendix B secret `12345678901234567890`.
    const SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    #[test]
    fn test_base32() {
        assert_eq!(decode_base32(SECRET).unwrap(), b"12345678901234567890");
        assert_eq!(decode_base32("gezdgnbv").unwrap(), b"12345");
        assert_eq!(decode_base32("MZXW6===").unwrap(), b"foo");
        assert!(decode_base32("not base32!").is_none());
        assert!(decode_base32("1").is_none(), "0, 1, 8 and 9 are not in the alphabet");
    }

    // RFC 6238 appendix B, SHA-1, last six digits of the eight given there.
    #[test]
    fn test_rfc6238_vectors() {
        for (time, code) in [
            (59, "287082"),
            (1111111109, "081804"),
            (1111111111, "050471"),
            (1234567890, "005924"),
            (2000000000, "279037"),
            (20000000000, "353130"),
        ] {
            assert_eq!(verify(SECRET, code.as_bytes(), time).unwrap(), Some(time / 30), "{time}");
        }
    }

    // RFC 4226 appendix D, the secret `12345678901234567890`.
    #[test]
    fn test_rfc4226_vectors() {
        let codes = ["755224", "287082", "359152", "969429", "338314", "254676", "287922", "162583", "399871", "520489"];
        for (counter, code) in codes.iter().enumerate() {
            assert_eq!(hotp(b"12345678901234567890", counter as u64).as_deref(), Some(*code), "counter {counter}");
        }
    }

    #[test]
    fn test_base32_encoding() {
        assert_eq!(encode_base32(b"12345678901234567890"), SECRET);
        assert_eq!(encode_base32(b"foo"), "MZXW6");
        assert_eq!(encode_base32(b""), "");
        for length in 0..=21 {
            let bytes: Vec<u8> = (0..length).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(decode_base32(&encode_base32(&bytes)).unwrap(), bytes, "{length} octets");
        }
    }

    #[test]
    fn test_window() {
        let now = 10 * 30;
        for (from_step, result) in [(8, None), (9, Some(9)), (10, Some(10)), (11, Some(11)), (12, None)] {
            let code = code_at(SECRET, from_step * 30);
            assert_eq!(verify(SECRET, code.as_bytes(), now).unwrap(), result, "code from step {from_step}");
        }
    }

    #[test]
    fn test_window_at_the_epoch() {
        assert_eq!(verify(SECRET, code_at(SECRET, 0).as_bytes(), 0).unwrap(), Some(0));
    }

    #[test]
    fn test_wrong_codes() {
        for code in ["000000", "28708", "2870822", "abcdef", ""] {
            assert_eq!(verify(SECRET, code.as_bytes(), 59).unwrap(), None, "{code}");
        }
    }

    #[test]
    fn test_invalid_secret() {
        assert!(verify("not base32!", b"123456", 59).is_err());
        // With no key, anyone could compute the code.
        assert!(verify("", b"123456", 59).is_err());
        assert!(verify("===", b"123456", 59).is_err());
    }
}
