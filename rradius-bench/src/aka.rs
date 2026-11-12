//! A USIM's side of EAP-AKA' (RFC 9048, RFC 4187), enough to run whole logins against a
//! server: Milenage (3GPP TS 35.206) for the card, CK' and IK' (3GPP TS 33.402 annex A.2), and
//! the key derivation of RFC 9048 section 3.3. Written apart from the server's code, as the rest
//! of this tool is, so that the tests can catch a misreading on either side.
//!
//! Every subscriber of a load shares one Ki and OPc, given as `--password KI:OPC` in hex: a
//! test tool has no use for a key per card, and the server cannot tell.
use aes::cipher::{BlockCipherEncrypt, KeyInit};
use aes::{Aes128, Block};
use hmac::Mac;
use sha2::Sha256;

pub const TYPE_AKA_PRIME: u8 = 50;

const SUBTYPE_CHALLENGE: u8 = 1;
const SUBTYPE_IDENTITY: u8 = 5;
const AT_RAND: u8 = 1;
const AT_AUTN: u8 = 2;
const AT_RES: u8 = 3;
const AT_PERMANENT_ID_REQ: u8 = 10;
const AT_MAC: u8 = 11;
const AT_IDENTITY: u8 = 14;
const AT_KDF_INPUT: u8 = 23;
const AT_KDF: u8 = 24;
/// AMF with the separation bit set, which AKA' needs (3GPP TS 33.402 section 6.2).
const SEPARATION_BIT: u8 = 0x80;

/// One card's long-term keys.
#[derive(Clone)]
pub struct CardKeys {
    ki: [u8; 16],
    opc: [u8; 16],
}

impl CardKeys {
    /// `KI:OPC`, each 32 hex digits.
    pub fn parse(text: &str) -> Result<CardKeys, String> {
        let wrong = || "aka-prime needs --password KI:OPC, each 32 hex digits".to_string();
        let (ki, opc) = text.split_once(':').ok_or_else(wrong)?;
        Ok(CardKeys { ki: hex16(ki).ok_or_else(wrong)?, opc: hex16(opc).ok_or_else(wrong)? })
    }
}

fn hex16(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Milenage's outputs for one RAND.
struct Vector {
    mac_a: [u8; 8],
    res: [u8; 8],
    ck: [u8; 16],
    ik: [u8; 16],
    ak: [u8; 6],
}

fn milenage(keys: &CardKeys, rand: &[u8; 16], sqn: &[u8; 6], amf: &[u8; 2]) -> Vector {
    let cipher = Aes128::new(&keys.ki.into());
    let encrypt = |input: [u8; 16]| -> [u8; 16] {
        let mut block = Block::from(input);
        cipher.encrypt_block(&mut block);
        block.into()
    };
    let xor = |a: [u8; 16], b: &[u8; 16]| -> [u8; 16] { std::array::from_fn(|i| a[i] ^ b[i]) };
    let temp = encrypt(xor(*rand, &keys.opc));
    // OUTn = E(rot(TEMP xor OPc, rn) xor cn) xor OPc, with rn in octets and cn's last octet.
    let out = |input: [u8; 16], rotation: usize, constant: u8| -> [u8; 16] {
        let mut rotated = xor(input, &keys.opc);
        rotated.rotate_left(rotation);
        rotated[15] ^= constant;
        xor(encrypt(rotated), &keys.opc)
    };
    let mut in1 = [0u8; 16];
    in1[..6].copy_from_slice(sqn);
    in1[6..8].copy_from_slice(amf);
    in1[8..14].copy_from_slice(sqn);
    in1[14..].copy_from_slice(amf);
    // OUT1 = E(TEMP xor rot(IN1 xor OPc, r1) xor c1) xor OPc.
    let mut rotated = xor(in1, &keys.opc);
    rotated.rotate_left(8);
    let out1 = xor(encrypt(xor(temp, &rotated)), &keys.opc);
    let out2 = out(temp, 0, 1);
    Vector {
        mac_a: std::array::from_fn(|i| out1[i]),
        res: std::array::from_fn(|i| out2[8 + i]),
        ck: out(temp, 4, 2),
        ik: out(temp, 8, 4),
        ak: std::array::from_fn(|i| out2[i]),
    }
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> Result<[u8; 32], String> {
    let mut mac = <hmac::Hmac<Sha256>>::new_from_slice(key).map_err(|_| "an HMAC key".to_string())?;
    for part in parts {
        mac.update(part);
    }
    Ok(mac.finalize().into_bytes().into())
}

/// K_aut and the MSK, from CK' || IK' and the identity.
fn derive(ck_ik_prime: &[u8; 32], identity: &[u8]) -> Result<([u8; 32], [u8; 64]), String> {
    // PRF' is keyed with IK' || CK' (RFC 9048 section 3.4).
    let key = [&ck_ik_prime[16..], &ck_ik_prime[..16]].concat();
    let seed = [b"EAP-AKA'".as_slice(), identity].concat();
    let mut material = Vec::with_capacity(224);
    let mut last: Vec<u8> = Vec::new();
    for round in 1..=7u8 {
        let block = hmac_sha256(&key, &[&last, &seed, &[round]])?;
        material.extend_from_slice(&block);
        last = block.to_vec();
    }
    // K_encr 16, K_aut 32, K_re 32, MSK 64.
    Ok((std::array::from_fn(|i| material[16 + i]), std::array::from_fn(|i| material[80 + i])))
}

/// The attributes of an AKA' message, after its subtype and two reserved octets.
fn attributes(data: &[u8]) -> Result<Vec<(u8, &[u8])>, String> {
    let mut rest = data.get(3..).ok_or("an AKA' message shorter than its header")?;
    let mut found = Vec::new();
    while let [kind, length, ..] = *rest {
        let length = usize::from(length) * 4;
        if length < 4 || length > rest.len() {
            return Err("an AKA' attribute whose length does not fit".to_string());
        }
        found.push((kind, &rest[2..length]));
        rest = &rest[length..];
    }
    if !rest.is_empty() {
        return Err("an AKA' attribute whose length does not fit".to_string());
    }
    Ok(found)
}

fn attribute(kind: u8, value: &[u8]) -> Vec<u8> {
    let length = (value.len() + 2).next_multiple_of(4);
    let mut out = vec![kind, (length / 4) as u8];
    out.extend_from_slice(value);
    out.resize(length, 0);
    out
}

/// One card in one login.
pub struct AkaPeer {
    keys: CardKeys,
    identity: Vec<u8>,
    started: bool,
    msk: Option<[u8; 64]>,
}

impl AkaPeer {
    pub fn new(keys: CardKeys, identity: &[u8]) -> AkaPeer {
        AkaPeer { keys, identity: identity.to_vec(), started: false, msk: None }
    }

    /// Whether the server has sent anything of AKA' yet.
    pub fn started(&self) -> bool {
        self.started
    }

    pub fn msk(&self) -> Option<&[u8; 64]> {
        self.msk.as_ref()
    }

    /// The data of the EAP-Response (what follows the Type) to `request`, a whole EAP-Request
    /// of type AKA'. `respond` builds the Response packet around data, for the MAC.
    pub fn answer(&mut self, request: &[u8], respond: impl Fn(&[u8]) -> Vec<u8>) -> Result<Vec<u8>, String> {
        self.started = true;
        let data = request.get(5..).ok_or("an AKA' request without data")?;
        let found = attributes(data)?;
        let get = |kind: u8| found.iter().find(|(other, _)| *other == kind).map(|(_, value)| *value);
        match data.first() {
            Some(&SUBTYPE_IDENTITY) if get(AT_PERMANENT_ID_REQ).is_some() => {
                let mut value = (self.identity.len() as u16).to_be_bytes().to_vec();
                value.extend_from_slice(&self.identity);
                let mut reply = vec![SUBTYPE_IDENTITY, 0, 0];
                reply.extend(attribute(AT_IDENTITY, &value));
                Ok(reply)
            }
            Some(&SUBTYPE_IDENTITY) => Err("an AKA-Identity that asks for no permanent identity".to_string()),
            Some(&SUBTYPE_CHALLENGE) => self.challenge(request, &found, respond),
            Some(other) => Err(format!("AKA' subtype {other} from the server")),
            None => Err("an AKA' request without data".to_string()),
        }
    }

    fn challenge(&mut self, request: &[u8], found: &[(u8, &[u8])], respond: impl Fn(&[u8]) -> Vec<u8>) -> Result<Vec<u8>, String> {
        let get = |kind: u8| found.iter().find(|(other, _)| *other == kind).map(|(_, value)| *value);
        let rand: [u8; 16] = get(AT_RAND).and_then(|value| value.get(2..18)).and_then(|value| value.try_into().ok()).ok_or("no AT_RAND")?;
        let autn: [u8; 16] = get(AT_AUTN).and_then(|value| value.get(2..18)).and_then(|value| value.try_into().ok()).ok_or("no AT_AUTN")?;
        if get(AT_KDF) != Some(&[0, 1]) {
            return Err("an AT_KDF other than 1".to_string());
        }
        let name = get(AT_KDF_INPUT).ok_or("no AT_KDF_INPUT")?;
        let (length, rest) = name.split_first_chunk::<2>().ok_or("an AT_KDF_INPUT without a length")?;
        let network = rest.get(..usize::from(u16::from_be_bytes(*length))).ok_or("an AT_KDF_INPUT whose length does not fit")?;
        let amf = [autn[6], autn[7]];
        if amf[0] & SEPARATION_BIT == 0 {
            return Err("an AUTN without the AMF separation bit".to_string());
        }
        // SQN xor AK, then AMF, then MAC-A. Any SQN does: a load does not keep a card's state.
        let first = milenage(&self.keys, &rand, &[0; 6], &amf);
        let sqn: [u8; 6] = std::array::from_fn(|i| autn[i] ^ first.ak[i]);
        let vector = milenage(&self.keys, &rand, &sqn, &amf);
        if vector.mac_a != autn[8..] {
            return Err("an AUTN that does not prove the network knows the card's key".to_string());
        }
        let key = [vector.ck.as_slice(), vector.ik.as_slice()].concat();
        let ck_ik_prime = hmac_sha256(&key, &[&[0x20], network, &(network.len() as u16).to_be_bytes(), &autn[..6], &6u16.to_be_bytes()])?;
        let (k_aut, msk) = derive(&ck_ik_prime, &self.identity)?;
        // The server's MAC covers its whole packet with the MAC zero.
        let mac_at = mac_offset(request).ok_or("no AT_MAC from the server")?;
        let mut zeroed = request.to_vec();
        zeroed[mac_at..mac_at + 16].fill(0);
        if hmac_sha256(&k_aut, &[&zeroed])?[..16] != request[mac_at..mac_at + 16] {
            return Err("a wrong AT_MAC from the server".to_string());
        }
        let mut res = 64u16.to_be_bytes().to_vec();
        res.extend_from_slice(&vector.res);
        let mut reply = vec![SUBTYPE_CHALLENGE, 0, 0];
        reply.extend(attribute(AT_RES, &res));
        reply.extend(attribute(AT_MAC, &[0; 18]));
        // `AT_MAC` comes last, so its 16 octets end both the data and the packet.
        let mac = hmac_sha256(&k_aut, &[&respond(&reply)])?;
        let end = reply.len();
        reply[end - 16..].copy_from_slice(&mac[..16]);
        self.msk = Some(msk);
        Ok(reply)
    }
}

/// Where the 16 octets of `AT_MAC` start in a whole EAP packet of type AKA'.
fn mac_offset(packet: &[u8]) -> Option<usize> {
    let mut at = 8;
    while at + 2 <= packet.len() {
        let length = usize::from(packet[at + 1]) * 4;
        if length == 0 || at + length > packet.len() {
            return None;
        }
        if packet[at] == AT_MAC {
            return (length == 20).then_some(at + 4);
        }
        at += length;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3GPP TS 35.208 test set 1.
    #[test]
    fn test_milenage_test_set_1() -> Result<(), String> {
        let keys = CardKeys::parse("465b5ce8b199b49faa5f0a2ee238a6bc:cd63cb71954a9f4e48a5994e37a02baf")?;
        let rand = hex16("23553cbe9637a89d218ae64dae47bf35").ok_or("rand")?;
        let vector = milenage(&keys, &rand, &[0xff, 0x9b, 0xb4, 0xd0, 0xb6, 0x07], &[0xb9, 0xb9]);
        assert_eq!(vector.mac_a, [0x4a, 0x9f, 0xfa, 0xc3, 0x54, 0xdf, 0xaf, 0xb3]);
        assert_eq!(vector.res, [0xa5, 0x42, 0x11, 0xd5, 0xe3, 0xba, 0x50, 0xbf]);
        assert_eq!(vector.ck, hex16("b40ba9a3c58b2a05bbf0d987b21bf8cb").ok_or("ck")?);
        assert_eq!(vector.ik, hex16("f769bcd751044604127672711c6d3441").ok_or("ik")?);
        assert_eq!(vector.ak, [0xaa, 0x68, 0x9c, 0x64, 0x83, 0x70]);
        Ok(())
    }

    #[test]
    fn test_card_keys() {
        assert!(CardKeys::parse("465b5ce8b199b49faa5f0a2ee238a6bc:cd63cb71954a9f4e48a5994e37a02baf").is_ok());
        assert!(CardKeys::parse("465b5ce8b199b49faa5f0a2ee238a6bc").is_err());
        assert!(CardKeys::parse("465b5ce8b199b49faa5f0a2ee238a6:cd63cb71954a9f4e48a5994e37a02baf").is_err());
        assert!(CardKeys::parse("zz5b5ce8b199b49faa5f0a2ee238a6bc:cd63cb71954a9f4e48a5994e37a02baf").is_err());
    }
}
