//! MS-CHAPv2 as a RADIUS server needs it: RFC 2759 section 8 for the response and the
//! authenticator response, RFC 3079 section 3 for the session keys.
use crate::passwords::{constant_time_eq, split_scheme};
use des::cipher::{BlockCipherEncrypt, KeyInit};
use des::Des;
use md4::{Digest as _, Md4};
use sha1::Sha1;
use zeroize::{Zeroize, Zeroizing};

const AUTHENTICATOR_MAGIC1: &[u8] = b"Magic server to client signing constant";
const AUTHENTICATOR_MAGIC2: &[u8] = b"Pad to make it do more than one iteration";
const MASTER_KEY_MAGIC: &[u8] = b"This is the MPPE Master Key";
const CLIENT_SEND_SERVER_RECEIVE: &[u8] = b"On the client side, this is the send key; on the server side, it is the receive key.";
const CLIENT_RECEIVE_SERVER_SEND: &[u8] = b"On the client side, this is the receive key; on the server side, it is the send key.";
const SHS_PAD1: [u8; 40] = [0x00; 40];
const SHS_PAD2: [u8; 40] = [0xF2; 40];

/// What a verified response proves, for the Access-Accept.
pub struct Proof {
    /// `S=` and 40 hexadecimal digits.
    pub authenticator_response: String,
    pub send_key: Zeroizing<[u8; 16]>,
    pub recv_key: Zeroizing<[u8; 16]>,
}

pub(crate) fn nt_password_hash(password: &str) -> Zeroizing<[u8; 16]> {
    // Sized first: growing would leave part of it behind. A UTF-8 octet is at most 2 in UTF-16.
    let mut unicode = Zeroizing::new(Vec::with_capacity(password.len() * 2));
    unicode.extend(password.encode_utf16().flat_map(u16::to_le_bytes));
    Zeroizing::new(Md4::digest(&*unicode).into())
}

pub(crate) fn hash_nt_password_hash(hash: &[u8; 16]) -> Zeroizing<[u8; 16]> {
    Zeroizing::new(Md4::digest(hash).into())
}

/// RFC 2759 section 8.2. Only the user name goes in, without any domain.
pub(crate) fn challenge_hash(peer_challenge: &[u8; 16], authenticator_challenge: &[u8; 16], user: &str) -> [u8; 8] {
    let name = user.rsplit('\\').next().unwrap_or(user);
    let mut hasher = Sha1::new();
    hasher.update(peer_challenge);
    hasher.update(authenticator_challenge);
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    let mut challenge = [0u8; 8];
    challenge.copy_from_slice(&digest[..8]);
    challenge
}

/// Spreads 7 key octets over 8, RFC 2759 section 8.6. DES ignores the parity bit.
fn des_key(key: &[u8]) -> [u8; 8] {
    [
        key[0],
        (key[0] << 7) | (key[1] >> 1),
        (key[1] << 6) | (key[2] >> 2),
        (key[2] << 5) | (key[3] >> 3),
        (key[3] << 4) | (key[4] >> 4),
        (key[4] << 3) | (key[5] >> 5),
        (key[5] << 2) | (key[6] >> 6),
        key[6] << 1,
    ]
}

/// RFC 2759 section 8.5.
pub(crate) fn challenge_response(challenge: &[u8; 8], hash: &[u8; 16]) -> Option<[u8; 24]> {
    let mut padded = Zeroizing::new([0u8; 21]);
    padded[..16].copy_from_slice(hash);
    let mut response = [0u8; 24];
    for (key, out) in padded.chunks(7).zip(response.chunks_mut(8)) {
        let cipher = Des::new_from_slice(&*Zeroizing::new(des_key(key))).ok()?;
        out.copy_from_slice(challenge);
        cipher.encrypt_block(out.try_into().ok()?);
    }
    Some(response)
}

/// RFC 2759 section 8.7.
fn authenticator_response(hash_hash: &[u8; 16], nt_response: &[u8; 24], challenge: &[u8; 8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(hash_hash);
    hasher.update(nt_response);
    hasher.update(AUTHENTICATOR_MAGIC1);
    let first = hasher.finalize();
    let mut hasher = Sha1::new();
    hasher.update(first);
    hasher.update(challenge);
    hasher.update(AUTHENTICATOR_MAGIC2);
    let digest: String = hasher.finalize().iter().map(|b| format!("{b:02X}")).collect();
    format!("S={digest}")
}

/// RFC 3079 section 3.4: GetMasterKey.
fn master_key(hash_hash: &[u8; 16], nt_response: &[u8; 24]) -> Zeroizing<[u8; 16]> {
    let mut hasher = Sha1::new();
    hasher.update(hash_hash);
    hasher.update(nt_response);
    hasher.update(MASTER_KEY_MAGIC);
    let mut digest = hasher.finalize();
    let mut key = Zeroizing::new([0u8; 16]);
    key.copy_from_slice(&digest[..16]);
    digest.as_mut_slice().zeroize();
    key
}

/// RFC 3079 section 3.4: GetAsymmetricStartKey, for 128-bit keys.
fn start_key(master: &[u8; 16], magic: &[u8]) -> Zeroizing<[u8; 16]> {
    let mut hasher = Sha1::new();
    hasher.update(master);
    hasher.update(SHS_PAD1);
    hasher.update(magic);
    hasher.update(SHS_PAD2);
    let mut digest = hasher.finalize();
    let mut key = Zeroizing::new([0u8; 16]);
    key.copy_from_slice(&digest[..16]);
    digest.as_mut_slice().zeroize();
    key
}

/// An NT hash written as 32 hexadecimal digits.
pub(crate) fn parse_nt_hash(value: &str) -> Option<Zeroizing<[u8; 16]>> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut hash = Zeroizing::new([0u8; 16]);
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

/// The NT hash of a stored password, if it can be known: plaintext, `{clear}` or
/// `{nthash}`. Salted and one-way formats cannot be used with MS-CHAPv2.
fn stored_hash(stored: &str) -> Option<Zeroizing<[u8; 16]>> {
    let (scheme, value) = split_scheme(stored);
    match scheme.as_deref() {
        None | Some("clear") => Some(nt_password_hash(value)),
        Some("nthash") => parse_nt_hash(value),
        Some(_) => None,
    }
}

/// Checks an MS-CHAPv2 response against a stored password.
pub fn verify(stored: &str, user: &str, peer_challenge: &[u8; 16], authenticator_challenge: &[u8; 16], response: &[u8; 24]) -> Option<Proof> {
    let hash = stored_hash(stored)?;
    let challenge = challenge_hash(peer_challenge, authenticator_challenge, user);
    let expected = challenge_response(&challenge, &hash)?;
    if !constant_time_eq(&expected, response) {
        return None;
    }
    Some(proof(&hash_nt_password_hash(&hash), response, &challenge))
}

/// What a domain controller's acceptance proves: the user session key it returns for an
/// MS-CHAPv2 response is the hash of the NT hash, which is all the proof is computed from.
/// `challenge` is the challenge hash the response answers. An all-zero key is no proof.
pub fn proof_from_session_key(user_session_key: &[u8; 16], response: &[u8; 24], challenge: &[u8; 8]) -> Option<Proof> {
    if user_session_key.iter().all(|byte| *byte == 0) {
        return None;
    }
    Some(proof(user_session_key, response, challenge))
}

fn proof(hash_hash: &[u8; 16], response: &[u8; 24], challenge: &[u8; 8]) -> Proof {
    let master = master_key(hash_hash, response);
    Proof {
        authenticator_response: authenticator_response(hash_hash, response, challenge),
        send_key: start_key(&master, CLIENT_RECEIVE_SERVER_SEND),
        recv_key: start_key(&master, CLIENT_SEND_SERVER_RECEIVE),
    }
}

/// The response a client would send, for tests elsewhere.
#[cfg(test)]
pub(crate) fn response_for(password: &str, user: &str, peer_challenge: &[u8; 16], authenticator_challenge: &[u8; 16]) -> [u8; 24] {
    let challenge = challenge_hash(peer_challenge, authenticator_challenge, user);
    challenge_response(&challenge, &nt_password_hash(password)).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 2759 section 9.2, and the same exchange in RFC 3079 section 3.5.3.
    const AUTH_CHALLENGE: [u8; 16] = [0x5B, 0x5D, 0x7C, 0x7D, 0x7B, 0x3F, 0x2F, 0x3E, 0x3C, 0x2C, 0x60, 0x21, 0x32, 0x26, 0x26, 0x28];
    const PEER_CHALLENGE: [u8; 16] = [0x21, 0x40, 0x23, 0x24, 0x25, 0x5E, 0x26, 0x2A, 0x28, 0x29, 0x5F, 0x2B, 0x3A, 0x33, 0x7C, 0x7E];
    const NT_RESPONSE: [u8; 24] = [
        0x82, 0x30, 0x9E, 0xCD, 0x8D, 0x70, 0x8B, 0x5E, 0xA0, 0x8F, 0xAA, 0x39, 0x81, 0xCD, 0x83, 0x54, 0x42, 0x33, 0x11, 0x4A, 0x3D, 0x85, 0xD6, 0xDF,
    ];
    const PASSWORD_HASH: [u8; 16] = [0x44, 0xEB, 0xBA, 0x8D, 0x53, 0x12, 0xB8, 0xD6, 0x11, 0x47, 0x44, 0x11, 0xF5, 0x69, 0x89, 0xAE];
    const AUTH_RESPONSE: &str = "S=407A5589115FD0D6209F510FE9C04566932CDA56";

    #[test]
    fn test_rfc2759_intermediate_values() {
        assert_eq!(*nt_password_hash("clientPass"), PASSWORD_HASH);
        assert_eq!(challenge_hash(&PEER_CHALLENGE, &AUTH_CHALLENGE, "User"), [0xD0, 0x2E, 0x43, 0x86, 0xBC, 0xE9, 0x12, 0x26]);
        assert_eq!(challenge_response(&[0xD0, 0x2E, 0x43, 0x86, 0xBC, 0xE9, 0x12, 0x26], &PASSWORD_HASH).unwrap(), NT_RESPONSE);
        assert_eq!(*hash_nt_password_hash(&PASSWORD_HASH), [0x41, 0xC0, 0x0C, 0x58, 0x4B, 0xD2, 0xD9, 0x1C, 0x40, 0x17, 0xA2, 0xA1, 0x2F, 0xA5, 0x9F, 0x3F]);
    }

    #[test]
    fn test_rfc3079_keys() {
        let master = master_key(&hash_nt_password_hash(&PASSWORD_HASH), &NT_RESPONSE);
        assert_eq!(*master, [0xFD, 0xEC, 0xE3, 0x71, 0x7A, 0x8C, 0x83, 0x8C, 0xB3, 0x88, 0xE5, 0x27, 0xAE, 0x3C, 0xDD, 0x31]);
        // The RFC's example is the server's send key. It gives none for the receive key.
        assert_eq!(*start_key(&master, CLIENT_RECEIVE_SERVER_SEND), [0x8B, 0x7C, 0xDC, 0x14, 0x9B, 0x99, 0x3A, 0x1B, 0xA1, 0x18, 0xCB, 0x15, 0x3F, 0x56, 0xDC, 0xCB]);
    }

    #[test]
    fn test_verify() {
        let proof = verify("clientPass", "User", &PEER_CHALLENGE, &AUTH_CHALLENGE, &NT_RESPONSE).unwrap();
        assert_eq!(proof.authenticator_response, AUTH_RESPONSE);
        assert_ne!(proof.send_key, proof.recv_key);
        let same = |other: Proof| other.authenticator_response == proof.authenticator_response && other.send_key == proof.send_key && other.recv_key == proof.recv_key;
        for stored in ["{clear}clientPass", "{nthash}44EBBA8D5312B8D611474411F56989AE", "{nthash}44ebba8d5312b8d611474411f56989ae"] {
            assert!(same(verify(stored, "User", &PEER_CHALLENGE, &AUTH_CHALLENGE, &NT_RESPONSE).unwrap()), "{stored}");
        }
        assert!(same(verify("clientPass", "BIGCO\\User", &PEER_CHALLENGE, &AUTH_CHALLENGE, &NT_RESPONSE).unwrap()));
    }

    /// Scenario: Same proof as local verification, from the key a domain controller returns.
    #[test]
    fn test_proof_from_session_key() -> Result<(), String> {
        let local = verify("clientPass", "User", &PEER_CHALLENGE, &AUTH_CHALLENGE, &NT_RESPONSE).ok_or("not verified")?;
        let session_key = hash_nt_password_hash(&nt_password_hash("clientPass"));
        let challenge = challenge_hash(&PEER_CHALLENGE, &AUTH_CHALLENGE, "User");
        let proof = proof_from_session_key(&session_key, &NT_RESPONSE, &challenge).ok_or("no proof")?;
        assert_eq!(proof.authenticator_response, AUTH_RESPONSE);
        assert_eq!((proof.send_key, proof.recv_key), (local.send_key, local.recv_key));
        assert!(proof_from_session_key(&[0; 16], &NT_RESPONSE, &challenge).is_none());
        Ok(())
    }

    #[test]
    fn test_parse_nt_hash() {
        assert_eq!(*parse_nt_hash("44EBBA8D5312B8D611474411F56989AE").unwrap(), PASSWORD_HASH);
        assert_eq!(*parse_nt_hash("44ebba8d5312b8d611474411f56989ae").unwrap(), PASSWORD_HASH);
        for bad in ["", "44EBBA8D", "+4EBBA8D5312B8D611474411F56989AE", "44EBBA8D5312B8D611474411F56989AG", "44EBBA8D5312B8D611474411F56989A\u{e9}"] {
            assert!(parse_nt_hash(bad).is_none(), "{bad}");
        }
    }

    /// Does not compile if any of these is held in memory nobody clears.
    #[test]
    fn test_what_stands_for_the_password_is_wiped() {
        fn cleared<T: zeroize::ZeroizeOnDrop>(_: &T) {}
        let hash = nt_password_hash("clientPass");
        cleared(&hash);
        cleared(&hash_nt_password_hash(&hash));
        cleared(&stored_hash("clientPass").unwrap());
        let master = master_key(&hash_nt_password_hash(&hash), &NT_RESPONSE);
        cleared(&master);
        cleared(&start_key(&master, CLIENT_RECEIVE_SERVER_SEND));
        let proof = verify("clientPass", "User", &PEER_CHALLENGE, &AUTH_CHALLENGE, &NT_RESPONSE).unwrap();
        cleared(&proof.send_key);
        cleared(&proof.recv_key);
    }

    #[test]
    fn test_verify_fails() {
        for stored in ["other", "{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=", "{crypt}abJnggxhB/yWI", "{nthash}nothex", "{nthash}44EBBA8D5312B8D611474411F56989"] {
            assert!(verify(stored, "User", &PEER_CHALLENGE, &AUTH_CHALLENGE, &NT_RESPONSE).is_none(), "{stored}");
        }
        assert!(verify("clientPass", "Other", &PEER_CHALLENGE, &AUTH_CHALLENGE, &NT_RESPONSE).is_none());
    }
}
