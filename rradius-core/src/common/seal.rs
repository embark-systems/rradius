//! Sealing secrets kept in the database with a master key the database does not hold: AES-256-GCM
//! under a key read from a file, so that the database, its backups and its replicas see only
//! ciphertext.
use crate::common::hex::{decode_hex, decode_hex_vec, encode_hex};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// A master key secrets are sealed under: Ki and OPc, DPSK PMKs, session ticket keys. It has no
/// `Debug`.
pub struct SealKey {
    id: [u8; 4],
    key: LessSafeKey,
}

impl SealKey {
    /// From a key file's text: 64 hex digits and maybe a line end. The error holds none of it.
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        let digits = text.trim_end_matches(['\r', '\n']);
        let bytes = Zeroizing::new(decode_hex::<32>(digits).ok_or("it does not hold 64 hex digits")?);
        let digest = Sha256::digest(&bytes[..]);
        let id = [digest[0], digest[1], digest[2], digest[3]];
        let key = UnboundKey::new(&AES_256_GCM, &bytes[..]).map_err(|_| "it cannot be used as a key")?;
        Ok(Self { id, key: LessSafeKey::new(key) })
    }
}

impl SealKey {
    /// `<version>:<key id>:<nonce>:<ciphertext and tag>`: the caller gives the version, which also
    /// starts `aad`, so that one kind never opens as another (Ki and OPc use `aka1`, DPSK PMKs
    /// `psk1`).
    pub fn seal_value(&self, version: &str, aad: &[u8], value: &[u8]) -> Result<String, &'static str> {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(|_| "no random nonce")?;
        let mut sealed = value.to_vec();
        self.key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut sealed).map_err(|_| "it cannot be sealed")?;
        Ok(format!("{version}:{}:{}:{}", encode_hex(&self.id), encode_hex(&nonce), encode_hex(&sealed)))
    }
}

/// What `seal_value` sealed with this version and `aad`, under whichever of `keys` it names. The
/// errors hold none of it.
pub fn open_value(keys: &[&SealKey], version: &str, aad: &[u8], sealed: &str) -> Result<Zeroizing<Vec<u8>>, &'static str> {
    let mut parts = sealed.split(':');
    let (Some(found), Some(id), Some(nonce), Some(body), None) = (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err("not a sealed value");
    };
    if found != version {
        return Err("not a sealed value");
    }
    let id = decode_hex::<4>(id).ok_or("not a sealed value")?;
    let nonce = decode_hex::<12>(nonce).ok_or("not a sealed value")?;
    let mut body = Zeroizing::new(decode_hex_vec(body).ok_or("not a sealed value")?);
    let key = keys.iter().find(|key| key.id == id).ok_or("sealed under a key this server does not have")?;
    // In place, and the tag cut off: no copy of what it holds is left anywhere else.
    let length = key.key.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut body).map_err(|_| "it does not open")?.len();
    body.truncate(length);
    Ok(body)
}

/// A master key from its file. No error holds what is in it.
pub fn read_key_file(path: &std::path::Path) -> Result<SealKey, String> {
    // Ki in the database and this key beside it are all it takes to copy every card.
    let text = read_secret_file(path)?;
    SealKey::parse(&text).map_err(|e| format!("'{}': {e}", path.display()))
}

/// The text of a file holding a key, refused when others can reach it. Group read is fine, for a
/// file root owns and the service's group reads. Errors name the file and never hold its text.
pub fn read_secret_file(path: &std::path::Path) -> Result<Zeroizing<String>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).map_err(|e| format!("'{}': {e}", path.display()))?.permissions().mode();
        if mode & 0o027 != 0 {
            return Err(format!("'{}': others can reach it (mode {:o}); make it 0600 or 0640", path.display(), mode & 0o777));
        }
    }
    Ok(Zeroizing::new(std::fs::read_to_string(path).map_err(|e| format!("'{}': {e}", path.display()))?))
}
