//! The cryptography of an AES Netlogon secure channel (MS-NRPC): the session key (3.1.4.3),
//! credentials (3.1.4.4.2), the sealing and signing of RPC messages (3.3.4.2.1, with the
//! NL_AUTH_SHA2_SIGNATURE of 2.2.1.3.3), and the user session key of a network logon (3.2.4.1).
use aes::cipher::{BlockCipherEncrypt, KeyInit};
use aes::Aes128;
use zeroize::Zeroizing;

/// The length of an NL_AUTH_SHA2_SIGNATURE with a confounder: what follows a sealed message.
pub const SIGNATURE_LEN: usize = 56;
const SIGN_HMAC_SHA256: [u8; 2] = [0x13, 0x00];
const SEAL_AES128: [u8; 2] = [0x1a, 0x00];

/// Why a sealed message from the other side was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealError {
    /// Not an AES signature of the expected length and algorithms.
    Malformed,
    /// The sequence number is not the one expected next.
    Sequence,
    /// The checksum does not match the message.
    Checksum,
}

impl std::fmt::Display for SealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "the Netlogon signature is malformed",
            Self::Sequence => "the Netlogon signature has an unexpected sequence number",
            Self::Checksum => "the Netlogon signature does not match the message",
        })
    }
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> ring::hmac::Tag {
    let mut context = ring::hmac::Context::with_key(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key));
    parts.iter().for_each(|part| context.update(part));
    context.sign()
}

/// AES-128 in 8-bit CFB mode: one stream, which several buffers can go through in turn.
struct Cfb8 {
    cipher: Aes128,
    register: Zeroizing<[u8; 16]>,
}

impl Cfb8 {
    fn new(key: &[u8; 16], iv: [u8; 16]) -> Self {
        Self { cipher: Aes128::new(&(*key).into()), register: Zeroizing::new(iv) }
    }

    fn apply(&mut self, data: &mut [u8], decrypt: bool) {
        for byte in data {
            let mut block = aes::Block::from(*self.register);
            self.cipher.encrypt_block(&mut block);
            let output = *byte ^ block[0];
            let ciphertext = if decrypt { *byte } else { output };
            *byte = output;
            self.register.copy_within(1.., 0);
            self.register[15] = ciphertext;
        }
    }
}

/// AES-128 in 8-bit CFB mode over `data`, in place.
fn cfb8(key: &[u8; 16], iv: [u8; 16], data: &mut [u8], decrypt: bool) {
    Cfb8::new(key, iv).apply(data, decrypt);
}

/// MS-NRPC 3.1.4.3, when AES is negotiated: HMAC-SHA256 keyed with the NT hash of the computer
/// account's password, over both challenges, cut to 16 octets.
pub fn session_key(nt_hash: &[u8; 16], client_challenge: &[u8; 8], server_challenge: &[u8; 8]) -> Zeroizing<[u8; 16]> {
    let digest = hmac_sha256(nt_hash, &[client_challenge, server_challenge]);
    let mut key = Zeroizing::new([0u8; 16]);
    key.copy_from_slice(&digest.as_ref()[..16]);
    key
}

/// MS-NRPC 3.1.4.4.2: AES-128-CFB8 with a zero IV.
pub fn credential(session_key: &[u8; 16], input: &[u8; 8]) -> [u8; 8] {
    let mut output = *input;
    cfb8(session_key, [0; 16], &mut output, false);
    output
}

/// The user session key and LM session key of a network logon's validation information, which
/// the domain controller encrypts with AES-128-CFB8 and a zero IV unless they are all zero.
pub fn decrypt_validation_key(session_key: &[u8; 16], key: &mut [u8]) {
    if key.iter().any(|byte| *byte != 0) {
        cfb8(session_key, [0; 16], key, true);
    }
}

/// How a domain controller encrypts a validation key, for the test domain controller.
#[cfg(test)]
pub fn encrypt_validation_key(session_key: &[u8; 16], key: &mut [u8]) {
    if key.iter().any(|byte| *byte != 0) {
        cfb8(session_key, [0; 16], key, false);
    }
}

/// The sequence number as it is signed: the low 32 bits then the high 32 bits, each big-endian,
/// with the top bit of the fifth octet set when the client sent it.
fn sequence_octets(sequence: u64, from_client: bool) -> [u8; 8] {
    let mut octets = [0u8; 8];
    octets[..4].copy_from_slice(&(sequence as u32).to_be_bytes());
    octets[4..].copy_from_slice(&((sequence >> 32) as u32).to_be_bytes());
    if from_client {
        octets[4] |= 0x80;
    }
    octets
}

fn header() -> [u8; 8] {
    [SIGN_HMAC_SHA256[0], SIGN_HMAC_SHA256[1], SEAL_AES128[0], SEAL_AES128[1], 0xff, 0xff, 0, 0]
}

fn checksum(session_key: &[u8; 16], header: &[u8; 8], confounder: &[u8; 8], data: &[u8]) -> [u8; 8] {
    let mut checksum = [0u8; 8];
    checksum.copy_from_slice(&hmac_sha256(session_key, &[header, confounder, data]).as_ref()[..8]);
    checksum
}

fn sealing_key(session_key: &[u8; 16]) -> Zeroizing<[u8; 16]> {
    let mut key = Zeroizing::new(*session_key);
    key.iter_mut().for_each(|byte| *byte ^= 0xf0);
    key
}

fn doubled(half: &[u8; 8]) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[..8].copy_from_slice(half);
    iv[8..].copy_from_slice(half);
    iv
}

/// Signs and encrypts `data` in place (MS-NRPC 3.3.4.2.1) and returns the signature that goes
/// after it. `confounder` must be random.
pub fn seal(session_key: &[u8; 16], sequence: u64, from_client: bool, confounder: [u8; 8], data: &mut [u8]) -> [u8; SIGNATURE_LEN] {
    let header = header();
    let checksum = checksum(session_key, &header, &confounder, data);
    let sequence = sequence_octets(sequence, from_client);
    let mut encrypted_confounder = confounder;
    let mut stream = Cfb8::new(&sealing_key(session_key), doubled(&sequence));
    stream.apply(&mut encrypted_confounder, false);
    stream.apply(data, false);
    let mut encrypted_sequence = sequence;
    cfb8(session_key, doubled(&checksum), &mut encrypted_sequence, false);
    let mut signature = [0u8; SIGNATURE_LEN];
    signature[..8].copy_from_slice(&header);
    signature[8..16].copy_from_slice(&encrypted_sequence);
    signature[16..24].copy_from_slice(&checksum);
    signature[24..32].copy_from_slice(&encrypted_confounder);
    signature
}

/// Checks and decrypts a sealed message in place (MS-NRPC 3.3.4.2.2): `sequence` is the number
/// expected from the other side, which sent it as the client when `from_client`.
pub fn unseal(session_key: &[u8; 16], sequence: u64, from_client: bool, signature: &[u8], data: &mut [u8]) -> Result<(), SealError> {
    // The reserved octets after the confounder may be left out.
    let (Some(header), Some(encrypted_sequence), Some(checksum), Some(encrypted_confounder)) =
        (signature.get(..8), signature.get(8..16), signature.get(16..24), signature.get(24..32))
    else {
        return Err(SealError::Malformed);
    };
    if header[..2] != SIGN_HMAC_SHA256 || header[2..4] != SEAL_AES128 {
        return Err(SealError::Malformed);
    }
    let checksum: [u8; 8] = checksum.try_into().map_err(|_| SealError::Malformed)?;
    let mut received_sequence: [u8; 8] = encrypted_sequence.try_into().map_err(|_| SealError::Malformed)?;
    cfb8(session_key, doubled(&checksum), &mut received_sequence, true);
    let expected = sequence_octets(sequence, from_client);
    if !crate::passwords::constant_time_eq(&received_sequence, &expected) {
        return Err(SealError::Sequence);
    }
    let mut confounder: [u8; 8] = encrypted_confounder.try_into().map_err(|_| SealError::Malformed)?;
    let header: [u8; 8] = header.try_into().map_err(|_| SealError::Malformed)?;
    // Decrypted into a copy, so that a message that fails the check leaves `data` as it came.
    let mut plain = Zeroizing::new(data.to_vec());
    let mut stream = Cfb8::new(&sealing_key(session_key), doubled(&expected));
    stream.apply(&mut confounder, true);
    stream.apply(&mut plain, true);
    if !crate::passwords::constant_time_eq(&self::checksum(session_key, &header, &confounder, &plain), &checksum) {
        return Err(SealError::Checksum);
    }
    data.copy_from_slice(&plain);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex<const N: usize>(text: &str) -> Result<[u8; N], String> {
        let bytes: Vec<u8> = (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16)).collect::<Result<_, _>>().map_err(|e| e.to_string())?;
        bytes.try_into().map_err(|_| format!("not {N} octets: {text}"))
    }

    /// The example values Microsoft published for AES and strong keys.
    #[test]
    fn test_session_key_and_credential() -> Result<(), String> {
        let key = session_key(&hex("13c0b04b66250d08b8a3904dcc8b34e3")?, &hex("2563e35f69e15a24")?, &hex("9c665f90d983df43")?);
        assert_eq!(*key, hex::<16>("c9c7f72fc6b913e367aea91d0ae3a770")?);
        assert_eq!(credential(&key, &hex("2563e35f69e15a24")?), hex::<8>("586adf53ef7278d9")?);
        Ok(())
    }

    /// CFB8 against NIST SP 800-38A F.3.13 (CFB8-AES128.Encrypt), first 16 octets.
    #[test]
    fn test_cfb8_vector() -> Result<(), String> {
        let key = hex::<16>("2b7e151628aed2a6abf7158809cf4f3c")?;
        let iv = hex::<16>("000102030405060708090a0b0c0d0e0f")?;
        let plain = hex::<16>("6bc1bee22e409f96e93d7e117393172a")?;
        let mut data = plain;
        cfb8(&key, iv, &mut data, false);
        assert_eq!(data, hex::<16>("3b79424c9c0dd436bace9e0ed4586a4f")?);
        cfb8(&key, iv, &mut data, true);
        assert_eq!(data, plain);
        Ok(())
    }

    /// The AES sealing vector of Samba's schannel tests: the first message from the client.
    #[test]
    fn test_sealing_vector() -> Result<(), String> {
        let session = hex::<16>("8ee8278583413c8dc95470758ec96991")?;
        let clear = hex::<128>(&[
            "0100000000000200010000000400020001000000010100000000000100000000",
            "0000000000000000010000000000000000000000000000008ae3137102f43671",
            "010004000100000002402800785734123412cdabef000123456789ab00000000",
            "045d888aeb1cc9119fe808002b10486002000000000000000000000000000000",
        ]
        .concat())?;
        let expected = hex::<128>(&[
            "e2e5e32645fbfcf39c14dde13923e055ed8ff492a1bddc40586fd25bf9c9a387",
            "464b7fb203d235223e709f1e3f1fdb7d79885a3dd3401e69d7e21d5ae93be1e2",
            "98fdcb3af7b51cf8ca0200999f0c01e6d200afe051886250b7e86d634b9705c1",
            "d483962980aed8a2edc95d0d29ff2c2302fa3beee8ba060195df80760b170ed8",
        ]
        .concat())?;
        let mut confounder = hex::<8>("6e09259401a00931")?;
        let mut data = clear;
        let mut stream = Cfb8::new(&sealing_key(&session), doubled(&sequence_octets(0, true)));
        stream.apply(&mut confounder, false);
        stream.apply(&mut data, false);
        assert_eq!(confounder, hex::<8>("cafbacfba826752a")?);
        assert_eq!(data, expected);
        Ok(())
    }

    #[test]
    fn test_validation_key() -> Result<(), String> {
        let session = hex::<16>("c9c7f72fc6b913e367aea91d0ae3a770")?;
        let plain = hex::<16>("41c00c584bd2d91c4017a2a12fa59f3f")?;
        let mut key = plain;
        cfb8(&session, [0; 16], &mut key, false);
        assert_ne!(key, plain);
        decrypt_validation_key(&session, &mut key);
        assert_eq!(key, plain);
        // An all-zero key is not encrypted, and stays all zero.
        let mut zero = [0u8; 16];
        decrypt_validation_key(&session, &mut zero);
        assert_eq!(zero, [0; 16]);
        Ok(())
    }

    #[test]
    fn test_seal_round_trip() -> Result<(), String> {
        let session = hex::<16>("c9c7f72fc6b913e367aea91d0ae3a770")?;
        let plain = b"a request stub, with its padding".to_vec();
        let mut data = plain.clone();
        let signature = seal(&session, 7, true, [9; 8], &mut data);
        assert_ne!(data, plain);
        assert_eq!(signature[..8], [0x13, 0x00, 0x1a, 0x00, 0xff, 0xff, 0, 0]);
        // The other side expects the next number from the client.
        assert_eq!(unseal(&session, 7, false, &signature, &mut data.clone()), Err(SealError::Sequence));
        assert_eq!(unseal(&session, 8, true, &signature, &mut data.clone()), Err(SealError::Sequence));
        let mut tampered = data.clone();
        tampered[3] ^= 1;
        assert_eq!(unseal(&session, 7, true, &signature, &mut tampered), Err(SealError::Checksum));
        let mut bad_signature = signature;
        bad_signature[20] ^= 1;
        assert!(unseal(&session, 7, true, &bad_signature, &mut data.clone()).is_err());
        assert_eq!(unseal(&session, 7, true, &signature[..31], &mut data.clone()), Err(SealError::Malformed));
        unseal(&session, 7, true, &signature, &mut data).map_err(|e| e.to_string())?;
        assert_eq!(data, plain);
        // Numbers above 32 bits go in the second half.
        assert_eq!(sequence_octets(0x1_0000_0002, true), [0, 0, 0, 2, 0x80, 0, 0, 1]);
        assert_eq!(sequence_octets(5, false), [0, 0, 0, 5, 0, 0, 0, 0]);
        Ok(())
    }
}
