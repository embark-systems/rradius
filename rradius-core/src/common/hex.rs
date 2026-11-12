//! Hexadecimal text, strictly: two digits an octet, nothing else.

pub fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn decode_hex_vec(text: &str) -> Option<Vec<u8>> {
    // Not `from_str_radix` alone: that takes a leading `+`.
    if !text.len().is_multiple_of(2) || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

/// Exactly `N` octets of hex.
pub fn decode_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    // No `Vec` on the way: this is how master keys and Ki arrive, and nothing would clear it.
    if text.len() != 2 * N || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}
