//! WPA2 pre-shared keys: the PMK a passphrase gives for an SSID.
use crate::common::hex::decode_hex;
use sha1::Sha1;
use zeroize::Zeroizing;

pub type Pmk = Zeroizing<[u8; 32]>;

/// The PMK of a passphrase of 8 to 63 printable ASCII characters, or of a PSK given as 64 hex
/// digits, for `ssid`. The errors hold neither.
pub fn pmk_from_passphrase(passphrase: &str, ssid: &str) -> Result<Pmk, &'static str> {
    if ssid.is_empty() || ssid.len() > 32 {
        return Err("the SSID must be 1 to 32 octets");
    }
    if passphrase.len() == 64 && passphrase.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return decode_hex::<32>(passphrase).map(Zeroizing::new).ok_or("the PSK is not 64 hex digits");
    }
    if !(8..=63).contains(&passphrase.len()) || !passphrase.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return Err("the passphrase must be 8 to 63 printable ASCII characters, or the PSK 64 hex digits");
    }
    let mut pmk = Zeroizing::new([0u8; 32]);
    pbkdf2::pbkdf2_hmac::<Sha1>(passphrase.as_bytes(), ssid.as_bytes(), 4096, &mut pmk[..]);
    Ok(pmk)
}
