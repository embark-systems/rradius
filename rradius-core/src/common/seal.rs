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

/// What to say about a file that holds a password or a key but is not refused for its mode:
/// the line `warn_if_open` logs, or `None` when the file is fine, cannot be looked at, or the
/// system has no Unix modes. A bit of `0o027` is what `read_secret_file` refuses; here it only
/// gets a line. The line names the setting, the path and the mode, never what the file holds.
pub fn open_file_warning(setting: &str, path: &std::path::Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A file that cannot be looked at cannot be read either: the read reports that.
        let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
        if mode & 0o027 != 0 {
            return Some(format!("{setting}: '{}' has mode {:o}, which others can reach or its group can write; make it 0600 or 0640", path.display(), mode));
        }
    }
    #[cfg(not(unix))]
    let _ = (setting, path);
    None
}

/// Logs one WARN line when `open_file_warning` has something to say, and goes on: the file is read
/// as before. For the files whose mode is not refused (a client's `secret_file`, the passwords of
/// the cluster, a database, LDAP and Netlogon): the example configurations are checked out as 0644.
pub fn warn_if_open(setting: &str, path: &std::path::Path) {
    if let Some(line) = open_file_warning(setting, path) {
        tracing::warn!("{line}");
    }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A file holding `hunter2-secret` with the given mode.
    fn file_with_mode(dir: &std::path::Path, name: &str, mode: u32) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
        let path = dir.join(name);
        std::fs::write(&path, "hunter2-secret\n")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        Ok(path)
    }

    #[test]
    fn test_open_file_warning_by_mode() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        for (mode, warns) in [(0o600, false), (0o640, false), (0o400, false), (0o644, true), (0o660, true), (0o666, true)] {
            let path = file_with_mode(dir.path(), &format!("f{mode:o}"), mode)?;
            let line = open_file_warning("secret_file", &path);
            assert_eq!(line.is_some(), warns, "mode {mode:o}: {line:?}");
            if let Some(line) = line {
                assert!(line.contains("secret_file") && line.contains(&path.display().to_string()) && line.contains(&format!("{mode:o}")), "unexpected line: {line}");
                assert!(!line.contains("hunter2"), "the line holds the file's text: {line}");
            }
        }
        Ok(())
    }

    #[test]
    fn test_open_file_warning_says_nothing_about_a_file_it_cannot_look_at() {
        assert_eq!(open_file_warning("secret_file", std::path::Path::new("/nonexistent/rradius/secret")), None);
    }

    #[test]
    fn test_read_secret_file_still_refuses_what_warn_only_warns_about() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = file_with_mode(dir.path(), "key", 0o644)?;
        assert!(read_secret_file(&path).is_err());
        assert!(open_file_warning("key_file", &path).is_some());
        Ok(())
    }
}
