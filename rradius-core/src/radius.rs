use zeroize::Zeroizing;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use std::io;
use std::net::SocketAddr;
use hmac::{Hmac, Mac, KeyInit};
use md5::{Digest, Md5};
use crate::auth_chain::{AuthChains, Credentials, Secret};
use crate::clients::{Client, ClientTable, MessageAuthenticatorPolicy};
use crate::dictionary::{Attribute, Dictionary, VendorFormat};
use crate::config::EapSettings;
use crate::eap::{certificate_name, handler_name, CertificateSubject, join_message, split_message, EapPacket, EapService, Finished, InnerResult, Owner, PendingAuth, Step, EAP_MESSAGE, TYPE_TTLS};
use crate::lua_engine::{log_line, now_unix, LuaAuthEngine, LuaRequest};
use crate::common::{AttrValue, Outcome};
use crate::metrics::SpoolCounters;
use crate::spool::{Spool, SpooledRequest};
use futures::future::BoxFuture;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use crate::metrics::Metrics;
use crate::reload::Live;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const ACCESS_REQUEST: u8 = 1;
pub const ACCESS_ACCEPT: u8 = 2;
pub const ACCESS_REJECT: u8 = 3;
pub const ACCOUNTING_REQUEST: u8 = 4;
pub const ACCOUNTING_RESPONSE: u8 = 5;
pub const ACCESS_CHALLENGE: u8 = 11;
pub const STATUS_SERVER: u8 = 12;
pub const USER_PASSWORD: u8 = 2;
pub const CHAP_PASSWORD: u8 = 3;
pub const VENDOR_SPECIFIC: u8 = 26;
/// The most an attribute's value can hold.
const MAX_ATTRIBUTE_VALUE: usize = 253;
/// The most a TLV can be, with its own two octets.
const MAX_TLV: usize = 255;
/// RFC 6929 section 2.2: what a `long-extended` fragment holds, after the Extended-Type and Flags.
const LONG_EXTENDED_FRAGMENT: usize = MAX_ATTRIBUTE_VALUE - 2;
const LONG_EXTENDED_MORE: u8 = 0x80;
/// The Extended-Type of Extended-Vendor-Specific: RFC 6929 section 2.4.
const EVS_TYPE: u8 = 26;
const VENDOR_CONTINUATION: u8 = 0x80;
pub const CHAP_CHALLENGE: u8 = 60;
const MICROSOFT_VENDOR_ID: u32 = 311;
const MS_CHAP_CHALLENGE: u8 = 11;
const MS_CHAP2_RESPONSE: u8 = 25;
pub const MESSAGE_AUTHENTICATOR: u8 = 80;
pub const PROXY_STATE: u8 = 33;
pub(crate) const STATE: u8 = 24;
/// How often EAP conversations that went quiet are looked for.
const EAP_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const CALLING_STATION_ID: u8 = 31;

const HEADER_LEN: usize = 20;
pub const MAX_PACKET_LEN: usize = 4096;
/// The longest value that fits an encrypt=2 attribute: 253 octets, less the tag and the
/// salt, leaves 15 blocks of 16, and the first octet of those holds the length.
const MAX_SALTED_VALUE: usize = 239;
/// Far more than fit in a packet. There are only 32768 salts, and looking for an unused
/// one would never end without a limit below that.
const MAX_SALTED_ATTRIBUTES: usize = 256;

#[derive(Clone)]
pub struct RadiusPacket {
    pub code: u8,
    pub identifier: u8,
    pub authenticator: [u8; 16],
    /// Attributes in wire order as (type, value).
    pub attributes: Vec<(u8, Vec<u8>)>,
}

impl RadiusPacket {
    /// Parses a packet, rejecting a header Length that differs from the received size
    /// and attributes whose length is below 2 or runs past the end of the packet.
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        if data.len() < HEADER_LEN {
            return Err("Packet too short".to_string());
        }
        let length = u16::from_be_bytes([data[2], data[3]]) as usize;
        if length != data.len() || length > MAX_PACKET_LEN {
            return Err(format!("Header length {} does not match packet size {}", length, data.len()));
        }
        let mut authenticator = [0u8; 16];
        authenticator.copy_from_slice(&data[4..HEADER_LEN]);

        let mut attributes = Vec::new();
        let mut rest = &data[HEADER_LEN..];
        while !rest.is_empty() {
            if rest.len() < 2 {
                return Err("Truncated attribute header".to_string());
            }
            let attr_len = rest[1] as usize;
            if attr_len < 2 || attr_len > rest.len() {
                return Err(format!("Invalid attribute length {} for type {}", attr_len, rest[0]));
            }
            attributes.push((rest[0], rest[2..attr_len].to_vec()));
            rest = &rest[attr_len..];
        }

        Ok(RadiusPacket {
            code: data[0],
            identifier: data[1],
            authenticator,
            attributes,
        })
    }

    pub fn attribute(&self, attr_type: u8) -> Option<&[u8]> {
        self.attributes.iter().find(|(t, _)| *t == attr_type).map(|(_, v)| v.as_slice())
    }

    /// Serializes the packet as-is, including its current authenticator.
    pub fn encode(&self) -> Vec<u8> {
        let mut packet = vec![self.code, self.identifier, 0, 0];
        packet.extend_from_slice(&self.authenticator);
        for (attr_type, value) in &self.attributes {
            packet.push(*attr_type);
            packet.push((value.len() + 2) as u8);
            packet.extend_from_slice(value);
        }
        let len_bytes = (packet.len() as u16).to_be_bytes();
        packet[2] = len_bytes[0];
        packet[3] = len_bytes[1];
        packet
    }

    /// Verifies the RFC 3579 Message-Authenticator. Returns `None` when the packet
    /// has no Message-Authenticator attribute.
    pub fn verify_message_authenticator(&self, shared_secret: &[u8]) -> Option<bool> {
        let received = self.attribute(MESSAGE_AUTHENTICATOR)?.to_vec();
        let expected = self.message_authenticator(shared_secret);
        Some(crate::passwords::constant_time_eq(&received, &expected))
    }

    /// HMAC-MD5 over the packet with the Message-Authenticator value zeroed.
    fn message_authenticator(&self, shared_secret: &[u8]) -> [u8; 16] {
        let mut zeroed = RadiusPacket {
            code: self.code,
            identifier: self.identifier,
            authenticator: self.authenticator,
            attributes: self.attributes.clone(),
        };
        for (attr_type, value) in zeroed.attributes.iter_mut() {
            if *attr_type == MESSAGE_AUTHENTICATOR {
                *value = vec![0u8; 16];
            }
        }
        // The only allowed expect in production code: HMAC accepts keys of any
        // length, so this cannot fail (hmac's own `KeyInit::new` does the same).
        #[allow(clippy::expect_used)]
        let mut mac = Hmac::<Md5>::new_from_slice(shared_secret).expect("HMAC accepts keys of any length");
        mac.update(&zeroed.encode());
        mac.finalize().into_bytes().into()
    }

    /// Inserts a Message-Authenticator as the first attribute, computed over this
    /// packet with its value zeroed (RFC 3579).
    pub fn push_message_authenticator(&mut self, shared_secret: &[u8]) {
        // First: it then covers everything after it, leaving nothing an attacker can
        // choose in front of it (the BlastRADIUS mitigation).
        self.attributes.insert(0, (MESSAGE_AUTHENTICATOR, vec![0u8; 16]));
        self.attributes[0].1 = self.message_authenticator(shared_secret).to_vec();
    }

    /// Verifies an Accounting-Request authenticator (RFC 2866 section 3):
    /// MD5(Code + Identifier + Length + 16 zero octets + Attributes + secret).
    pub fn verify_accounting_request(&self, shared_secret: &[u8]) -> bool {
        let mut packet = self.encode();
        packet[4..HEADER_LEN].fill(0);
        md5_with_secret(&packet, shared_secret) == self.authenticator
    }

    /// Builds a response whose Response Authenticator is
    /// MD5(Code + Identifier + Length + Request Authenticator + Attributes + secret).
    /// Copies the request's Proxy-State after `attributes`. An Access-Accept or
    /// Access-Reject starts with a Message-Authenticator computed over the response
    /// (carrying the Request Authenticator).
    pub fn create_response(code: u8, request: &RadiusPacket, mut attributes: Vec<(u8, Vec<u8>)>, shared_secret: &[u8]) -> Vec<u8> {
        attributes.extend(request.attributes.iter().filter(|(attr_type, _)| *attr_type == PROXY_STATE).cloned());
        let with_message_authenticator = matches!(code, ACCESS_ACCEPT | ACCESS_REJECT | ACCESS_CHALLENGE);
        Self::signed_response(code, request, attributes, shared_secret, with_message_authenticator)
    }

    fn signed_response(code: u8, request: &RadiusPacket, attributes: Vec<(u8, Vec<u8>)>, shared_secret: &[u8], with_message_authenticator: bool) -> Vec<u8> {
        let mut response = RadiusPacket {
            code,
            identifier: request.identifier,
            authenticator: request.authenticator,
            attributes,
        };
        if with_message_authenticator {
            response.push_message_authenticator(shared_secret);
        }
        let mut packet = response.encode();
        let auth = md5_with_secret(&packet, shared_secret);
        packet[4..HEADER_LEN].copy_from_slice(&auth);
        packet
    }

    /// Builds an Access-Request carrying a Message-Authenticator, as a client would.
    pub fn create_access_request(identifier: u8, request_authenticator: [u8; 16], attributes: Vec<(u8, Vec<u8>)>, shared_secret: &[u8]) -> Vec<u8> {
        let mut request = RadiusPacket {
            code: ACCESS_REQUEST,
            identifier,
            authenticator: request_authenticator,
            attributes,
        };
        request.push_message_authenticator(shared_secret);
        request.encode()
    }

    /// Builds an Accounting-Request with its RFC 2866 Request Authenticator, as a client would.
    pub fn create_accounting_request(identifier: u8, attributes: Vec<(u8, Vec<u8>)>, shared_secret: &[u8]) -> Vec<u8> {
        Self::create_signed_request(ACCOUNTING_REQUEST, identifier, attributes, shared_secret)
    }

    /// Builds a request whose Request Authenticator is the MD5 of the packet with a zeroed
    /// authenticator and the secret: Accounting-Request (RFC 2866), and Disconnect-Request
    /// and CoA-Request (RFC 5176).
    pub fn create_signed_request(code: u8, identifier: u8, attributes: Vec<(u8, Vec<u8>)>, shared_secret: &[u8]) -> Vec<u8> {
        let request = RadiusPacket {
            code,
            identifier,
            authenticator: [0u8; 16],
            attributes,
        };
        let mut packet = request.encode();
        let auth = md5_with_secret(&packet, shared_secret);
        packet[4..HEADER_LEN].copy_from_slice(&auth);
        packet
    }
}

pub fn md5_with_secret(packet: &[u8], shared_secret: &[u8]) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(packet);
    hasher.update(shared_secret);
    hasher.finalize().into()
}

/// Decrypts User-Password per RFC 2865 section 5.2 and strips the zero padding:
/// b1 = MD5(secret + Request Authenticator), bi = MD5(secret + c(i-1)), p = c XOR b.
pub fn decrypt_user_password(cipher: &[u8], shared_secret: &[u8], request_authenticator: &[u8; 16]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(cipher.len());
    let mut previous: &[u8] = request_authenticator;
    for chunk in cipher.chunks(16) {
        let mut hasher = Md5::new();
        hasher.update(shared_secret);
        hasher.update(previous);
        let pad = hasher.finalize();
        plain.extend(chunk.iter().zip(pad.iter()).map(|(c, b)| c ^ b));
        previous = chunk;
    }
    while plain.last() == Some(&0) {
        plain.pop();
    }
    plain
}

/// Converts a raw attribute value to its Lua form according to the dictionary type.
fn decode_value(attribute: &Attribute, value: &[u8]) -> AttrValue {
    let number = |len: usize| -> Option<AttrValue> {
        (value.len() == len).then(|| AttrValue::Number(value.iter().fold(0u64, |acc, b| (acc << 8) | *b as u64) as f64))
    };
    let decoded = match attribute.data_type.as_str() {
        "integer" | "date" => number(4),
        "signed" => (value.len() == 4).then(|| AttrValue::Number(i32::from_be_bytes([value[0], value[1], value[2], value[3]]) as f64)),
        "byte" => number(1),
        "short" => number(2),
        "integer64" => number(8),
        "ipaddr" => (value.len() == 4).then(|| AttrValue::Bytes(Ipv4Addr::new(value[0], value[1], value[2], value[3]).to_string().into_bytes())),
        _ => decode_address(&attribute.data_type, value).map(String::into_bytes).map(AttrValue::Bytes),
    };
    decoded.unwrap_or_else(|| AttrValue::Bytes(value.to_vec()))
}

/// The text of a value of one of the address types, if its length fits the type.
fn decode_address(data_type: &str, value: &[u8]) -> Option<String> {
    let ipv4 = |octets: &[u8]| <[u8; 4]>::try_from(octets).ok().map(Ipv4Addr::from);
    let ipv6 = |octets: &[u8]| <[u8; 16]>::try_from(octets).ok().map(Ipv6Addr::from);
    match data_type {
        "ipv6addr" => ipv6(value).map(|address| address.to_string()),
        "combo-ip" => ipv4(value).map(|address| address.to_string()).or_else(|| ipv6(value).map(|address| address.to_string())),
        // RFC 3162 section 2.3: the octets after the prefix can be left out.
        "ipv6prefix" => match value {
            [_, length, prefix @ ..] if *length <= 128 && prefix.len() <= 16 => {
                let mut octets = [0u8; 16];
                octets[..prefix.len()].copy_from_slice(prefix);
                Some(format!("{}/{length}", Ipv6Addr::from(octets)))
            }
            _ => None,
        },
        "ipv4prefix" => match value {
            [_, length, prefix @ ..] if *length <= 32 => ipv4(prefix).map(|address| format!("{address}/{length}")),
            _ => None,
        },
        "ifid" => <[u8; 8]>::try_from(value).ok().map(|octets| {
            octets.chunks(2).map(|pair| format!("{:02x}{:02x}", pair[0], pair[1])).collect::<Vec<_>>().join(":")
        }),
        "ether" => <[u8; 6]>::try_from(value).ok().map(|octets| octets.iter().map(|octet| format!("{octet:02x}")).collect::<Vec<_>>().join(":")),
        _ => None,
    }
}

/// `text` as `<address>/<length>`, with a length of up to `max`.
fn split_prefix(text: &str, max: u8) -> Option<(&str, u8)> {
    let (address, length) = text.split_once('/')?;
    if length.is_empty() || !length.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let length: u8 = length.parse().ok()?;
    (length <= max).then_some((address, length))
}

/// The octets of `groups` colon-separated groups of exactly `digits` hexadecimal digits.
/// Strict, since a looser reading could turn a typing mistake into another valid address.
fn hex_groups(text: &str, groups: usize, digits: usize) -> Option<Vec<u8>> {
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() != groups || parts.iter().any(|part| part.len() != digits || !part.bytes().all(|b| b.is_ascii_hexdigit())) {
        return None;
    }
    let hex: String = parts.concat();
    (0..hex.len()).step_by(2).map(|start| u8::from_str_radix(&hex[start..start + 2], 16).ok()).collect()
}

/// The octets of a value of one of the address types, from its text. Bits after a prefix
/// have to be zero as the RFCs ask: clearing them would send something other than what was
/// asked for, and a prefix written that way is more likely a mistake.
fn encode_address(data_type: &str, text: &str) -> Option<Vec<u8>> {
    match data_type {
        "ipv6prefix" => {
            let (address, length) = split_prefix(text, 128)?;
            let address: Ipv6Addr = address.parse().ok()?;
            let host_bits = u128::from(address).checked_shl(u32::from(length)).unwrap_or(0);
            // All 16 octets: allowed by RFC 3162, and the form every reader has to take.
            (host_bits == 0).then(|| [&[0, length], &address.octets()[..]].concat())
        }
        "ipv4prefix" => {
            let (address, length) = split_prefix(text, 32)?;
            let address: Ipv4Addr = address.parse().ok()?;
            let host_bits = u32::from(address).checked_shl(u32::from(length)).unwrap_or(0);
            (host_bits == 0).then(|| [&[0, length], &address.octets()[..]].concat())
        }
        "ifid" => hex_groups(text, 4, 4),
        "ether" => hex_groups(text, 6, 2),
        "combo-ip" => match text.parse::<IpAddr>().ok()? {
            IpAddr::V4(address) => Some(address.octets().to_vec()),
            IpAddr::V6(address) => Some(address.octets().to_vec()),
        },
        _ => None,
    }
}

/// Maps request attributes to (dictionary name, Lua value) pairs: Vendor-Specific, extended and
/// TLV attributes are split into what they hold. Attributes missing from the dictionary are left
/// out, and so is all of one that is invalid (RFC 6929 section 2.8), with the rest of the request
/// going on.
pub fn request_attributes(packet: &RadiusPacket, dictionary: &Dictionary, shared_secret: &[u8]) -> Vec<(String, AttrValue)> {
    let mut result = Vec::new();
    let attributes = &packet.attributes;
    let mut at = 0;
    while at < attributes.len() {
        let (attr_type, value) = &attributes[at];
        at += 1;
        let number = u32::from(*attr_type);
        match *attr_type {
            USER_PASSWORD => {
                let plain = decrypt_user_password(value, shared_secret, &packet.authenticator);
                read_attribute(dictionary, 0, vec![number], &plain, &mut result);
            }
            VENDOR_SPECIFIC if value.len() >= 5 => at = read_vendor_specific(dictionary, attributes, at, &mut result),
            _ => match dictionary.get(0, &[number]).map(|attribute| attribute.data_type.as_str()) {
                Some("extended") => {
                    if let Some((extended, data)) = value.split_first() {
                        read_extended(dictionary, number, *extended, data, &mut result);
                    }
                }
                Some("long-extended") => {
                    let (joined, next) = join_fragments(attributes, at - 1);
                    at = next;
                    if let Some((extended, data)) = joined {
                        read_extended(dictionary, number, extended, &data, &mut result);
                    }
                }
                _ => read_attribute(dictionary, 0, vec![number], value, &mut result),
            },
        }
    }
    result
}

/// Adds what an attribute holds to `result`, or nothing at all if a TLV in it is invalid.
fn read_attribute(dictionary: &Dictionary, vendor: u32, mut number: Vec<u32>, value: &[u8], result: &mut Vec<(String, AttrValue)>) {
    let mut found = Vec::new();
    if read_tlvs(dictionary, vendor, &mut number, value, &mut found) {
        result.append(&mut found);
    }
}

/// `false` if a TLV is invalid: RFC 6929 section 2.3. As deep as the dictionary goes.
fn read_tlvs(dictionary: &Dictionary, vendor: u32, number: &mut Vec<u32>, value: &[u8], found: &mut Vec<(String, AttrValue)>) -> bool {
    let Some(attribute) = dictionary.get(vendor, number) else {
        return true;
    };
    if attribute.data_type != "tlv" {
        found.push((attribute.name.clone(), decode_value(attribute, value)));
        return true;
    }
    let mut rest = value;
    while !rest.is_empty() {
        let &[kind, length, ..] = rest else {
            return false;
        };
        let length = usize::from(length);
        if kind == 0 || length < 3 || length > rest.len() {
            return false;
        }
        number.push(u32::from(kind));
        let valid = read_tlvs(dictionary, vendor, number, &rest[2..length], found);
        number.pop();
        if !valid {
            return false;
        }
        rest = &rest[length..];
    }
    true
}

/// What follows the Extended-Type of an attribute, with any fragments joined.
fn read_extended(dictionary: &Dictionary, top: u32, extended: u8, data: &[u8], result: &mut Vec<(String, AttrValue)>) {
    let is_evs = dictionary.get(0, &[top, u32::from(extended)]).is_some_and(|attribute| attribute.data_type == "evs");
    if !is_evs {
        return read_attribute(dictionary, 0, vec![top, u32::from(extended)], data, result);
    }
    let Some((vendor, rest)) = data.split_first_chunk::<4>() else { return };
    let Some((number, value)) = rest.split_first() else { return };
    read_attribute(dictionary, u32::from_be_bytes(*vendor), vec![top, u32::from(extended), u32::from(*number)], value, result);
}

/// The Extended-Type and value of the `long-extended` attribute at `start`, if its fragments are
/// valid, and where to go on from. An invalid one takes the rest of that attribute with it, so
/// that part of a value is never taken for all of it.
fn join_fragments(attributes: &[(u8, Vec<u8>)], start: usize) -> (Option<(u8, Vec<u8>)>, usize) {
    let kind = attributes[start].0;
    let Some(&extended) = attributes[start].1.first() else {
        return (None, start + 1);
    };
    let same = attributes[start..].iter().take_while(|(other, value)| *other == kind && value.first() == Some(&extended)).count();
    let mut joined = Vec::new();
    for (index, (_, value)) in attributes[start..start + same].iter().enumerate() {
        let Some(&flags) = value.get(1) else { break };
        joined.extend_from_slice(&value[2..]);
        if flags & LONG_EXTENDED_MORE == 0 {
            return (Some((extended, joined)), start + index + 1);
        }
        if value.len() != MAX_ATTRIBUTE_VALUE {
            break;
        }
    }
    (None, start + same)
}

/// One of a vendor's attributes at the start of `rest`: its number, value, whether it goes on in
/// the next Vendor-Specific, and what follows it.
fn vendor_attribute(rest: &[u8], format: VendorFormat) -> Option<(u32, &[u8], bool, &[u8])> {
    let flag_size = usize::from(format.continuation);
    let header = format.type_size + format.length_size + flag_size;
    if rest.len() < header {
        return None;
    }
    let big_endian = |bytes: &[u8]| bytes.iter().fold(0usize, |number, byte| number << 8 | usize::from(*byte));
    let end = match format.length_size {
        0 => rest.len(),
        size => big_endian(&rest[format.type_size..format.type_size + size]),
    };
    if end < header || end > rest.len() {
        return None;
    }
    let more = format.continuation && rest[header - 1] & VENDOR_CONTINUATION != 0;
    Some((big_endian(&rest[..format.type_size]) as u32, &rest[header..end], more, &rest[end..]))
}

/// The Vendor-Specific before `at`, and those that go on from it. Returns where to go on from.
fn read_vendor_specific(dictionary: &Dictionary, attributes: &[(u8, Vec<u8>)], mut at: usize, result: &mut Vec<(String, AttrValue)>) -> usize {
    let (vendor, mut rest) = match attributes[at - 1].1.split_first_chunk::<4>() {
        Some((vendor, rest)) => (u32::from_be_bytes(*vendor), rest),
        None => return at,
    };
    let format = dictionary.vendor_format(vendor);
    while let Some((number, value, more, after)) = vendor_attribute(rest, format) {
        rest = after;
        if !more {
            read_attribute(dictionary, vendor, vec![number], value, result);
            continue;
        }
        let mut joined = value.to_vec();
        while let Some((VENDOR_SPECIFIC, next)) = attributes.get(at).map(|(kind, value)| (*kind, value.as_slice())) {
            let Some((next_vendor, next)) = next.split_first_chunk::<4>() else { break };
            let Some((next_number, value, more, _)) = vendor_attribute(next, format) else { break };
            if u32::from_be_bytes(*next_vendor) != vendor || next_number != number {
                break;
            }
            at += 1;
            joined.extend_from_slice(value);
            if !more {
                read_attribute(dictionary, vendor, vec![number], &joined, result);
                break;
            }
        }
        return at;
    }
    at
}

/// RFC 2759 section 6: what tells an MS-CHAPv2 client it was refused. 691 is "authentication
/// failure"; `R=0` asks for no retry, and the challenge a retry would use is then unused.
fn mschap_error(dictionary: &Dictionary, credentials: Option<&Credentials>) -> Vec<(u8, Vec<u8>)> {
    let Some(Credentials { secret: Secret::MsChapV2 { ident, challenge, .. }, .. }) = credentials else {
        return Vec::new();
    };
    let hex: String = challenge.iter().map(|b| format!("{b:02X}")).collect();
    let mut text = vec![*ident];
    text.extend_from_slice(format!("E=691 R=0 C={hex} V=3").as_bytes());
    encode_response_attribute(dictionary, "MS-CHAP-Error", &AttrValue::Bytes(text), b"", &[0; 16], &mut Vec::new())
        .map_or_else(|_| Vec::new(), |attribute| vec![attribute])
}

/// What RFC 2865 section 4.4 does not allow in an Access-Challenge, if anything: besides
/// vendor attributes, only these, and at most one State. Proxy-State is added afterwards.
fn challenge_attribute_problem(dictionary: &Dictionary, attributes: &[(String, AttrValue)]) -> Option<String> {
    const ALLOWED: [&str; 4] = ["Reply-Message", "State", "Idle-Timeout", "Session-Timeout"];
    let mut states = 0;
    for (name, _) in attributes {
        let Ok((attribute, _)) = attribute_and_tag(dictionary, name) else {
            return Some(format!("unknown attribute '{name}'"));
        };
        // Under `evs` it has a vendor, but is not a Vendor-Specific.
        let is_evs = matches!(locate(dictionary, attribute), Ok((Container::Evs { .. }, _)));
        if is_evs || (attribute.vendor == 0 && !ALLOWED.contains(&attribute.name.as_str())) {
            return Some(format!("'{name}' is not allowed in an Access-Challenge"));
        }
        states += usize::from(attribute.vendor == 0 && attribute.name == "State");
    }
    (states > 1).then(|| "more than one State".to_string())
}

/// Splits a tag off an attribute name, as in `Tunnel-Type:1`.
fn attribute_and_tag<'a>(dictionary: &'a Dictionary, name: &str) -> Result<(&'a Attribute, Option<u8>), String> {
    if let Some(attribute) = dictionary.get_by_name(name) {
        return Ok((attribute, None));
    }
    let unknown = || format!("unknown attribute '{}'", name);
    let (base, tag) = name.rsplit_once(':').ok_or_else(unknown)?;
    let attribute = dictionary.get_by_name(base).ok_or_else(unknown)?;
    if !has_option(attribute, "has_tag") {
        return Err(format!("attribute '{}' takes no tag", base));
    }
    match tag.parse::<u8>() {
        Ok(tag @ 1..=31) => Ok((attribute, Some(tag))),
        _ => Err(format!("'{}' needs a tag from 1 to 31", name)),
    }
}

/// Whether the dictionary marks the attribute with an `encrypt=` option.
pub fn attribute_is_encrypted(dictionary: &Dictionary, name: &str) -> Result<bool, String> {
    let (attribute, _) = attribute_and_tag(dictionary, name)?;
    Ok(attribute.options().any(|option| option.starts_with("encrypt=")))
}

fn has_option(attribute: &Attribute, option: &str) -> bool {
    attribute.options().any(|o| o == option)
}

/// RFC 2868 section 3.5: a salt with its top bit set, unique within the response, then
/// the length, the password and zero padding, XORed with MD5(secret + authenticator +
/// salt) for the first block and MD5(secret + previous cipher block) for the others.
fn encrypt_salted(value: &[u8], shared_secret: &[u8], request_authenticator: &[u8; 16], salts: &mut Vec<[u8; 2]>) -> Result<Vec<u8>, String> {
    if value.len() > MAX_SALTED_VALUE {
        return Err(format!("value is longer than {} octets", MAX_SALTED_VALUE));
    }
    if salts.len() >= MAX_SALTED_ATTRIBUTES {
        return Err(format!("more than {} encrypted attributes in one response", MAX_SALTED_ATTRIBUTES));
    }
    let salt = loop {
        let mut salt = [0u8; 2];
        getrandom::fill(&mut salt).map_err(|e| format!("no random salt: {e}"))?;
        salt[0] |= 0x80;
        if !salts.contains(&salt) {
            break salt;
        }
    };
    salts.push(salt);
    let mut plain = Zeroizing::new(Vec::with_capacity(MAX_SALTED_VALUE + 16));
    plain.push(value.len() as u8);
    plain.extend_from_slice(value);
    let padded = plain.len().next_multiple_of(16);
    plain.resize(padded, 0);
    let mut encrypted = salt.to_vec();
    let mut previous: Vec<u8> = [request_authenticator.as_slice(), salt.as_slice()].concat();
    for block in plain.chunks(16) {
        let mut hasher = Md5::new();
        hasher.update(shared_secret);
        hasher.update(&previous);
        let pad = hasher.finalize();
        previous = block.iter().zip(pad.iter()).map(|(p, b)| p ^ b).collect();
        encrypted.extend_from_slice(&previous);
    }
    Ok(encrypted)
}

/// Encodes one attribute that fits one wire attribute. Attributes that may take several, or share
/// one, go through `encode_response_attributes`.
pub fn encode_response_attribute(
    dictionary: &Dictionary,
    name: &str,
    value: &AttrValue,
    shared_secret: &[u8],
    request_authenticator: &[u8; 16],
    salts: &mut Vec<[u8; 2]>,
) -> Result<(u8, Vec<u8>), String> {
    let mut encoded = encode_response_attributes(dictionary, [(name, value)], shared_secret, request_authenticator, salts)?;
    match (encoded.pop(), encoded.is_empty()) {
        (Some(attribute), true) => Ok(attribute),
        _ => Err(format!("value of '{}' does not fit one attribute", name)),
    }
}

/// Where an attribute goes on the wire. With the TLV path `locate` also returns, this is all that
/// tells it apart.
#[derive(Clone, Copy, PartialEq)]
enum Container {
    Plain(u8),
    Vendor { vendor: u32, number: u32, format: VendorFormat },
    Extended { top: u8, extended: u8 },
    LongExtended { top: u8, extended: u8 },
    Evs { top: u8, long: bool, vendor: u32, number: u8 },
}

impl Container {
    /// How much one attribute holds, or `None` if it goes on in others.
    fn capacity(&self) -> Option<usize> {
        match self {
            Container::Plain(_) => Some(MAX_ATTRIBUTE_VALUE),
            Container::Vendor { format, .. } if format.continuation => None,
            Container::Vendor { format, .. } => Some(MAX_ATTRIBUTE_VALUE - 4 - format.type_size - format.length_size),
            Container::Extended { .. } => Some(MAX_ATTRIBUTE_VALUE - 1),
            Container::Evs { long: false, .. } => Some(MAX_ATTRIBUTE_VALUE - 6),
            Container::LongExtended { .. } | Container::Evs { long: true, .. } => None,
        }
    }

    /// Adds `payload` to `out`, in as many attributes as it takes.
    fn write(&self, payload: &[u8], name: &str, out: &mut Vec<(u8, Vec<u8>)>) -> Result<(), String> {
        if self.capacity().is_some_and(|capacity| payload.len() > capacity) {
            return Err(format!("value of '{}' is too long", name));
        }
        let long = |top: u8, extended: u8, body: &[u8], out: &mut Vec<(u8, Vec<u8>)>| {
            let pieces: Vec<&[u8]> = if body.is_empty() { vec![body] } else { body.chunks(LONG_EXTENDED_FRAGMENT).collect() };
            for (index, piece) in pieces.iter().enumerate() {
                let flags = if index + 1 < pieces.len() { LONG_EXTENDED_MORE } else { 0 };
                out.push((top, [&[extended, flags][..], piece].concat()));
            }
        };
        match *self {
            Container::Plain(number) => out.push((number, payload.to_vec())),
            Container::Extended { top, extended } => out.push((top, [&[extended][..], payload].concat())),
            Container::LongExtended { top, extended } => long(top, extended, payload, out),
            Container::Evs { top, long: false, vendor, number } => {
                out.push((top, [&[EVS_TYPE][..], &vendor.to_be_bytes(), &[number], payload].concat()));
            }
            Container::Evs { top, long: true, vendor, number } => {
                long(top, EVS_TYPE, &[&vendor.to_be_bytes()[..], &[number], payload].concat(), out);
            }
            Container::Vendor { vendor, number, format } => {
                let flag_size = usize::from(format.continuation);
                let header = format.type_size + format.length_size + flag_size;
                let piece_size = MAX_ATTRIBUTE_VALUE - 4 - header;
                let pieces: Vec<&[u8]> = if payload.is_empty() { vec![payload] } else { payload.chunks(piece_size).collect() };
                for (index, piece) in pieces.iter().enumerate() {
                    let mut attribute = vendor.to_be_bytes().to_vec();
                    attribute.extend_from_slice(&number.to_be_bytes()[4 - format.type_size..]);
                    let length = ((header + piece.len()) as u32).to_be_bytes();
                    attribute.extend_from_slice(&length[4 - format.length_size..]);
                    if format.continuation {
                        attribute.push(if index + 1 < pieces.len() { VENDOR_CONTINUATION } else { 0 });
                    }
                    attribute.extend_from_slice(piece);
                    out.push((VENDOR_SPECIFIC, attribute));
                }
            }
        }
        Ok(())
    }
}

/// A TLV's path inside its attribute, its value, and the attribute's name for any error.
type TlvItem<'a> = (Vec<u32>, Vec<u8>, &'a str);

/// Where this attribute goes, and the TLV path inside that.
fn locate<'a>(dictionary: &Dictionary, attribute: &'a Attribute) -> Result<(Container, &'a [u32]), String> {
    let unsupported = || format!("attribute '{}' has an unsupported number", attribute.name);
    let octet = |number: u32| u8::try_from(number).map_err(|_| unsupported());
    let number = attribute.number.as_slice();
    let kind = |number: &[u32]| dictionary.get(0, number).map(|attribute| attribute.data_type.as_str());
    let (container, depth) = if attribute.vendor == 0 {
        let &first = number.first().ok_or_else(unsupported)?;
        match (kind(&[first]), number.get(1)) {
            (Some("extended"), Some(&extended)) => (Container::Extended { top: octet(first)?, extended: octet(extended)? }, 2),
            (Some("long-extended"), Some(&extended)) => (Container::LongExtended { top: octet(first)?, extended: octet(extended)? }, 2),
            _ => (Container::Plain(octet(first)?), 1),
        }
    } else if number.len() >= 3 && kind(&number[..2]) == Some("evs") {
        let long = kind(&number[..1]) == Some("long-extended");
        (Container::Evs { top: octet(number[0])?, long, vendor: attribute.vendor, number: octet(number[2])? }, 3)
    } else {
        let format = dictionary.vendor_format(attribute.vendor);
        let &first = number.first().ok_or_else(unsupported)?;
        if format.type_size < 4 && first >= 1 << (8 * format.type_size) {
            return Err(unsupported());
        }
        (Container::Vendor { vendor: attribute.vendor, number: first, format }, 1)
    };
    let path = &number[depth..];
    for index in 0..path.len() {
        let parent = dictionary.get(attribute.vendor, &number[..depth + index]);
        if parent.is_none_or(|parent| parent.data_type != "tlv") || !(1..=255).contains(&path[index]) {
            return Err(unsupported());
        }
    }
    Ok((container, path))
}

/// Each TLV at this level, of at most `tlv_max` octets, as its octets and the name of what is last
/// in it. Those with the same parent next to each other go under it, and under another of it when
/// they do not fit: whatever fits on its own fits here, so a script that was told its value is fine
/// is not refused later. A value of no octets cannot be a TLV: RFC 6929 section 2.3.
fn render_tlvs<'a>(items: &[(&[u32], &[u8], &'a str)], tlv_max: usize) -> Result<Vec<(Vec<u8>, &'a str)>, String> {
    let value_max = tlv_max - 2;
    let wrap = |kind: u32, value: &[u8], name: &str| -> Result<Vec<u8>, String> {
        if value.is_empty() {
            return Err(format!("value of '{}' is empty, which a TLV cannot be", name));
        }
        if value.len() > value_max {
            return Err(format!("value of '{}' is too long", name));
        }
        Ok([&[kind as u8, (value.len() + 2) as u8][..], value].concat())
    };
    let mut rendered = Vec::new();
    let mut at = 0;
    while at < items.len() {
        let (path, value, name) = items[at];
        if let [kind] = path {
            rendered.push((wrap(*kind, value, name)?, name));
            at += 1;
            continue;
        }
        let same = items[at..].iter().take_while(|(other, _, _)| other.len() > 1 && other[0] == path[0]).count();
        let inner: Vec<(&[u32], &[u8], &str)> = items[at..at + same].iter().map(|(other, value, name)| (&other[1..], *value, *name)).collect();
        let mut body = Vec::new();
        let mut last = name;
        for (tlv, name) in render_tlvs(&inner, value_max)? {
            if !body.is_empty() && body.len() + tlv.len() > value_max {
                rendered.push((wrap(path[0], &body, last)?, last));
                body.clear();
            }
            body.extend_from_slice(&tlv);
            last = name;
        }
        rendered.push((wrap(path[0], &body, last)?, last));
        at += same;
    }
    Ok(rendered)
}

/// Adds the attributes of one `tlv`, starting another when the next TLV does not fit.
fn write_tlvs(container: Container, items: &[TlvItem], out: &mut Vec<(u8, Vec<u8>)>) -> Result<(), String> {
    let items: Vec<(&[u32], &[u8], &str)> = items.iter().map(|(path, value, name)| (path.as_slice(), value.as_slice(), *name)).collect();
    // One octet of length, whatever the attribute around it could hold.
    let tlv_max = container.capacity().map_or(MAX_TLV, |capacity| capacity.min(MAX_TLV));
    let mut payload = Vec::new();
    let mut last = "";
    for (tlv, name) in render_tlvs(&items, tlv_max)? {
        if !payload.is_empty() && container.capacity().is_some_and(|capacity| payload.len() + tlv.len() > capacity) {
            container.write(&payload, last, out)?;
            payload.clear();
        }
        payload.extend_from_slice(&tlv);
        last = name;
    }
    container.write(&payload, last, out)
}

/// Encodes `set_attr(name, value)` pairs as wire attributes using the dictionary, encrypting a
/// value with the secret and authenticator if its attribute calls for that. `salts` holds those
/// already used in this response. Attributes next to each other under the same `tlv` share an
/// attribute.
pub fn encode_response_attributes<'a>(
    dictionary: &Dictionary,
    attributes: impl IntoIterator<Item = (&'a str, &'a AttrValue)>,
    shared_secret: &[u8],
    request_authenticator: &[u8; 16],
    salts: &mut Vec<[u8; 2]>,
) -> Result<Vec<(u8, Vec<u8>)>, String> {
    let mut out = Vec::new();
    // The TLVs waiting to share an attribute.
    let mut open: Option<(Container, Vec<TlvItem>)> = None;
    for (name, value) in attributes {
        let (attribute, tag) = attribute_and_tag(dictionary, name)?;
        let (container, path) = locate(dictionary, attribute)?;
        let on_its_own = matches!(container, Container::Plain(_) | Container::Vendor { .. }) && path.is_empty();
        if !on_its_own && (has_option(attribute, "has_tag") || attribute.options.as_deref().is_some_and(|options| options.contains("encrypt="))) {
            return Err(format!("'{}' cannot have a tag or be encrypted where it is defined", attribute.name));
        }
        let bytes = encode_value(dictionary, attribute, tag, value, shared_secret, request_authenticator, salts)?;
        if !open.as_ref().is_some_and(|(open_container, _)| !path.is_empty() && *open_container == container) {
            if let Some((open_container, items)) = open.take() {
                write_tlvs(open_container, &items, &mut out)?;
            }
        }
        if path.is_empty() {
            container.write(&bytes, &attribute.name, &mut out)?;
        } else {
            open.get_or_insert_with(|| (container, Vec::new())).1.push((path.to_vec(), bytes, &attribute.name));
        }
    }
    if let Some((open_container, items)) = open {
        write_tlvs(open_container, &items, &mut out)?;
    }
    Ok(out)
}

/// The octets of a value, as its attribute's dictionary type has them.
fn encode_value(
    dictionary: &Dictionary,
    attribute: &Attribute,
    tag: Option<u8>,
    value: &AttrValue,
    shared_secret: &[u8],
    request_authenticator: &[u8; 16],
    salts: &mut Vec<[u8; 2]>,
) -> Result<Vec<u8>, String> {
    let name = attribute.name.as_str();
    let as_number = |max_bytes: usize| -> Result<Vec<u8>, String> {
        let n = match value {
            AttrValue::Number(n) => *n,
            AttrValue::Bytes(b) => {
                let text = String::from_utf8_lossy(b);
                match dictionary.value_number(name, &text) {
                    Some(v) => v as f64,
                    None => text.parse::<f64>().map_err(|_| format!("'{}' is not a value of '{}'", text, name))?,
                }
            }
        };
        let limit = if max_bytes == 8 { u64::MAX as f64 } else { ((1u64 << (8 * max_bytes)) - 1) as f64 };
        if n < 0.0 || n > limit || n.fract() != 0.0 {
            return Err(format!("value {} out of range for '{}'", n, name));
        }
        Ok((n as u64).to_be_bytes()[8 - max_bytes..].to_vec())
    };
    let bytes = match attribute.data_type.as_str() {
        "string" | "text" | "octets" => match value {
            AttrValue::Bytes(b) => b.clone(),
            AttrValue::Number(n) => n.to_string().into_bytes(),
        },
        "integer" | "date" => as_number(4)?,
        "signed" => {
            let n = match value {
                AttrValue::Number(n) => *n,
                AttrValue::Bytes(b) => String::from_utf8_lossy(b)
                    .parse::<f64>()
                    .map_err(|_| format!("'{}' needs a number", name))?,
            };
            if n < i32::MIN as f64 || n > i32::MAX as f64 || n.fract() != 0.0 {
                return Err(format!("value {} out of range for '{}'", n, name));
            }
            (n as i32).to_be_bytes().to_vec()
        }
        "byte" => as_number(1)?,
        "short" => as_number(2)?,
        "integer64" => as_number(8)?,
        "ipaddr" => match value {
            AttrValue::Bytes(b) => String::from_utf8_lossy(b)
                .parse::<Ipv4Addr>()
                .map_err(|_| format!("'{}' needs an IPv4 address", name))?
                .octets()
                .to_vec(),
            AttrValue::Number(_) => return Err(format!("'{}' needs an IPv4 address string", name)),
        },
        "ipv6addr" => match value {
            AttrValue::Bytes(b) => String::from_utf8_lossy(b)
                .parse::<Ipv6Addr>()
                .map_err(|_| format!("'{}' needs an IPv6 address", name))?
                .octets()
                .to_vec(),
            AttrValue::Number(_) => return Err(format!("'{}' needs an IPv6 address string", name)),
        },
        kind @ ("ipv6prefix" | "ipv4prefix" | "ifid" | "ether" | "combo-ip") => {
            let text = match value {
                AttrValue::Bytes(b) => String::from_utf8_lossy(b),
                AttrValue::Number(_) => return Err(format!("'{}' needs a {} string", name, kind)),
            };
            encode_address(kind, &text).ok_or_else(|| format!("'{}' is not a valid {} for '{}'", text, kind, name))?
        }
        other => return Err(format!("attribute type '{}' of '{}' is not supported by set_attr", other, name)),
    };
    let is_string = matches!(attribute.data_type.as_str(), "string" | "text" | "octets");
    if has_option(attribute, "encrypt=1") || has_option(attribute, "encrypt=3") {
        // Sending these as they are would put a secret on the wire in plain text.
        return Err(format!("'{}' uses an encryption set_attr does not support", name));
    }
    let bytes = if has_option(attribute, "encrypt=2") {
        if !is_string {
            return Err(format!("'{}' is encrypted but is not a string", name));
        }
        // A copy of the secret, or of a session key, which nothing else clears.
        let plain = Zeroizing::new(bytes);
        encrypt_salted(&plain, shared_secret, request_authenticator, salts).map_err(|e| format!("'{}': {}", name, e))?
    } else {
        bytes
    };
    let bytes = if !has_option(attribute, "has_tag") {
        bytes
    } else if attribute.data_type == "integer" {
        // RFC 2868: the tag takes the top octet of the integer.
        if bytes[0] != 0 {
            return Err(format!("value of '{}' does not fit in three octets", name));
        }
        [&[tag.unwrap_or(0)], &bytes[1..]].concat()
    } else if is_string && has_option(attribute, "encrypt=2") {
        [&[tag.unwrap_or(0)], bytes.as_slice()].concat()
    } else if is_string {
        // Without a tag, a first octet of 0x1F or below would be read as one.
        match (tag, bytes.first()) {
            (Some(tag), _) => [&[tag], bytes.as_slice()].concat(),
            (None, Some(first)) if *first <= 0x1F => [&[0], bytes.as_slice()].concat(),
            (None, _) => bytes,
        }
    } else {
        return Err(format!("attribute type '{}' of '{}' cannot have a tag", attribute.data_type, name));
    };
    Ok(bytes)
}

/// The attribute with this vendor type from the request's Microsoft attributes.
fn microsoft_attribute(packet: &RadiusPacket, vendor_type: u8) -> Option<&[u8]> {
    packet.attributes.iter().filter(|(attr_type, _)| *attr_type == VENDOR_SPECIFIC).find_map(|(_, value)| {
        let (vendor, mut rest) = value.split_first_chunk::<4>()?;
        if u32::from_be_bytes(*vendor) != MICROSOFT_VENDOR_ID {
            return None;
        }
        // One Vendor-Specific can hold several vendor attributes, as `request_attributes` reads them.
        while let [inner_type, inner_length, ..] = rest {
            let length = usize::from(*inner_length);
            if length < 2 || length > rest.len() {
                return None;
            }
            if *inner_type == vendor_type {
                return Some(&rest[2..length]);
            }
            rest = &rest[length..];
        }
        None
    })
}

/// RFC 2548: MS-CHAP-Challenge and MS-CHAP2-Response, in their only valid lengths.
fn mschapv2_secret(packet: &RadiusPacket) -> Option<Secret> {
    let challenge: [u8; 16] = microsoft_attribute(packet, MS_CHAP_CHALLENGE)?.try_into().ok()?;
    let response = microsoft_attribute(packet, MS_CHAP2_RESPONSE)?;
    let [ident, _flags, rest @ ..] = response else { return None };
    let rest: &[u8; 48] = rest.try_into().ok()?;
    Some(Secret::MsChapV2 {
        ident: *ident,
        peer_challenge: rest[..16].try_into().ok()?,
        response: rest[24..].try_into().ok()?,
        challenge,
    })
}

/// The user name and secret of an Access-Request: User-Password (decrypted) as
/// PAP, or CHAP-Password with CHAP-Challenge, falling back to the Request
/// Authenticator as the challenge (RFC 2865 section 2.2).
pub fn request_credentials(packet: &RadiusPacket, shared_secret: &[u8]) -> Credentials {
    let user = packet.attribute(1).map(|name| String::from_utf8_lossy(name).into_owned());
    let secret = if let Some(cipher) = packet.attribute(USER_PASSWORD) {
        Secret::pap(decrypt_user_password(cipher, shared_secret, &packet.authenticator))
    } else if let Some(chap) = packet.attribute(CHAP_PASSWORD).filter(|chap| chap.len() == 17) {
        let challenge = packet.attribute(CHAP_CHALLENGE).map(<[u8]>::to_vec).unwrap_or_else(|| packet.authenticator.to_vec());
        Secret::Chap { id: chap[0], response: chap[1..].to_vec(), challenge }
    } else {
        mschapv2_secret(packet).unwrap_or(Secret::None)
    };
    Credentials { user, secret }
}

/// The authentication record of a finished EAP conversation.
fn eap_record(client: &Client, source: IpAddr, user: &str, account: &str, handler: &'static str, code: u8) {
    crate::logging::auth_record(&crate::logging::AuthRecord {
        protocol: "radius",
        client: &client.name,
        source,
        user,
        account,
        chain: "eap",
        handler,
        decision: if code == ACCESS_ACCEPT { "accept" } else { "reject" },
    });
}

/// Writes a request from `client` to `spool`, and says whether it is now on disk; `label` names
/// the spool in the log. `received` is when the request came, in Unix seconds.
pub async fn spool_request(spool: &Spool, label: &str, client: &Client, data: &[u8], source: IpAddr, received: u64) -> bool {
    let request = SpooledRequest { client: client.name.clone(), source, received, packet: data.to_vec(), radsec: client.secret == crate::clients::RADSEC_SECRET };
    match spool.write(&request).await {
        Ok(()) => true,
        Err(e) => {
            log_line(tracing::Level::ERROR, format!("{label}: request from client {} not written: {e}", client.name));
            false
        }
    }
}

/// Records a decision another server made for a forwarded request: a challenge is a round of a
/// conversation still going, and accounting has no record.
fn record_forwarded(client: &Client, source: IpAddr, packet: &RadiusPacket, response: &[u8], account: &str, realm: &str) {
    let decision = match response.first() {
        Some(&ACCESS_ACCEPT) => "accept",
        Some(&ACCESS_REJECT) => "reject",
        _ => return,
    };
    let user = String::from_utf8_lossy(packet.attribute(1).unwrap_or_default());
    crate::logging::auth_record(&crate::logging::AuthRecord { protocol: "radius", client: &client.name, source, user: &user, account, chain: "proxy", handler: realm, decision });
}

/// What came of replaying one spooled request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Replayed {
    Accepted,
    TryAgain,
    Refused(&'static str),
}

/// How many results of a window, in the order written, are settled: those before the first
/// that no server answered. The position in a spool can only move past them in order.
fn settled(results: &[Replayed]) -> usize {
    results.iter().position(|result| *result == Replayed::TryAgain).unwrap_or(results.len())
}

/// How long an answered request stays in the duplicate cache; RFC 5080 section
/// 2.2.2 asks for 5 to 30 seconds.
const DUPLICATE_RETENTION: Duration = Duration::from_secs(10);

/// Most requests kept in the duplicate cache; further ones are served uncached.
const DUPLICATE_MAX_ENTRIES: usize = 500_000;

/// Default limit of requests handled at once, per port.
pub const DEFAULT_MAX_INFLIGHT: usize = 4096;

/// Decides when to log discards: at most once per second, with how many there
/// were since the last line, so overload does not also flood the log.
#[derive(Default)]
pub struct DiscardLog {
    pending: u64,
    last_logged: Option<Instant>,
}

impl DiscardLog {
    /// Counts one discard; returns the number to log when a line is due.
    pub fn record(&mut self, now: Instant) -> Option<u64> {
        self.pending += 1;
        if self.last_logged.is_some_and(|last| now.duration_since(last) < Duration::from_secs(1)) {
            return None;
        }
        self.last_logged = Some(now);
        Some(std::mem::take(&mut self.pending))
    }

    /// Counts one, and gives the line to write when one is due: its text, with how many there
    /// have been when that is more than this one.
    pub fn line(&mut self, now: Instant, text: impl FnOnce() -> String) -> Option<String> {
        let count = self.record(now)?;
        Some(if count > 1 { format!("{} ({count} since the last line)", text()) } else { text() })
    }
}

/// An Access-Reject holding `eap`, or without it when it does not fit beside what the request
/// has to get back.
fn eap_rejection(packet: &RadiusPacket, eap: &[u8], secret: &[u8]) -> Option<Vec<u8>> {
    let response = RadiusPacket::create_response(ACCESS_REJECT, packet, split_message(eap), secret);
    if response.len() <= MAX_PACKET_LEN {
        return Some(response);
    }
    let response = RadiusPacket::create_response(ACCESS_REJECT, packet, Vec::new(), secret);
    (response.len() <= MAX_PACKET_LEN).then_some(response)
}

/// How long to wait after `accept` fails. The usual cause is having no file descriptors
/// left, which trying again at once only makes worse by spinning.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// The next connection. A failed `accept` is logged and tried again: it is usually about
/// one connection or a passing shortage, and a listener that gave up would leave the
/// port closed until a restart, with the process still running.
pub async fn accept_next(listener: &TcpListener, what: &str, failures: &mut DiscardLog) -> (TcpStream, SocketAddr) {
    retry_accept(|| listener.accept(), what, failures, ACCEPT_RETRY).await
}

async fn retry_accept<T, F, Fut>(mut accept: F, what: &str, failures: &mut DiscardLog, wait: Duration) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = io::Result<T>>,
{
    loop {
        match accept().await {
            Ok(accepted) => return accepted,
            Err(e) => {
                if let Some(count) = failures.record(Instant::now()) {
                    tracing::warn!("{what}: accepting a connection failed {count} times, the last with: {e}");
                }
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// The port a packet arrived on, which separates its requests from the other's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketKind {
    Auth,
    Acct,
}

/// The port, the socket it came to if there is one to tell, the source and the Identifier.
type DuplicateKey = (PacketKind, Option<SocketAddr>, SocketAddr, u8);

enum DuplicateState {
    InFlight,
    Done(Vec<u8>),
}

struct DuplicateEntry {
    authenticator: [u8; 16],
    /// Tells this request from a later one that took over the same key.
    generation: u64,
    state: DuplicateState,
}

#[derive(Default)]
struct DuplicateEntries {
    entries: HashMap<DuplicateKey, DuplicateEntry>,
    /// Answered requests in the order they expire; retention is fixed.
    expiry: VecDeque<(Instant, DuplicateKey, u64)>,
    next_generation: u64,
}

/// Detects retransmissions as RFC 5080 section 2.2.2 describes: an answered
/// request is answered again from here, and one still in progress is discarded.
pub struct DuplicateCache {
    retention: Duration,
    max_entries: usize,
    inner: Mutex<DuplicateEntries>,
}

/// What to do with a request, from `DuplicateCache::begin`.
pub enum DuplicateCheck<'a> {
    /// A retransmission of an answered request: send this response again.
    Replay(Vec<u8>),
    /// A retransmission of a request still in progress: send nothing.
    Discard,
    /// A new request: process it and hand the response to the ticket.
    Process(DuplicateTicket<'a>),
    /// A new request while the cache is full: process it, nothing is kept.
    ProcessUncached,
}

/// A request in progress. Dropping it without `finish` forgets the request, so
/// a retransmission is processed again.
pub struct DuplicateTicket<'a> {
    cache: &'a DuplicateCache,
    key: DuplicateKey,
    generation: u64,
}

impl DuplicateCache {
    pub fn new(retention: Duration, max_entries: usize) -> Self {
        Self { retention, max_entries, inner: Mutex::default() }
    }

    pub fn begin(&self, kind: PacketKind, receiving: Option<SocketAddr>, source: SocketAddr, identifier: u8, authenticator: [u8; 16]) -> DuplicateCheck<'_> {
        let key = (kind, receiving, source, identifier);
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.remove_expired(Instant::now());
        let replaces = match inner.entries.get(&key) {
            Some(entry) if entry.authenticator == authenticator => {
                return match &entry.state {
                    DuplicateState::InFlight => DuplicateCheck::Discard,
                    DuplicateState::Done(response) => DuplicateCheck::Replay(response.clone()),
                };
            }
            Some(_) => true,
            None => false,
        };
        if !replaces && inner.entries.len() >= self.max_entries {
            return DuplicateCheck::ProcessUncached;
        }
        let generation = inner.next_generation;
        inner.next_generation += 1;
        inner.entries.insert(key, DuplicateEntry { authenticator, generation, state: DuplicateState::InFlight });
        DuplicateCheck::Process(DuplicateTicket { cache: self, key, generation })
    }
}

impl DuplicateEntries {
    fn remove_expired(&mut self, now: Instant) {
        while let Some((expires, key, generation)) = self.expiry.front().copied() {
            if expires > now {
                return;
            }
            self.expiry.pop_front();
            if self.entries.get(&key).is_some_and(|entry| entry.generation == generation) {
                self.entries.remove(&key);
            }
        }
    }
}

impl DuplicateTicket<'_> {
    /// Records the response, unless a later request has taken over the key.
    pub fn finish(self, response: &[u8]) {
        let mut inner = self.cache.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = inner.entries.get_mut(&self.key) {
            if entry.generation == self.generation {
                entry.state = DuplicateState::Done(response.to_vec());
                let expires = Instant::now() + self.cache.retention;
                inner.expiry.push_back((expires, self.key, self.generation));
            }
        }
    }
}

impl Drop for DuplicateTicket<'_> {
    fn drop(&mut self) {
        let mut inner = self.cache.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.entries.get(&self.key).is_some_and(|e| e.generation == self.generation && matches!(e.state, DuplicateState::InFlight)) {
            inner.entries.remove(&self.key);
        }
    }
}

/// The values a client's `auto` settings became.
#[derive(Debug, Clone, Copy, Default)]
struct Learned {
    require: Option<bool>,
    limit: Option<bool>,
}

/// What a reload replaces. A request reads this once and keeps it until it is done.
pub struct RadiusState {
    pub clients: Arc<ClientTable>,
    /// `clients` as they are over RadSec.
    pub radsec_clients: ClientTable,
    pub engine: Arc<LuaAuthEngine>,
    /// Decides authentication for each client with the chain it selects;
    /// without a chain the top-level script does.
    pub chains: Option<Arc<AuthChains>>,
    /// Which requests to write out in full, for how long.
    pub trace: Option<Arc<Trace>>,
    /// What forwards requests to other servers, such as those of `[proxy]`.
    pub forwarder: Option<Arc<dyn crate::forwarding::Forwarder>>,
    /// Clients the configuration does not list, found by their UDP source, such as those of
    /// `[radius.dynamic_clients]`.
    pub client_source: Option<Arc<dyn crate::client_source::ClientSource>>,
    /// What proves the identity of requests before the chain runs, such as `[radius.dpsk]`.
    pub pre_chain: Option<Arc<dyn crate::pre_chain::PreChainAuthenticator>>,
}

/// `[log.trace]`, from when it was loaded.
pub struct Trace {
    users: Vec<String>,
    clients: Vec<String>,
    until: Instant,
}

impl Trace {
    fn new(settings: &crate::config::TraceSettings) -> Self {
        Self { users: settings.users.clone(), clients: settings.clients.clone(), until: Instant::now() + settings.duration }
    }

    fn covers(&self, client: &str, user: &[u8]) -> bool {
        Instant::now() < self.until && (self.clients.iter().any(|name| name == client) || self.users.iter().any(|name| name.as_bytes() == user))
    }
}

/// Standard attributes a trace hides by number, whatever the dictionary calls them: User-Password
/// is decrypted by its number, CHAP-Password, CHAP-Challenge, ARAP-Password and Tunnel-Password
/// prove something, State and Message-Authenticator open a conversation, and EAP-Message holds one.
const TRACE_HIDDEN_NUMBERS: [u32; 8] = [2, 3, 24, 60, 69, 70, 79, 80];
/// What in a name, in lowercase, means its value is hidden in a trace. More than strictly needs it:
/// a value hidden can be looked for another way, and one written out cannot be taken back.
const TRACE_HIDDEN_WORDS: [&str; 13] = ["pass", "secret", "key", "token", "chap", "arap", "-pw", "-cpw", "digest", "signature", "psk", "pmk", "credential"];

/// Whether a trace hides this attribute's value: by the dictionary's options, by what it is
/// called, or by its number. What a value holds is not looked at.
fn trace_hides(dictionary: &Dictionary, name: &str) -> bool {
    let marked = dictionary.get_by_name(name).and_then(|attribute| attribute.options.as_deref()).is_some_and(|options| {
        options.split([',', ' ']).any(|option| option == "secret" || option.starts_with("encrypt="))
    });
    let lowercase = name.to_ascii_lowercase();
    marked
        || TRACE_HIDDEN_WORDS.iter().any(|word| lowercase.contains(word))
        || TRACE_HIDDEN_NUMBERS.iter().any(|number| dictionary.get(0, &[*number]).is_some_and(|attribute| attribute.name == name))
}

/// How much of a value a trace shows, in characters or in octets of hex.
const TRACE_VALUE_LEN: usize = 128;

/// A packet's attributes for a trace, with secrets hidden.
fn trace_attributes(dictionary: &Dictionary, attributes: &[(String, AttrValue)]) -> String {
    let text: Vec<String> = attributes
        .iter()
        .map(|(name, value)| {
            let shown = match value {
                _ if trace_hides(dictionary, name) => "<hidden>".to_string(),
                AttrValue::Number(number) => number.to_string(),
                AttrValue::Bytes(bytes) => match std::str::from_utf8(bytes) {
                    Ok(text) if !text.chars().any(char::is_control) => text.chars().take(TRACE_VALUE_LEN).collect(),
                    _ => format!("0x{}", bytes.iter().take(TRACE_VALUE_LEN / 2).map(|byte| format!("{byte:02x}")).collect::<String>()),
                },
            };
            format!("{name} = {shown}")
        })
        .collect();
    text.join(", ")
}

/// Writes out a request and what came of it, for `[log.trace]`.
fn trace_exchange(dictionary: &Dictionary, client: &Client, source: IpAddr, packet: &RadiusPacket, response: Option<&[u8]>) {
    let user = String::from_utf8_lossy(packet.attribute(1).unwrap_or_default());
    let attributes = trace_attributes(dictionary, &request_attributes(packet, dictionary, &client.secret));
    tracing::info!(target: crate::logging::TRACE_TARGET, client = client.name.as_str(), source = %source, user = user.as_ref(), code = packet.code, attributes = attributes.as_str(), "request");
    let parsed = response.and_then(|response| RadiusPacket::parse(response).ok());
    match parsed {
        Some(parsed) => {
            let attributes = trace_attributes(dictionary, &request_attributes(&parsed, dictionary, b""));
            tracing::info!(target: crate::logging::TRACE_TARGET, client = client.name.as_str(), source = %source, user = user.as_ref(), code = parsed.code, attributes = attributes.as_str(), "response");
        }
        None => tracing::info!(target: crate::logging::TRACE_TARGET, client = client.name.as_str(), source = %source, user = user.as_ref(), "no response"),
    }
}

impl RadiusState {
    pub fn new(clients: Arc<ClientTable>, engine: Arc<LuaAuthEngine>, chains: Option<Arc<AuthChains>>) -> Self {
        let radsec_clients = clients.for_radsec();
        Self { clients, radsec_clients, engine, chains, trace: None, forwarder: None, client_source: None, pre_chain: None }
    }

    /// `[radius.tls] network_clients_need_tls_name`.
    pub fn with_radsec_network_names(mut self, on: bool) -> Self {
        self.radsec_clients = self.radsec_clients.with_networks_needing_tls_name(on);
        self
    }


    pub fn with_pre_chain(mut self, pre_chain: Option<Arc<dyn crate::pre_chain::PreChainAuthenticator>>) -> Self {
        self.pre_chain = pre_chain;
        self
    }

    pub fn with_client_source(mut self, client_source: Option<Arc<dyn crate::client_source::ClientSource>>) -> Self {
        self.client_source = client_source;
        self
    }

    /// With what forwards requests to other servers.
    pub fn with_forwarder(mut self, forwarder: Option<Arc<dyn crate::forwarding::Forwarder>>) -> Self {
        self.forwarder = forwarder;
        self
    }

    /// Tells the forwarder, if any, that requests use it from now on.
    fn forwarder_in_use(&self) {
        if let Some(forwarder) = &self.forwarder {
            forwarder.in_use();
        }
    }

    /// With `[log.trace]`, which starts counting its time from here.
    pub fn with_trace(mut self, settings: Option<&crate::config::TraceSettings>) -> Self {
        self.trace = settings.map(|settings| {
            tracing::warn!("[log.trace] is on for {} minutes: requests of these users and clients are written out in full, personal data included", settings.duration.as_secs() / 60);
            Arc::new(Trace::new(settings))
        });
        self
    }

    /// The name of every client, over UDP or RadSec.
    fn client_names(&self) -> impl Iterator<Item = &str> {
        self.clients.names().chain(self.radsec_clients.names())
    }
}

pub struct RadiusServer {
    pub auth_addr: SocketAddr,
    pub acct_addr: SocketAddr,
    /// The other addresses to listen on, on the same ports.
    more_binds: Vec<IpAddr>,
    /// Sockets on each address and port; above 1 they share it with SO_REUSEPORT.
    sockets: usize,
    pub dictionary: Arc<Dictionary>,
    state: Live<RadiusState>,
    #[doc(hidden)]
    pub metrics: Arc<Metrics>,
    duplicates: DuplicateCache,
    status_server: bool,
    /// What `auto` settled on for each single-host client, until the server restarts.
    learned: Mutex<HashMap<IpAddr, Learned>>,
    /// Where accounting goes while the database cannot take it.
    spool: Option<Arc<Spool>>,
    eap: EapService,
    policy_discards: Mutex<DiscardLog>,
    /// EAP conversations refused for overload, logged a line a second at most, for each reason.
    eap_overload_lines: Mutex<DiscardLog>,
    eap_vector_lines: Mutex<DiscardLog>,
    stranger_discards: Mutex<DiscardLog>,
    /// Requests discarded because DNS cannot tell their realm now.
    unknown_realm_lines: Mutex<DiscardLog>,
    auth_overload_lines: Mutex<DiscardLog>,
    acct_overload_lines: Mutex<DiscardLog>,
    /// Requests handled at once, separately so accounting cannot starve authentication.
    auth_slots: Arc<Semaphore>,
    acct_slots: Arc<Semaphore>,
}

impl RadiusServer {
    pub fn new(auth_addr: SocketAddr, acct_addr: SocketAddr, clients: Arc<ClientTable>, engine: Arc<LuaAuthEngine>, dictionary: Arc<Dictionary>) -> Self {
        Self::from_state(auth_addr, acct_addr, RadiusState::new(clients, engine, None), dictionary)
    }

    pub fn from_state(auth_addr: SocketAddr, acct_addr: SocketAddr, state: RadiusState, dictionary: Arc<Dictionary>) -> Self {
        let metrics: Arc<Metrics> = Arc::default();
        metrics.set_radius_clients(state.client_names());
        state.forwarder_in_use();
        Self {
            auth_addr,
            acct_addr,
            more_binds: Vec::new(),
            sockets: 1,
            dictionary,
            state: Live::new(state),
            metrics,
            duplicates: DuplicateCache::new(DUPLICATE_RETENTION, DUPLICATE_MAX_ENTRIES),
            status_server: true,
            learned: Mutex::default(),
            spool: None,
            eap: EapService::default(),
            policy_discards: Mutex::default(),
            eap_overload_lines: Mutex::default(),
            eap_vector_lines: Mutex::default(),
            stranger_discards: Mutex::default(),
            unknown_realm_lines: Mutex::default(),
            auth_overload_lines: Mutex::default(),
            acct_overload_lines: Mutex::default(),
            auth_slots: Arc::new(Semaphore::new(DEFAULT_MAX_INFLIGHT)),
            acct_slots: Arc::new(Semaphore::new(DEFAULT_MAX_INFLIGHT)),
        }
    }

    /// A duplicate cache small or short-lived enough for a test to reach its limits.
    #[cfg(test)]
    fn with_duplicate_cache(mut self, retention: Duration, max_entries: usize) -> Self {
        self.duplicates = DuplicateCache::new(retention, max_entries);
        self
    }

    /// Sets where this server counts what it handles.
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        metrics.set_radius_clients(self.state.get().client_names());
        self.metrics = metrics;
        self
    }

    /// Also listens on these addresses, on the same ports.
    pub fn with_more_binds(mut self, addresses: Vec<IpAddr>) -> Self {
        self.more_binds = addresses;
        self
    }

    /// Opens `sockets` sockets on each address and port, each with its own receive loop.
    pub fn with_sockets(mut self, sockets: usize) -> Self {
        self.sockets = sockets.max(1);
        self
    }

    /// Sets whether Status-Server packets are answered.
    pub fn with_status_server(mut self, enabled: bool) -> Self {
        self.status_server = enabled;
        self
    }

    /// Sets how many authentication and accounting requests are handled at once.
    pub fn with_eap(self, settings: Option<Arc<EapSettings>>) -> Self {
        self.eap.set_settings(settings);
        self
    }

    /// New EAP conversations start with these.
    pub fn eap_settings(&self) -> Option<Arc<EapSettings>> {
        self.eap.settings()
    }

    pub fn replace_eap(&self, settings: Option<Arc<EapSettings>>) {
        self.eap.set_settings(settings);
    }

    pub fn with_spool(mut self, spool: Arc<Spool>) -> Self {
        self.spool = Some(spool);
        self
    }

    pub fn with_limits(mut self, max_auth_inflight: usize, max_acct_inflight: usize) -> Self {
        self.auth_slots = Arc::new(Semaphore::new(max_auth_inflight));
        self.acct_slots = Arc::new(Semaphore::new(max_acct_inflight));
        self
    }

    pub fn with_auth_chains(self, chains: Arc<AuthChains>) -> Self {
        let state = self.state.get();
        self.state.set(RadiusState::new(state.clients.clone(), state.engine.clone(), Some(chains)));
        self
    }

    /// What requests read from now on; those being handled finish with what they read.
    pub fn replace_state(&self, state: RadiusState) {
        self.metrics.set_radius_clients(state.client_names());
        state.forwarder_in_use();
        self.state.set(state);
    }

    pub fn state(&self) -> Arc<RadiusState> {
        self.state.get()
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn dictionary(&self) -> &Arc<Dictionary> {
        &self.dictionary
    }

    #[cfg(test)]
    fn with_clients(self, clients: Arc<ClientTable>) -> Self {
        let state = self.state.get();
        self.state.set(RadiusState::new(clients, state.engine.clone(), state.chains.clone()));
        self
    }

    /// Serves both ports; each received packet is handled in its own task so a slow
    /// script or database wait does not hold up other packets. A packet beyond the
    /// port's limit is dropped before any task exists for it: no Access-Reject, since
    /// the NAS retransmits and a refusal would look like a wrong password.
    pub async fn run(self: Arc<Self>) -> io::Result<()> {
        let sockets = self.bind_sockets().await?;
        self.serve(sockets).await;
        Ok(())
    }

    /// Binds every address and port, or says which could not be.
    pub async fn bind_sockets(&self) -> io::Result<Vec<(PacketKind, UdpSocket)>> {
        // Listed together, `0.0.0.0` and `::` only get along if `::` keeps to IPv6.
        let shared = self.sockets > 1;
        let bind = |address: SocketAddr, only_v6: bool| -> io::Result<UdpSocket> {
            let socket = socket2::Socket::new(socket2::Domain::for_address(address), socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
            if address.is_ipv6() && only_v6 {
                socket.set_only_v6(true)?;
            }
            // The kernel hashes each source to one of them, so a retransmission comes back to the
            // same receive loop.
            if shared {
                socket.set_reuse_port(true)?;
            }
            socket.set_nonblocking(true)?;
            socket.bind(&address.into())?;
            UdpSocket::from_std(socket.into())
        };
        let named = |address: SocketAddr| move |e: io::Error| io::Error::new(e.kind(), format!("cannot listen on {address}: {e}"));
        let mut sockets = Vec::new();
        if self.more_binds.is_empty() && !shared {
            sockets.push((PacketKind::Auth, UdpSocket::bind(self.auth_addr).await.map_err(named(self.auth_addr))?));
            sockets.push((PacketKind::Acct, UdpSocket::bind(self.acct_addr).await.map_err(named(self.acct_addr))?));
        } else {
            // One address alone keeps the system's IPv6 default, as `UdpSocket::bind` does.
            let only_v6 = !self.more_binds.is_empty();
            for ip in std::iter::once(self.auth_addr.ip()).chain(self.more_binds.iter().copied()) {
                for (kind, port) in [(PacketKind::Auth, self.auth_addr.port()), (PacketKind::Acct, self.acct_addr.port())] {
                    let address = SocketAddr::new(ip, port);
                    for _ in 0..self.sockets {
                        sockets.push((kind, bind(address, only_v6).map_err(named(address))?));
                    }
                }
            }
        }
        Ok(sockets)
    }

    /// Serves on what `bind_sockets` bound, for as long as the server runs.
    pub async fn serve(self: Arc<Self>, sockets: Vec<(PacketKind, UdpSocket)>) {
        tracing::info!("RADIUS Server listening on {}:{} and {}:{}",
                 self.auth_addr.ip(), self.auth_addr.port(),
                 self.acct_addr.ip(), self.acct_addr.port());
        if self.sockets > 1 {
            tracing::info!("RADIUS: {} sockets on each address and port", self.sockets);
        }
        for address in &self.more_binds {
            tracing::info!("RADIUS Server also listening on {address}");
        }

        for (kind, socket) in sockets {
            tokio::spawn(self.clone().receive_loop(kind, Arc::new(socket)));
        }
        let mut eap_sweep = tokio::time::interval(EAP_SWEEP_INTERVAL);
        loop {
            eap_sweep.tick().await;
            self.eap.sweep(&self.metrics);
        }
    }

    /// Takes in what comes to `socket`, and answers from it, for as long as the server runs.
    /// A request that can be answered without waiting is answered here; one that has to wait,
    /// or that came with another packet already waiting, goes on in a task of its own, so
    /// that a slow script does not hold up the others and a busy socket's requests run on
    /// several threads. A request keeps its slot until its response is sent.
    async fn receive_loop(self: Arc<Self>, kind: PacketKind, socket: Arc<UdpSocket>) {
        let mut buffer = [0u8; MAX_PACKET_LEN];
        let receiving = socket.local_addr().ok();
        let mut next: Option<(Vec<u8>, SocketAddr)> = None;
        loop {
            let (data, addr) = match next.take() {
                Some(packet) => packet,
                None => {
                    let Ok((size, addr)) = socket.recv_from(&mut buffer).await else { continue };
                    (buffer[..size].to_vec(), addr)
                }
            };
            // Without waiting: is the next one here already?
            if let Ok((size, from)) = socket.try_recv_from(&mut buffer) {
                next = Some((buffer[..size].to_vec(), from));
            }
            let Some(slot) = self.try_slot(kind) else {
                self.metrics.radius_discarded_overload.inc();
                if let Some(count) = self.overload_line(kind, Instant::now()) {
                    let what = if matches!(kind, PacketKind::Auth) { "authentication" } else { "accounting" };
                    tracing::warn!("RADIUS {} overloaded: discarded {} packets", what, count);
                }
                continue;
            };
            let server = self.clone();
            let mut work = Box::pin(async move {
                match receiving {
                    Some(receiving) => server.receive_at(receiving, kind, &data, addr).await,
                    None => server.receive(kind, &data, addr).await,
                }
            });
            let socket = socket.clone();
            if next.is_some() {
                tokio::spawn(async move {
                    // Yield first: a freshly spawned task sits in this worker's LIFO slot, which other
                    // workers cannot steal, so a script that never yields would also stall the receive loop.
                    tokio::task::yield_now().await;
                    if let Some(response) = work.await {
                        let _ = socket.send_to(&response, addr).await;
                    }
                    drop(slot);
                });
                continue;
            }
            // A panic would otherwise end this loop, and nothing would answer on the socket again.
            match futures::poll!(futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(&mut work))) {
                std::task::Poll::Ready(Err(_)) => {
                    tracing::error!("RADIUS request from {addr} not answered: handling it panicked");
                    drop(slot);
                }
                std::task::Poll::Ready(Ok(None)) => drop(slot),
                std::task::Poll::Ready(Ok(Some(response))) => {
                    // Not waiting here for a full socket buffer: receiving goes on.
                    if socket.try_send_to(&response, addr).is_err() {
                        tokio::spawn(async move {
                            let _ = socket.send_to(&response, addr).await;
                            drop(slot);
                        });
                    }
                }
                std::task::Poll::Pending => {
                    tokio::spawn(async move {
                        if let Some(response) = work.await {
                            let _ = socket.send_to(&response, addr).await;
                        }
                        drop(slot);
                    });
                }
            }
        }
    }

    /// Counts a packet discarded on this port, and says how many to log when a line is due:
    /// one per second however many addresses it is bound to.
    fn overload_line(&self, kind: PacketKind, now: Instant) -> Option<u64> {
        let lines = match kind {
            PacketKind::Auth => &self.auth_overload_lines,
            PacketKind::Acct => &self.acct_overload_lines,
        };
        lines.lock().unwrap_or_else(PoisonError::into_inner).record(now)
    }

    /// A place among the requests of `kind` handled at once, shared by UDP and RadSec.
    pub fn try_slot(&self, kind: PacketKind) -> Option<OwnedSemaphorePermit> {
        let slots = match kind {
            PacketKind::Auth => &self.auth_slots,
            PacketKind::Acct => &self.acct_slots,
        };
        slots.clone().try_acquire_owned().ok()
    }

    /// Finds the client for `source`; unknown sources are logged and get `None`.
    fn client_for<'a>(&self, clients: &'a ClientTable, source: IpAddr) -> Option<&'a Client> {
        let client = clients.lookup(source);
        if client.is_none() {
            self.metrics.radius_discarded_unknown_client.inc();
            // Anyone can send these, and the log has more to say than this.
            if let Some(line) = self.stranger_discards.lock().unwrap_or_else(PoisonError::into_inner).line(Instant::now(), || format!("RADIUS packet from unknown client {source} discarded")) {
                tracing::warn!("{line}");
            }
        }
        client
    }

    /// Handles a received packet: checks it, answers retransmissions from the
    /// duplicate cache, and processes anything new. Only packets that pass the
    /// checks reach the cache. Returns the response to send, or `None` for none.
    pub async fn receive(&self, kind: PacketKind, data: &[u8], source: SocketAddr) -> Option<Vec<u8>> {
        let state = self.state.get();
        match self.dynamic_table(&state, source.ip()).await {
            Some(table) => self.receive_from(&state, &table, None, kind, data, source).await,
            None => self.receive_from(&state, &state.clients, None, kind, data, source).await,
        }
    }

    /// `receive` of a packet that came to the socket at `receiving`.
    pub async fn receive_at(&self, receiving: SocketAddr, kind: PacketKind, data: &[u8], source: SocketAddr) -> Option<Vec<u8>> {
        let state = self.state.get();
        match self.dynamic_table(&state, source.ip()).await {
            Some(table) => self.receive_from(&state, &table, Some(receiving), kind, data, source).await,
            None => self.receive_from(&state, &state.clients, Some(receiving), kind, data, source).await,
        }
    }

    /// The client `source` is in the client source, when it is no configured client.
    async fn dynamic_table(&self, state: &RadiusState, source: IpAddr) -> Option<Arc<ClientTable>> {
        let dynamic = state.client_source.as_ref()?;
        if state.clients.lookup(source).is_some() {
            return None;
        }
        dynamic.table_for(source).await
    }

    /// The client a spooled request came from: configured, or looked up again by its source in
    /// the client source when it has the same name. `Err` is what to do with the request
    /// when there is none: refuse it when it is gone, try again later when it cannot be known now.
    /// The client a spooled request came from, if it is still one: by name, from the
    /// configuration or from the client source.
    pub async fn spooled_client(&self, state: &RadiusState, spooled: &SpooledRequest) -> Result<Client, Replayed> {
        let gone = Replayed::Refused("the client no longer exists");
        let clients = if spooled.radsec { &state.radsec_clients } else { &state.clients };
        if let Some(client) = clients.clients().find(|client| client.name == spooled.client) {
            return Ok(client.clone());
        }
        let Some(dynamic) = state.client_source.as_ref().filter(|_| !spooled.radsec) else { return Err(gone) };
        match dynamic.answer_for(spooled.source).await {
            crate::client_source::Answer::Found(table) => table.lookup(spooled.source).filter(|client| client.name == spooled.client).cloned().ok_or(gone),
            crate::client_source::Answer::NotFound => Err(gone),
            crate::client_source::Answer::Unknown => Err(Replayed::TryAgain),
        }
    }

    /// `receive`, with `source` looked up in `clients`.
    pub async fn receive_from(&self, state: &RadiusState, clients: &ClientTable, receiving: Option<SocketAddr>, kind: PacketKind, data: &[u8], source: SocketAddr) -> Option<Vec<u8>> {
        if data.first() == Some(&STATUS_SERVER) {
            return self.answer_status_server(clients, kind, data, source.ip());
        }
        let (client, packet) = match kind {
            PacketKind::Auth => self.check_auth(clients, data, source.ip())?,
            PacketKind::Acct => self.check_acct(clients, data, source.ip())?,
        };
        let ticket = match self.duplicates.begin(kind, receiving, source, packet.identifier, packet.authenticator) {
            DuplicateCheck::Replay(response) => {
                self.metrics.radius_duplicates.inc();
                return Some(response);
            }
            DuplicateCheck::Discard => {
                self.metrics.radius_duplicates.inc();
                return None;
            }
            DuplicateCheck::Process(ticket) => Some(ticket),
            DuplicateCheck::ProcessUncached => None,
        };
        match kind {
            PacketKind::Auth => self.metrics.radius_packets_auth.inc(),
            PacketKind::Acct => self.metrics.radius_packets_acct.inc(),
        }
        let route = match &state.forwarder {
            // Before EAP, chains and scripts: none of them is this server's to run for a request
            // forwarded elsewhere.
            Some(forwarder) => forwarder.route(&crate::forwarding::ForwardRequest { kind, client, packet: &packet, data, source: source.ip() }).await,
            None => crate::forwarding::Route::Local,
        };
        let response = match (route, kind) {
            (crate::forwarding::Route::Local, PacketKind::Auth) => self.process_auth(state, client, &packet, source.ip()).await,
            (crate::forwarding::Route::Local, PacketKind::Acct) => self.process_acct(state, client, &packet, data, source.ip()).await,
            // Where it goes cannot be told now: the NAS sends it again, rather than it being
            // handled here or by `*` when it belongs elsewhere.
            (crate::forwarding::Route::Discard(reason), _) => {
                if let Some(line) = self.unknown_realm_lines.lock().unwrap_or_else(PoisonError::into_inner).line(Instant::now(), || format!("RADIUS request from client {} discarded: {reason}", client.name)) {
                    tracing::warn!("{line}");
                }
                return None;
            }
            (crate::forwarding::Route::Answered { response, account, realm }, _) => {
                record_forwarded(client, source.ip(), &packet, &response, &account, &realm);
                Some(response)
            }
            (crate::forwarding::Route::Replied(response), _) => Some(response),
            (crate::forwarding::Route::Unanswered, _) => None,
        };
        match response.as_ref().and_then(|response| response.first()) {
            Some(&ACCESS_ACCEPT) => self.metrics.radius_access_accept.inc(),
            Some(&ACCESS_REJECT) => self.metrics.radius_access_reject.inc(),
            Some(&ACCESS_CHALLENGE) => self.metrics.radius_access_challenge.inc(),
            Some(&ACCOUNTING_RESPONSE) => self.metrics.radius_accounting_response.inc(),
            _ => {}
        }
        if let (Some(code), Some(counters)) = (response.as_ref().and_then(|response| response.first()), self.metrics.radius_client(&client.name)) {
            counters.count(*code);
        }
        if state.trace.as_ref().is_some_and(|trace| trace.covers(&client.name, packet.attribute(1).unwrap_or_default())) {
            trace_exchange(&self.dictionary, client, source.ip(), &packet, response.as_deref());
        }
        // Without a response the ticket is dropped, so a retransmission runs again.
        if let (Some(ticket), Some(response)) = (ticket, &response) {
            ticket.finish(response);
        }
        response
    }

    /// RFC 5997: answers without running anything, and without the duplicate cache, as a
    /// recent answer is no reason to discard the next check. A Message-Authenticator is
    /// required whatever the client's settings, since nothing else authenticates these.
    fn answer_status_server(&self, clients: &ClientTable, kind: PacketKind, data: &[u8], source: IpAddr) -> Option<Vec<u8>> {
        if !self.status_server {
            return None;
        }
        let client = self.client_for(clients, source)?;
        let packet = RadiusPacket::parse(data).ok().filter(|packet| packet.verify_message_authenticator(&client.secret) == Some(true));
        let Some(packet) = packet else {
            self.metrics.radius_discarded_invalid.inc();
            return None;
        };
        // Its NAS is better off asking a node that can reach the others.
        if kind == PacketKind::Auth && crate::lua_engine::stopped_without_leader() {
            self.metrics.radius_discarded_no_leader.inc();
            return None;
        }
        self.metrics.radius_status_server.inc();
        Some(match kind {
            PacketKind::Auth => RadiusPacket::signed_response(ACCESS_ACCEPT, &packet, Vec::new(), &client.secret, true),
            PacketKind::Acct => RadiusPacket::signed_response(ACCOUNTING_RESPONSE, &packet, Vec::new(), &client.secret, false),
        })
    }

    /// Returns the Access-Accept/Access-Reject to send, or `None` to silently discard.
    pub async fn handle_auth(&self, data: &[u8], source: IpAddr) -> Option<Vec<u8>> {
        let state = self.state.get();
        let (client, packet) = self.check_auth(&state.clients, data, source)?;
        self.process_auth(&state, client, &packet, source).await
    }

    /// The client and packet of a valid Access-Request; anything else gets `None`.
    fn check_auth<'a>(&self, clients: &'a ClientTable, data: &[u8], source: IpAddr) -> Option<(&'a Client, RadiusPacket)> {
        let client = self.client_for(clients, source)?;
        let packet = RadiusPacket::parse(data).ok().filter(|packet| packet.code == ACCESS_REQUEST && packet.verify_message_authenticator(&client.secret) != Some(false));
        let Some(packet) = packet else {
            self.metrics.radius_discarded_invalid.inc();
            return None;
        };
        if !self.message_authenticator_policy_allows(client, source, &packet) {
            self.metrics.radius_discarded_invalid.inc();
            let count = self.policy_discards.lock().unwrap_or_else(PoisonError::into_inner).record(Instant::now());
            if let Some(count) = count {
                tracing::warn!("RADIUS: discarded {count} Access-Requests lacking a Message-Authenticator, the last from {source}; see require_message_authenticator and limit_proxy_state");
            }
            return None;
        }
        Some((client, packet))
    }

    /// Applies the client's settings to a request whose Message-Authenticator, if any,
    /// is valid. A setting of `auto` is decided here, by the first such request.
    fn message_authenticator_policy_allows(&self, client: &Client, source: IpAddr, packet: &RadiusPacket) -> bool {
        let has_authenticator = packet.attribute(MESSAGE_AUTHENTICATOR).is_some();
        let has_proxy_state = packet.attribute(PROXY_STATE).is_some();
        let mut learned = Learned::default();
        if client.require_message_authenticator == MessageAuthenticatorPolicy::Auto || client.limit_proxy_state == MessageAuthenticatorPolicy::Auto {
            let mut all = self.learned.lock().unwrap_or_else(PoisonError::into_inner);
            let entry = all.entry(source).or_default();
            entry.require.get_or_insert(has_authenticator);
            entry.limit.get_or_insert(has_authenticator || !has_proxy_state);
            learned = *entry;
        }
        let setting = |policy, learned: Option<bool>| match policy {
            MessageAuthenticatorPolicy::Yes => true,
            MessageAuthenticatorPolicy::No => false,
            MessageAuthenticatorPolicy::Auto => learned.unwrap_or(true),
        };
        if has_authenticator {
            return true;
        }
        let required = setting(client.require_message_authenticator, learned.require);
        let limited = setting(client.limit_proxy_state, learned.limit);
        !required && !(limited && has_proxy_state)
    }

    async fn process_auth(&self, state: &RadiusState, client: &Client, packet: &RadiusPacket, source: IpAddr) -> Option<Vec<u8>> {
        if crate::lua_engine::stopped_without_leader() {
            self.metrics.radius_discarded_no_leader.inc();
            return None;
        }
        if packet.attribute(EAP_MESSAGE).is_some() {
            let settings = self.eap.settings();
            if settings.is_some() || packet.attribute(STATE).is_some_and(|state| self.eap.has_conversation(state)) {
                return self.process_eap(state, client, packet, source, settings).await;
            }
        }
        let secret = client.secret.as_slice();
        let attributes = request_attributes(packet, &self.dictionary, secret);
        if let Some(authenticator) = &state.pre_chain {
            let user = String::from_utf8_lossy(packet.attribute(1).unwrap_or_default()).into_owned();
            let request = crate::pre_chain::PreChainRequest { client, attributes: &attributes, user: &user };
            let taken = authenticator.take(&request);
            if let Some(identified) = taken {
                let identified = identified.await;
                return self.process_pre_chain(state, client, packet, source, authenticator.name(), &user, identified).await;
            }
        }

        let mut request = LuaRequest::new("radius", "authentication", attributes);
        request.client = client.name.clone();
        request.radsec_subject = client.radsec_subject.clone();
        let dictionary = self.dictionary.clone();
        request.validator = Some(Arc::new(move |name: &str, value: &AttrValue| encode_response_attributes(&dictionary, [(name, value)], b"", &[0; 16], &mut Vec::new()).map(|_| ())));
        let chain = state.chains.as_deref().and_then(|chains| chains.for_client(client));
        let credentials = chain.map(|_| request_credentials(packet, secret));
        let user = String::from_utf8_lossy(packet.attribute(1).unwrap_or_default());
        let (decision, mut response_attributes, chain_name, handler, account, unavailable) = match (chain, &credentials) {
            (Some(chain), Some(credentials)) => {
                let result = chain.authenticate(credentials, &request).await;
                (result.decision, result.attributes, client.auth.as_deref().unwrap_or("default"), result.handler, result.account, result.unavailable)
            }
            _ => {
                let decision = state.engine.authenticate(request).await;
                let unavailable = decision.database_failed && decision.outcome != Outcome::Accept;
                (decision.outcome, decision.response_attributes, "script", "script", Some(user.to_string()), unavailable)
            }
        };
        // Not a refusal: the NAS sends it again, or asks a server that can reach the database.
        if unavailable {
            self.metrics.radius_discarded_database.inc();
            log_line(tracing::Level::WARN, format!("RADIUS Access-Request from client {} not answered: the cluster database could not be used", client.name));
            return None;
        }
        // What the client gets, which is a rejection if the decision cannot be sent.
        let record = |code: u8| {
            crate::logging::auth_record(&crate::logging::AuthRecord {
                protocol: "radius",
                client: &client.name,
                source,
                user: &user,
                account: account.as_deref().unwrap_or(""),
                chain: chain_name,
                handler,
                decision: match code {
                    ACCESS_ACCEPT => "accept",
                    ACCESS_CHALLENGE => "challenge",
                    _ => "reject",
                },
            });
        };
        let response_code = match decision {
            Outcome::Accept => ACCESS_ACCEPT,
            Outcome::Challenge => match challenge_attribute_problem(&self.dictionary, &response_attributes) {
                None => ACCESS_CHALLENGE,
                Some(problem) => {
                    log_line(tracing::Level::ERROR, format!("RADIUS Access-Challenge for client {} not sent: {problem}", client.name));
                    ACCESS_REJECT
                }
            },
            Outcome::Reject | Outcome::Ignore => ACCESS_REJECT,
        };

        let (code, attributes) = if response_code != ACCESS_REJECT {
            let mut salts = Vec::new();
            let encoded = self.encode_reply(&mut response_attributes, secret, &packet.authenticator, &mut salts);
            match encoded {
                Ok(attributes) => (response_code, attributes),
                Err(e) => {
                    tracing::error!("RADIUS response attribute error: {}", e);
                    (ACCESS_REJECT, vec![])
                }
            }
        } else {
            (ACCESS_REJECT, vec![])
        };
        let attributes = if code == ACCESS_REJECT { mschap_error(&self.dictionary, credentials.as_ref()) } else { attributes };
        let response = RadiusPacket::create_response(code, packet, attributes, secret);
        if response.len() <= MAX_PACKET_LEN {
            record(code);
            return Some(response);
        }
        tracing::error!("RADIUS response to {} is longer than {MAX_PACKET_LEN} octets", client.name);
        record(ACCESS_REJECT);
        let rejection = RadiusPacket::create_response(ACCESS_REJECT, packet, Vec::new(), secret);
        (rejection.len() <= MAX_PACKET_LEN).then_some(rejection)
    }

    /// An Access-Request a pre-chain authenticator took, such as one carrying the 4-way handshake
    /// of a station on a DPSK SSID: the identity it proved goes to the chain, and an accept adds
    /// what the authenticator gives, such as the PMK the access point finishes the handshake with.
    #[allow(clippy::too_many_arguments)]
    async fn process_pre_chain(&self, state: &RadiusState, client: &Client, packet: &RadiusPacket, source: IpAddr, name: &'static str, user: &str, identified: crate::pre_chain::Identified) -> Option<Vec<u8>> {
        let secret = client.secret.as_slice();
        let respond = |code: u8, account: &str, attributes: Vec<(u8, Vec<u8>)>| {
            let response = RadiusPacket::create_response(code, packet, attributes, secret);
            let (code, response) = if response.len() <= MAX_PACKET_LEN {
                (code, Some(response))
            } else {
                tracing::error!("RADIUS response to {} is longer than {MAX_PACKET_LEN} octets", client.name);
                // The request's Proxy-State goes back in a rejection too, and can make it too long.
                let rejection = RadiusPacket::create_response(ACCESS_REJECT, packet, Vec::new(), secret);
                (ACCESS_REJECT, (rejection.len() <= MAX_PACKET_LEN).then_some(rejection))
            };
            crate::logging::auth_record(&crate::logging::AuthRecord {
                protocol: "radius",
                client: &client.name,
                source,
                user,
                account,
                chain: client.auth.as_deref().unwrap_or("default"),
                handler: name,
                decision: if code == ACCESS_ACCEPT { "accept" } else { "reject" },
            });
            response
        };
        let (identity, mut reply) = match identified {
            crate::pre_chain::Identified::Found { identity, reply } => (identity, reply),
            crate::pre_chain::Identified::Refused { log } => {
                if let Some(log) = log {
                    log_line(tracing::Level::WARN, log);
                }
                return respond(ACCESS_REJECT, "", Vec::new());
            }
            // The access point asks again, as when busy.
            crate::pre_chain::Identified::Unavailable { log } => {
                self.metrics.radius_discarded_database.inc();
                log_line(tracing::Level::WARN, log);
                return None;
            }
            // The access point asks again.
            crate::pre_chain::Identified::Busy => return None,
        };
        let (decision, mut attributes, account, unavailable) = self.authorize_certificate(state, client, packet, &identity, name, None).await;
        if unavailable {
            crate::auth_chain::wipe_keys(&mut reply);
            self.metrics.radius_discarded_database.inc();
            log_line(tracing::Level::WARN, format!("RADIUS {} request from client {} not answered: the cluster database could not be used", name.to_uppercase(), client.name));
            return None;
        }
        let account = account.unwrap_or_else(|| identity.clone());
        if decision != Outcome::Accept {
            crate::auth_chain::wipe_keys(&mut attributes);
            crate::auth_chain::wipe_keys(&mut reply);
            return respond(ACCESS_REJECT, &account, Vec::new());
        }
        attributes.append(&mut reply);
        let mut salts = Vec::new();
        match self.encode_reply(&mut attributes, secret, &packet.authenticator, &mut salts) {
            Ok(encoded) => respond(ACCESS_ACCEPT, &account, encoded),
            Err(e) => {
                tracing::error!("RADIUS response attribute error: {e}");
                respond(ACCESS_REJECT, &account, Vec::new())
            }
        }
    }

    /// The attributes of a reply as they go out, overwriting the session keys among them.
    fn encode_reply(&self, attributes: &mut [(String, AttrValue)], secret: &[u8], authenticator: &[u8; 16], salts: &mut Vec<[u8; 2]>) -> Result<Vec<(u8, Vec<u8>)>, String> {
        let encoded = encode_response_attributes(&self.dictionary, attributes.iter().map(|(name, value)| (name.as_str(), value)), secret, authenticator, salts);
        crate::auth_chain::wipe_keys(attributes);
        encoded
    }

    /// One round of an EAP conversation. The script or chain only hears of it at the end.
    async fn process_eap(&self, state: &RadiusState, client: &Client, packet: &RadiusPacket, source: IpAddr, settings: Option<Arc<EapSettings>>) -> Option<Vec<u8>> {
        // RFC 3579 section 3.2: whatever the client's settings say. A wrong one is already gone.
        if packet.attribute(MESSAGE_AUTHENTICATOR).is_none() {
            self.metrics.radius_discarded_invalid.inc();
            return None;
        }
        let message = join_message(packet.attributes.iter().filter(|(kind, _)| *kind == EAP_MESSAGE).map(|(_, value)| value.as_slice()));
        let owner = Owner { client: client.name.clone(), calling_station: packet.attribute(CALLING_STATION_ID).map(<[u8]>::to_vec) };
        // What resuming needs at hand, such as the cluster's ticket keys, ahead of need.
        if let Some(resumption) = settings.as_ref().and_then(|settings| settings.resumption.as_ref()) {
            resumption.prepare(&state.engine.db).await;
        }
        let mut step = self.eap.handle(settings, owner, client.eap_methods.as_deref(), &message, packet.attribute(STATE), &self.metrics).await;
        let secret = client.secret.as_slice();
        let user = String::from_utf8_lossy(packet.attribute(1).unwrap_or_default()).into_owned();
        // Not answered, and the conversation waits: the NAS sends the request again.
        let unavailable = |reason: &str| {
            self.metrics.radius_discarded_database.inc();
            log_line(tracing::Level::WARN, format!("EAP request from client {} not answered: {reason}", client.name));
        };
        if let Step::Lookup(pending) = step {
            let method = pending.method.clone();
            let context = crate::eap_method::MethodContext { db: &state.engine.db, metrics: &self.metrics };
            step = match method.lookup(&context, &pending.request).await {
                Err(crate::eap_method::LookupError::Unavailable(reason)) => {
                    unavailable(&reason);
                    self.eap.lookup_waits(*pending)
                }
                found => self.eap.looked_up(*pending, found, &self.metrics),
            };
        }
        if let Step::Authorize(pending) = step {
            // The card took this number when it answered: stored first, whatever the chain says.
            let stored = self.before_authorize(state, client, &pending).await;
            let (result, refusal, waits) = match stored {
                Ok(()) => self.authorize_inner(state, client, packet, &pending).await,
                Err(not_ready) => {
                    log_line(tracing::Level::WARN, not_ready.log);
                    (InnerResult::Refused { account: pending.name.clone() }, Some(not_ready.refusal), not_ready.unavailable)
                }
            };
            if waits {
                unavailable("the cluster database could not be used");
                self.eap.authorization_waits(*pending);
                return None;
            }
            // Now, and not when the conversation ends: it need not end, for those guessing passwords.
            if let (InnerResult::Refused { account }, Some(reason)) = (&result, refusal) {
                log_line(tracing::Level::WARN, format!("EAP conversation with client {} failed: {reason}", client.name));
                // Not again, for one counted when it was dropped for room or timed out meanwhile.
                self.eap.count_refusal(&pending.state, pending.method, &self.metrics);
                eap_record(client, source, &user, account, handler_name(pending.method), ACCESS_REJECT);
            }
            step = self.eap.authorized(*pending, result, &self.metrics);
        }
        match step {
            Step::Discard | Step::Authorize(_) | Step::Lookup(_) => None,
            Step::Closed { packet: eap } => eap_rejection(packet, &eap, secret),
            Step::Challenge { packet: eap, state: token } => {
                let mut attributes = split_message(&eap);
                attributes.extend(token.map(|token| (STATE, token.to_vec())));
                let response = RadiusPacket::create_response(ACCESS_CHALLENGE, packet, attributes, secret);
                if response.len() <= MAX_PACKET_LEN {
                    return Some(response);
                }
                // Whatever fills the request will be there each time, and the peer would only time out.
                log_line(tracing::Level::WARN, format!("EAP conversation with client {} failed: the next request is longer than {MAX_PACKET_LEN} octets", client.name));
                if let Some(token) = token {
                    if let Some(ended) = self.eap.abandon(&token, &self.metrics).await {
                        eap_record(client, source, &user, &ended.account, handler_name(ended.method), ACCESS_REJECT);
                    }
                }
                // It answers the Response that came, whatever the Request that did not fit would have said.
                eap_rejection(packet, &EapPacket::failure(message.get(1).copied().unwrap_or(0)).encode(), secret)
            }
            Step::Failed { packet: eap, reason, ended } => {
                // An overload refuses many at once: a line a second, with how many.
                let text = || format!("EAP conversation with client {} failed: {reason}", client.name);
                let line = match ended.as_ref().is_some_and(|ended| ended.overload) {
                    true => {
                        let lines = if ended.as_ref().is_some_and(|ended| ended.over_budget) { &self.eap_vector_lines } else { &self.eap_overload_lines };
                        lines.lock().unwrap_or_else(PoisonError::into_inner).line(Instant::now(), text)
                    }
                    false => Some(text()),
                };
                if let Some(line) = line {
                    log_line(tracing::Level::WARN, line);
                }
                // Anyone can send a State no conversation has, and the conversation it names may go on.
                if let Some(ended) = ended {
                    eap_record(client, source, &user, &ended.account, handler_name(ended.method), ACCESS_REJECT);
                }
                eap_rejection(packet, &eap, secret)
            }
            Step::Finished(mut finished) => {
                let (id, method) = (finished.id, finished.method);
                let counters = self.metrics.eap(method);
                let refusal = |reason: String, account: &str, counted: bool| {
                    log_line(tracing::Level::WARN, format!("EAP conversation with client {} refused: {reason}", client.name));
                    if !counted {
                        counters.reject.inc();
                    }
                    eap_record(client, source, &user, account, handler_name(method), ACCESS_REJECT);
                    eap_rejection(packet, &EapPacket::failure(id).encode(), secret)
                };
                let decided = match finished.inner.take() {
                    Some(inner) => Ok((inner.account.clone(), inner.attributes, inner.account)),
                    None => self.decide_on_certificate(state, client, packet, &finished).await,
                };
                // The chain could not decide for want of the database: the conversation waits.
                if let Err((reason, _, true)) = &decided {
                    unavailable(reason);
                    self.eap.finish_waits(finished);
                    return None;
                }
                self.eap.settled(&mut finished, &self.metrics);
                let counted = finished.counted;
                let (name, mut attributes, account) = match decided {
                    Ok(decided) => decided,
                    Err((reason, account, _)) => return refusal(reason, &account, counted),
                };
                // Or the device goes on with the name the peer chose, in its logs and in accounting.
                if !attributes.iter().any(|(attribute, _)| attribute == "User-Name") {
                    attributes.push(("User-Name".to_string(), AttrValue::Bytes(name.as_bytes().to_vec())));
                }
                let mut salts = Vec::new();
                let keys = [("MS-MPPE-Recv-Key", &finished.msk[..32]), ("MS-MPPE-Send-Key", &finished.msk[32..])];
                let mut attributes: Vec<(String, AttrValue)> = keys.iter().map(|(key, value)| (key.to_string(), AttrValue::Bytes(value.to_vec()))).chain(attributes).collect();
                let encoded = self.encode_reply(&mut attributes, secret, &packet.authenticator, &mut salts);
                let encoded = match encoded {
                    Ok(encoded) => encoded,
                    Err(e) => return refusal(format!("a reply attribute cannot be sent: {e}"), &account, counted),
                };
                let mut reply = split_message(&EapPacket::success(id).encode());
                reply.extend(encoded);
                let response = RadiusPacket::create_response(ACCESS_ACCEPT, packet, reply, secret);
                if response.len() > MAX_PACKET_LEN {
                    return refusal(format!("the response is longer than {MAX_PACKET_LEN} octets"), &account, counted);
                }
                if !counted {
                    counters.accept.inc();
                }
                eap_record(client, source, &user, &account, handler_name(method), ACCESS_ACCEPT);
                Some(response)
            }
        }
    }

    /// What the chain says of the name in a verified certificate: the name, the reply attributes
    /// and the account, or why not and the account.
    /// The refusal says whether it was for want of the cluster database.
    async fn decide_on_certificate(&self, state: &RadiusState, client: &Client, packet: &RadiusPacket, finished: &Finished) -> Result<(String, Vec<(String, AttrValue)>, String), (String, String, bool)> {
        let certificate = finished.certificate.as_ref().ok_or_else(|| ("no certificate".to_string(), String::new(), false))?;
        let (name, subject) = certificate_name(certificate, &finished.identity, finished.match_identity).map_err(|reason| (reason, String::new(), false))?;
        // After the handshake, where the responder can be waited for; resumed sessions too.
        if let Some(revocation) = &finished.revocation {
            revocation.check(&finished.chain).await.map_err(|reason| (reason, name.clone(), false))?;
        }
        let (decision, attributes, account, unavailable) = self.authorize_certificate(state, client, packet, &name, "tls", Some(subject)).await;
        let account = account.unwrap_or_else(|| name.clone());
        if decision != Outcome::Accept {
            return Err(("not authorized".to_string(), account, unavailable));
        }
        Ok((name, attributes, account))
    }

    /// What the method of an extension does before the chain is asked about a peer that proved
    /// itself: the method of the running settings, as a reload may have changed what it does,
    /// and nothing when they no longer have it. The log line of an error names the client.
    async fn before_authorize(&self, state: &RadiusState, client: &Client, pending: &PendingAuth) -> Result<(), crate::eap_method::NotReady> {
        let Some(proof) = &pending.proof else {
            return Ok(());
        };
        let settings = self.eap.settings();
        let Some(method) = settings.as_ref().and_then(|settings| crate::eap_method::find(&settings.extra, pending.method)) else {
            return Ok(());
        };
        let context = crate::eap_method::MethodContext { db: &state.engine.db, metrics: &self.metrics };
        method.before_authorize(&context, &pending.name, proof).await.map_err(|not_ready| crate::eap_method::NotReady { log: format!("{} for client {}: {}", method.names().title, client.name, not_ready.log), ..not_ready })
    }

    /// Asks the client's chain about a password from inside a tunnel. Scripts see the name from
    /// inside as `User-Name`. For MS-CHAPv2, only a chain's `MS-CHAP2-Success` means a password was
    /// checked, so an accept without one is a refusal: a script that accepts everything does not
    /// open the network to every password. The keys that come with it stay here: the tunnel has its own.
    async fn authorize_inner(&self, state: &RadiusState, client: &Client, packet: &RadiusPacket, pending: &PendingAuth) -> (InnerResult, Option<&'static str>, bool) {
        // EAP-AKA': the card has proved it holds the key, as a certificate proves a name.
        if pending.secret == Secret::Certificate {
            let (decision, attributes, account, unavailable) = self.authorize_certificate(state, client, packet, &pending.name, crate::eap::method_name(pending.method), None).await;
            let account = account.unwrap_or_else(|| pending.name.clone());
            return match decision {
                Outcome::Accept => (InnerResult::Proved { success: None, account, attributes }, None, false),
                _ => (InnerResult::Refused { account }, Some("the chain did not accept the subscriber"), unavailable),
            };
        }
        let refused = |account: Option<String>, reason: &'static str| (InnerResult::Refused { account: account.unwrap_or_else(|| pending.name.clone()) }, Some(reason), false);
        let Some(chain) = state.chains.as_deref().and_then(|chains| chains.for_client(client)) else {
            return refused(None, "this client has no chain to check a password with");
        };
        let mut attributes = request_attributes(packet, &self.dictionary, &client.secret);
        attributes.retain(|(attribute, _)| attribute != "User-Name");
        attributes.push(("User-Name".to_string(), AttrValue::Bytes(pending.name.as_bytes().to_vec())));
        // As in any PAP request: a script is free to compare it, and must not log it.
        if let Secret::Pap(password) = &pending.secret {
            attributes.retain(|(attribute, _)| attribute != "User-Password");
            attributes.push(("User-Password".to_string(), AttrValue::Bytes(password.to_vec())));
        }
        let mut request = LuaRequest::new("radius", "authentication", attributes);
        request.client = client.name.clone();
        request.radsec_subject = client.radsec_subject.clone();
        request.eap_method = Some(if pending.method == TYPE_TTLS { "ttls" } else { "peap" });
        let dictionary = self.dictionary.clone();
        request.validator = Some(Arc::new(move |name: &str, value: &AttrValue| encode_response_attributes(&dictionary, [(name, value)], b"", &[0; 16], &mut Vec::new()).map(|_| ())));
        let credentials = Credentials { user: Some(pending.name.clone()), secret: pending.secret.clone() };
        let mut result = chain.authenticate(&credentials, &request).await;
        // The keys of a device come from the tunnel: these are not sent, whatever happens next.
        crate::auth_chain::wipe_keys(&mut result.attributes);
        if result.decision != Outcome::Accept {
            let (refusal, reason, _) = refused(result.account, "the chain did not accept the inner name");
            return (refusal, reason, result.unavailable);
        }
        let success = result.attributes.iter().find_map(|(attribute, value)| match (attribute.as_str(), value) {
            ("MS-CHAP2-Success", AttrValue::Bytes(value)) => value.get(1..).map(<[u8]>::to_vec),
            _ => None,
        });
        // MS-CHAPv2 proves a comparison by its result. With PAP nothing shows one, so it takes a
        // handler that compares, and a script that only checks an account is active is not one.
        let compared = match pending.secret {
            Secret::MsChapV2 { .. } => success.is_some(),
            _ => result.verified,
        };
        if !compared {
            return refused(result.account, "the chain accepted the inner name without checking a password");
        }
        let account = result.account.unwrap_or_else(|| pending.name.clone());
        let attributes = result.attributes.into_iter().filter(|(attribute, _)| !matches!(attribute.as_str(), "MS-CHAP2-Success" | "MS-MPPE-Send-Key" | "MS-MPPE-Recv-Key")).collect();
        (InnerResult::Proved { success, account, attributes }, None, false)
    }

    /// Asks the client's chain, or the top-level script without one, about a name that a verified
    /// certificate proved. Scripts see that name as `User-Name`.
    /// The last value says a refusal came of the cluster database being unavailable.
    async fn authorize_certificate(&self, state: &RadiusState, client: &Client, packet: &RadiusPacket, name: &str, method: &'static str, subject: Option<CertificateSubject>) -> (Outcome, Vec<(String, AttrValue)>, Option<String>, bool) {
        let mut attributes = request_attributes(packet, &self.dictionary, &client.secret);
        attributes.retain(|(attribute, _)| attribute != "User-Name");
        attributes.push(("User-Name".to_string(), AttrValue::Bytes(name.as_bytes().to_vec())));
        let mut request = LuaRequest::new("radius", "authentication", attributes);
        request.client = client.name.clone();
        request.radsec_subject = client.radsec_subject.clone();
        request.eap_method = Some(method);
        if let Some(subject) = subject {
            request.tls_subject = Some(subject.text);
            request.tls_subject_attrs = Some(subject.attrs);
        }
        let dictionary = self.dictionary.clone();
        request.validator = Some(Arc::new(move |name: &str, value: &AttrValue| encode_response_attributes(&dictionary, [(name, value)], b"", &[0; 16], &mut Vec::new()).map(|_| ())));
        match state.chains.as_deref().and_then(|chains| chains.for_client(client)) {
            Some(chain) => {
                let credentials = Credentials { user: Some(name.to_string()), secret: Secret::Certificate };
                let result = chain.authenticate(&credentials, &request).await;
                (result.decision, result.attributes, result.account, result.unavailable)
            }
            None => {
                let decision = state.engine.authenticate(request).await;
                let unavailable = decision.database_failed && decision.outcome != Outcome::Accept;
                (decision.outcome, decision.response_attributes, None, unavailable)
            }
        }
    }

    /// Runs the script with phase `accounting` and returns the Accounting-Response
    /// to send, or `None` to silently discard. There is no response unless the script
    /// accepted: RFC 2866 section 2 forbids acknowledging what was not recorded, and
    /// silence is what makes the NAS retransmit.
    pub async fn handle_acct(&self, data: &[u8], source: IpAddr) -> Option<Vec<u8>> {
        let state = self.state.get();
        let (client, packet) = self.check_acct(&state.clients, data, source)?;
        self.process_acct(&state, client, &packet, data, source).await
    }

    /// The client and packet of a valid Accounting-Request; anything else gets `None`.
    fn check_acct<'a>(&self, clients: &'a ClientTable, data: &[u8], source: IpAddr) -> Option<(&'a Client, RadiusPacket)> {
        let client = self.client_for(clients, source)?;
        let packet = RadiusPacket::parse(data).ok().filter(|packet| packet.code == ACCOUNTING_REQUEST && packet.verify_accounting_request(&client.secret));
        let Some(packet) = packet else {
            self.metrics.radius_discarded_invalid.inc();
            return None;
        };
        Some((client, packet))
    }

    async fn process_acct(&self, state: &RadiusState, client: &Client, packet: &RadiusPacket, data: &[u8], source: IpAddr) -> Option<Vec<u8>> {
        let secret = client.secret.as_slice();
        let mut request = LuaRequest::new("radius", "accounting", request_attributes(packet, &self.dictionary, secret));
        request.client = client.name.clone();
        request.radsec_subject = client.radsec_subject.clone();
        let decision = state.engine.authenticate(request).await;
        // A failure that says the database could not be used, caught by the script or not, may
        // have kept the record from being written: on disk first, whatever the script then said.
        // Other failures of uncertain cause are not spooled here, while a replay tries them again.
        let acknowledged = match decision.database_failed {
            true => self.spooled(client, data, source).await || (decision.accepted() && self.spool.is_none()),
            false => decision.accepted(),
        };
        acknowledged.then(|| RadiusPacket::create_response(ACCOUNTING_RESPONSE, packet, vec![], secret))
    }

    /// Whether the request is now on disk, to replay when the database works again. Without
    /// that the NAS gets no answer, and sends it again as long as it is willing to.
    async fn spooled(&self, client: &Client, data: &[u8], source: IpAddr) -> bool {
        match &self.spool {
            Some(spool) => spool_request(spool, "accounting spool", client, data, source, now_unix() as u64).await,
            None => false,
        }
    }

    /// Replays the spool for as long as the server runs, up to `window` requests at a time,
    /// settled in the order written (see `replay_spool_with`); with 1, the default, strictly in
    /// order. Whatever may go differently next time keeps a request at the front, to try again
    /// after `interval`: nothing sends an answered request again, so giving up on one loses it.
    pub async fn replay_spool(self: Arc<Self>, interval: Duration, window: usize) {
        let Some(spool) = self.spool.clone() else { return };
        let counters = self.metrics.acct_spool.clone();
        let server = self.clone();
        self.replay_spool_with(spool, interval, window, "accounting spool", &counters, move |spooled| {
            let server = server.clone();
            Box::pin(async move { server.replay_one(&spooled).await })
        })
        .await;
    }

    /// The loop of every spool, this server's and those of extensions: each request goes through
    /// `replay_one`, the script for this server's own. Up to `window` requests go at once; their
    /// results are settled in the order written, up to the first that no server answered, which
    /// starts the next window. Until that one has a result it goes alone, so that the requests
    /// after it, which other servers may have answered already, are not sent again on every
    /// retry. `label` names the spool in the log.
    pub async fn replay_spool_with<F>(&self, spool: Arc<Spool>, interval: Duration, window: usize, label: &str, counters: &SpoolCounters, replay_one: F)
    where
        F: Fn(SpooledRequest) -> BoxFuture<'static, Replayed>,
    {
        let mut waiting_on_first = false;
        loop {
            let size = if waiting_on_first { 1 } else { window.max(1) };
            let batch = match spool.next_batch(size).await {
                Ok(batch) if !batch.is_empty() => batch,
                Ok(_) => {
                    tokio::time::sleep(interval).await;
                    continue;
                }
                Err(e) => {
                    log_line(tracing::Level::ERROR, format!("{label}: cannot read: {e}"));
                    tokio::time::sleep(interval).await;
                    continue;
                }
            };
            let results = futures::future::join_all(batch.iter().map(|(spooled, _)| replay_one(spooled.clone()))).await;
            let done = settled(&results);
            waiting_on_first = done < results.len();
            let mut stopped = waiting_on_first;
            // Answered requests are marked done together, which saves the position once.
            let mut answered = Vec::with_capacity(done);
            for ((spooled, len), result) in batch.into_iter().zip(results).take(done) {
                let Replayed::Refused(reason) = result else {
                    answered.push(len);
                    continue;
                };
                let finished = match spool.advance_many(&answered).await {
                    Ok(()) => {
                        counters.replayed.add(answered.len() as u64);
                        answered.clear();
                        log_line(tracing::Level::ERROR, format!("{label}: request from client {} moved to rejected.log: {reason}", spooled.client));
                        spool.reject(spooled, len).await
                    }
                    Err(e) => Err(e),
                };
                if let Err(e) = finished {
                    log_line(tracing::Level::ERROR, format!("{label}: cannot record progress: {e}"));
                    answered.clear();
                    stopped = true;
                    break;
                }
            }
            match spool.advance_many(&answered).await {
                Ok(()) => counters.replayed.add(answered.len() as u64),
                Err(e) => {
                    log_line(tracing::Level::ERROR, format!("{label}: cannot record progress: {e}"));
                    stopped = true;
                }
            }
            if stopped {
                tokio::time::sleep(interval).await;
            }
        }
    }

    async fn replay_one(&self, spooled: &SpooledRequest) -> Replayed {
        let state = self.state.get();
        let client = match self.spooled_client(&state, spooled).await {
            Ok(client) => client,
            Err(replayed) => return replayed,
        };
        let Ok(packet) = RadiusPacket::parse(&spooled.packet) else {
            return Replayed::Refused("not a RADIUS packet");
        };
        let mut attributes = request_attributes(&packet, &self.dictionary, &client.secret);
        // RFC 2866 section 5.2: how long this has been on its way, for scripts working out times.
        let waited = (now_unix() as u64).saturating_sub(spooled.received) as f64;
        match attributes.iter_mut().find(|(name, _)| name == "Acct-Delay-Time") {
            Some((_, AttrValue::Number(delay))) => *delay += waited,
            _ => attributes.push(("Acct-Delay-Time".to_string(), AttrValue::Number(waited))),
        }
        let mut request = LuaRequest::new("radius", "accounting", attributes);
        request.client = client.name.clone();
        request.replayed = true;
        let decision = state.engine.authenticate(request).await;
        if decision.accepted() && decision.final_result {
            Replayed::Accepted
        } else if decision.accepted() {
            // The script caught a failure and accepted all the same, and the NAS will not send this again.
            Replayed::TryAgain
        } else if decision.final_result {
            Replayed::Refused("the script did not accept it")
        } else {
            Replayed::TryAgain
        }
    }
}

#[cfg(test)]
pub(crate) use crate::test_support::radius::share_test_dictionary;

/// The shipped example scripts, through real packets.
#[cfg(test)]
mod example_tests {
    use super::*;
    use crate::lua_engine::now_unix;
    use crate::test_support::{logged_errors, test_db, test_vms};

    const SECRET: &[u8] = b"s";

    /// A server running `script`, after running `schema` on the database shared by the
    /// tests, which is why each test uses a table of its own.
    async fn server(script: &str, schema: &[&str]) -> RadiusServer {
        let db = test_db();
        for statement in schema {
            db.execute(statement).await.unwrap();
        }
        let engine = LuaAuthEngine::new(test_vms(), db, script).unwrap();
        let clients = ClientTable::new(vec![
            ("127.0.0.1", Some("nas-a".to_string()), SECRET.to_vec(), None),
            ("127.0.0.2", Some("nas-b".to_string()), SECRET.to_vec(), None),
        ])
        .unwrap();
        let any = "127.0.0.1:0".parse().unwrap();
        RadiusServer::new(any, any, Arc::new(clients), Arc::new(engine), share_test_dictionary())
    }

    fn source() -> IpAddr {
        "127.0.0.1".parse().unwrap()
    }

    /// A login from a switch checking a device by its MAC address.
    async fn mac_login(server: &RadiusServer, user: &str, station: Option<&str>) -> RadiusPacket {
        let mut attributes = vec![(1, user.as_bytes().to_vec())];
        attributes.extend(station.map(|station| (31, station.as_bytes().to_vec())));
        let request = RadiusPacket::create_access_request(1, [1; 16], attributes, SECRET);
        RadiusPacket::parse(&server.handle_auth(&request, source()).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn test_mac_auth_example() {
        let schema = [
            "CREATE TABLE IF NOT EXISTS mac_devices (mac TEXT PRIMARY KEY, vlan TEXT NOT NULL)",
            "INSERT OR IGNORE INTO mac_devices VALUES ('001122aabbcc', '100')",
        ];
        let server = server(include_str!("../../rradius-server/examples/mac-auth.lua"), &schema).await;
        // However the switch writes it.
        for (user, station) in [("00-11-22-AA-BB-CC", "00-11-22-AA-BB-CC"), ("001122aabbcc", "00:11:22:aa:bb:cc"), ("0011.22aa.bbcc", "001122AABBCC")] {
            let accepted = mac_login(&server, user, Some(station)).await;
            assert_eq!(accepted.code, ACCESS_ACCEPT, "{user} {station}");
            assert_eq!(accepted.attribute(64), Some(&[1, 0, 0, 13][..]), "Tunnel-Type");
            assert_eq!(accepted.attribute(65), Some(&[1, 0, 0, 6][..]), "Tunnel-Medium-Type");
            assert_eq!(accepted.attribute(81), Some(&b"\x01100"[..]), "Tunnel-Private-Group-Id");
        }
        for (what, user, station) in [
            ("another device", "00-11-22-AA-BB-DD", Some("00-11-22-AA-BB-DD")),
            ("a name that is not the device", "alice", Some("00-11-22-AA-BB-CC")),
            ("a known name from another device", "00-11-22-AA-BB-CC", Some("00-11-22-AA-BB-DD")),
            ("no station", "00-11-22-AA-BB-CC", None),
            ("too short", "00-11-22", Some("00-11-22")),
        ] {
            assert_eq!(mac_login(&server, user, station).await.code, ACCESS_REJECT, "{what}");
        }
        // It does not speak for anything but RADIUS.
        let engine = LuaAuthEngine::new(test_vms(), test_db(), include_str!("../../rradius-server/examples/mac-auth.lua")).unwrap();
        for phase in ["authentication", "authorization", "accounting"] {
            let outcome = engine.authenticate(LuaRequest::new("tacacs", phase, Vec::new())).await.outcome;
            assert_eq!(outcome, Outcome::Reject, "{phase}");
        }
        let accounting = RadiusPacket::create_accounting_request(1, vec![(1, b"x".to_vec())], SECRET);
        assert!(server.handle_acct(&accounting, source()).await.is_some(), "RADIUS accounting goes on");
    }

    /// The code of the response to a login, and the response.
    async fn login(server: &RadiusServer, user: &str) -> RadiusPacket {
        let request = RadiusPacket::create_access_request(1, [1; 16], vec![(1, user.as_bytes().to_vec())], SECRET);
        RadiusPacket::parse(&server.handle_auth(&request, source()).await.unwrap()).unwrap()
    }

    async fn accounting(server: &RadiusServer, attributes: Vec<(u8, Vec<u8>)>) {
        let request = RadiusPacket::create_accounting_request(1, attributes, SECRET);
        assert!(server.handle_acct(&request, source()).await.is_some(), "the accounting packet was not acknowledged");
    }

    fn logged(word: &str) -> bool {
        logged_errors().lock().unwrap().iter().any(|line| line.contains(word))
    }

    /// Accounting from the second NAS.
    async fn accounting_from_other_nas(server: &RadiusServer, attributes: Vec<(u8, Vec<u8>)>) {
        let request = RadiusPacket::create_accounting_request(1, attributes, SECRET);
        assert!(server.handle_acct(&request, "127.0.0.2".parse().unwrap()).await.is_some(), "the accounting packet was not acknowledged");
    }

    fn session(user: &str, session: &str, status: u32, seconds: u32) -> Vec<(u8, Vec<u8>)> {
        vec![(1, user.as_bytes().to_vec()), (44, session.as_bytes().to_vec()), (40, status.to_be_bytes().to_vec()), (46, seconds.to_be_bytes().to_vec())]
    }

    #[tokio::test]
    async fn test_quota_example_limits_time() {
        let schema = [
            "CREATE TABLE IF NOT EXISTS quota_users (username TEXT PRIMARY KEY, quota_seconds INTEGER NOT NULL, from_hour INTEGER NOT NULL DEFAULT 0, to_hour INTEGER NOT NULL DEFAULT 24)",
            "CREATE TABLE IF NOT EXISTS quota_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, seconds INTEGER NOT NULL, PRIMARY KEY (client, session_id))",
            "CREATE INDEX IF NOT EXISTS quota_sessions_user ON quota_sessions (username)",
            "INSERT INTO quota_users (username, quota_seconds) VALUES ('q-time', 100)",
        ];
        let server = server(include_str!("../../rradius-server/examples/quota-and-hours.lua"), &schema).await;

        let first = login(&server, "q-time").await;
        assert_eq!(first.code, ACCESS_ACCEPT, "quota left");
        assert_eq!(first.attribute(27), Some(&100u32.to_be_bytes()[..]), "Session-Timeout is what is left");
        assert_eq!(login(&server, "q-nobody").await.code, ACCESS_REJECT, "no such user");

        accounting(&server, session("q-time", "q-s1", 2, 60)).await;
        accounting(&server, session("q-time", "q-s1", 2, 60)).await; // the same Stop again
        accounting(&server, session("q-time", "q-s0", 1, 500)).await; // a Start adds nothing
        assert_eq!(login(&server, "q-time").await.attribute(27), Some(&40u32.to_be_bytes()[..]), "used 60 once");

        accounting(&server, session("q-time", "q/s2", 2, 40)).await; // a CIDR client's name has one too
        assert_eq!(login(&server, "q-time").await.code, ACCESS_REJECT, "quota used up");
    }

    #[tokio::test]
    async fn test_quota_example_session_id_from_two_nases() {
        let schema = [
            "CREATE TABLE IF NOT EXISTS quota_users (username TEXT PRIMARY KEY, quota_seconds INTEGER NOT NULL, from_hour INTEGER NOT NULL DEFAULT 0, to_hour INTEGER NOT NULL DEFAULT 24)",
            "CREATE TABLE IF NOT EXISTS quota_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, seconds INTEGER NOT NULL, PRIMARY KEY (client, session_id))",
            "CREATE INDEX IF NOT EXISTS quota_sessions_user ON quota_sessions (username)",
            "INSERT INTO quota_users (username, quota_seconds) VALUES ('q-two', 100)",
        ];
        let server = server(include_str!("../../rradius-server/examples/quota-and-hours.lua"), &schema).await;

        accounting(&server, session("q-two", "q-same", 2, 60)).await;
        accounting_from_other_nas(&server, session("q-two", "q-same", 2, 30)).await;
        accounting_from_other_nas(&server, session("q-two", "q-same", 2, 30)).await; // sent again
        assert_eq!(login(&server, "q-two").await.attribute(27), Some(&10u32.to_be_bytes()[..]), "both sessions count, once each");
    }

    #[tokio::test]
    async fn test_quota_example_logs_what_it_cannot_count() {
        let schema = [
            "CREATE TABLE IF NOT EXISTS quota_users (username TEXT PRIMARY KEY, quota_seconds INTEGER NOT NULL, from_hour INTEGER NOT NULL DEFAULT 0, to_hour INTEGER NOT NULL DEFAULT 24)",
            "CREATE TABLE IF NOT EXISTS quota_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, seconds INTEGER NOT NULL, PRIMARY KEY (client, session_id))",
            "CREATE INDEX IF NOT EXISTS quota_sessions_user ON quota_sessions (username)",
        ];
        let server = server(include_str!("../../rradius-server/examples/quota-and-hours.lua"), &schema).await;
        accounting(&server, no_session("q-unusable-stop", 2)).await;
        assert!(logged("q-unusable-stop"), "a Stop that is not counted must be logged");
        accounting(&server, no_session("q-unusable-start", 1)).await;
        assert!(!logged("q-unusable-start"), "only a Stop counts, so nothing was lost");
        accounting(&server, session("q-empty-session", "", 2, 60)).await;
        assert!(logged("q-empty-session"), "an empty session ID is no session ID");
    }

    #[tokio::test]
    async fn test_quota_example_user_with_a_quote() {
        let schema = [
            "CREATE TABLE IF NOT EXISTS quota_users (username TEXT PRIMARY KEY, quota_seconds INTEGER NOT NULL, from_hour INTEGER NOT NULL DEFAULT 0, to_hour INTEGER NOT NULL DEFAULT 24)",
            "CREATE TABLE IF NOT EXISTS quota_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, seconds INTEGER NOT NULL, PRIMARY KEY (client, session_id))",
            "CREATE INDEX IF NOT EXISTS quota_sessions_user ON quota_sessions (username)",
            "INSERT INTO quota_users (username, quota_seconds) VALUES ('q-o''brien', 100)",
        ];
        let server = server(include_str!("../../rradius-server/examples/quota-and-hours.lua"), &schema).await;
        assert_eq!(login(&server, "q-o'brien").await.attribute(27), Some(&100u32.to_be_bytes()[..]));
        accounting(&server, session("q-o'brien", "q-it's a session", 2, 60)).await;
        assert_eq!(login(&server, "q-o'brien").await.attribute(27), Some(&40u32.to_be_bytes()[..]), "the session counted");
    }

    #[tokio::test]
    async fn test_quota_example_limits_hours() {
        let schema = [
            "CREATE TABLE IF NOT EXISTS quota_users (username TEXT PRIMARY KEY, quota_seconds INTEGER NOT NULL, from_hour INTEGER NOT NULL DEFAULT 0, to_hour INTEGER NOT NULL DEFAULT 24)",
            "CREATE TABLE IF NOT EXISTS quota_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, seconds INTEGER NOT NULL, PRIMARY KEY (client, session_id))",
            "CREATE INDEX IF NOT EXISTS quota_sessions_user ON quota_sessions (username)",
        ];
        let server = server(include_str!("../../rradius-server/examples/quota-and-hours.lua"), &schema).await;
        let hour = (now_unix() as i64 / 3600) % 24;
        let add = |user: &str, from: i64, to: i64| format!("INSERT INTO quota_users (username, quota_seconds, from_hour, to_hour) VALUES ('{user}', 100, {from}, {to})");
        let db = test_db();
        // Two hours wide, so that the hour changing during the test does not matter.
        db.execute(&add("h-now", hour, hour + 2)).await.unwrap();
        db.execute(&add("h-later", (hour + 3) % 24, (hour + 5) % 24)).await.unwrap();
        // Across midnight, so from is above to, with this hour and the next inside.
        let (from, to) = if hour >= 2 { (hour, 1) } else { (23, 3) };
        db.execute(&add("h-wrap-in", from, to)).await.unwrap();
        // Across midnight, with this hour and the next in the part left out.
        let (from, to) = if hour <= 21 { (hour + 2, hour) } else { (24, 21) };
        db.execute(&add("h-wrap-out", from, to)).await.unwrap();

        assert_eq!(login(&server, "h-now").await.code, ACCESS_ACCEPT);
        assert_eq!(login(&server, "h-later").await.code, ACCESS_REJECT);
        assert_eq!(login(&server, "h-wrap-in").await.code, ACCESS_ACCEPT);
        assert_eq!(login(&server, "h-wrap-out").await.code, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_simultaneous_use_example() {
        let schema = [
            "CREATE TABLE IF NOT EXISTS active_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, last_seen INTEGER NOT NULL, nas TEXT NOT NULL, PRIMARY KEY (client, session_id))",
            "CREATE INDEX IF NOT EXISTS active_sessions_user ON active_sessions (username)",
        ];
        let server = server(include_str!("../../rradius-server/examples/simultaneous-use.lua"), &schema).await;

        assert_eq!(login(&server, "s-user").await.code, ACCESS_ACCEPT);
        accounting(&server, session("s-user", "s-a", 1, 0)).await;
        assert_eq!(login(&server, "s-user").await.code, ACCESS_ACCEPT, "below the limit");
        accounting(&server, session("s-user", "s-b", 1, 0)).await;
        accounting(&server, session("s-user", "s-b", 3, 0)).await; // an update is not another session
        assert_eq!(login(&server, "s-user").await.code, ACCESS_REJECT, "at the limit");
        assert_eq!(login(&server, "s-other").await.code, ACCESS_ACCEPT, "another user");

        accounting(&server, session("s-user", "s-a", 2, 0)).await;
        assert_eq!(login(&server, "s-user").await.code, ACCESS_ACCEPT, "a Stop frees a place");

        accounting(&server, session("s-user", "s-c", 1, 0)).await;
        assert_eq!(login(&server, "s-user").await.code, ACCESS_REJECT);
        let old = now_unix() as i64 - 1000;
        test_db().execute(&format!("UPDATE active_sessions SET last_seen = {old} WHERE session_id = 's-c'")).await.unwrap();
        assert_eq!(login(&server, "s-user").await.code, ACCESS_ACCEPT, "a stale session does not count");
    }

    /// The CoA relay's session_query reads this table: `nas` says where a Disconnect goes, and
    /// the query the script suggests skips sessions it no longer counts, newest first.
    #[tokio::test]
    async fn test_simultaneous_use_example_records_where_to_send() {
        let schema = [
            "CREATE TABLE IF NOT EXISTS active_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, last_seen INTEGER NOT NULL, nas TEXT NOT NULL, PRIMARY KEY (client, session_id))",
            "CREATE INDEX IF NOT EXISTS active_sessions_user ON active_sessions (username)",
        ];
        let script = include_str!("../../rradius-server/examples/simultaneous-use.lua");
        let server = server(script, &schema).await;
        let mut v4 = session("w-user", "w-v4", 1, 0);
        v4.push((4, vec![192, 0, 2, 7]));
        accounting(&server, v4).await;
        let mut v6 = session("w-user", "w-v6", 1, 0);
        v6.push((95, "2001:db8::7".parse::<std::net::Ipv6Addr>().unwrap().octets().to_vec()));
        v6.push((41, 5u32.to_be_bytes().to_vec())); // Acct-Delay-Time: seen 5 seconds earlier
        accounting(&server, v6).await;
        let mut plain = session("w-user", "w-plain", 1, 0);
        plain.push((41, 10u32.to_be_bytes().to_vec()));
        accounting(&server, plain).await;
        accounting(&server, session("w-user", "w-stale", 1, 0)).await;
        let old = now_unix() as i64 - 1000;
        test_db().execute(&format!("UPDATE active_sessions SET last_seen = {old} WHERE session_id = 'w-stale'")).await.unwrap();

        let query: String = script
            .lines()
            .skip_while(|line| !line.contains("SELECT nas, session_id"))
            .take_while(|line| line.starts_with("--   "))
            .map(|line| line.trim_start_matches("--").trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(query.matches('?').count(), 1, "{query}");
        let rows = test_db().query_with_params(&query, &["w-user"]).await.unwrap();
        let found: Vec<(String, String)> = rows
            .iter()
            .map(|row| match (&row[0].1, &row[1].1) {
                (crate::lua_engine::DbValue::Text(nas), crate::lua_engine::DbValue::Text(session)) => (nas.clone(), session.clone()),
                other => panic!("{other:?}"),
            })
            .collect();
        let expected = [("192.0.2.7", "w-v4"), ("2001:db8::7", "w-v6"), ("nas-a", "w-plain")];
        assert_eq!(found, expected.map(|(nas, session)| (nas.to_string(), session.to_string())));
    }

    #[tokio::test]
    async fn test_simultaneous_use_example_session_id_from_two_nases() {
        let schema = [
            "CREATE TABLE IF NOT EXISTS active_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, last_seen INTEGER NOT NULL, nas TEXT NOT NULL, PRIMARY KEY (client, session_id))",
            "CREATE INDEX IF NOT EXISTS active_sessions_user ON active_sessions (username)",
        ];
        let server = server(include_str!("../../rradius-server/examples/simultaneous-use.lua"), &schema).await;

        accounting(&server, session("n-user", "n-same", 1, 0)).await;
        accounting_from_other_nas(&server, session("n-user", "n-same", 1, 0)).await;
        assert_eq!(login(&server, "n-user").await.code, ACCESS_REJECT, "two sessions, although with one ID");
        accounting_from_other_nas(&server, session("n-user", "n-same", 2, 0)).await;
        assert_eq!(login(&server, "n-user").await.code, ACCESS_ACCEPT, "one left");
        accounting_from_other_nas(&server, session("n-user", "n-other", 1, 0)).await;
        assert_eq!(login(&server, "n-user").await.code, ACCESS_REJECT, "the other NAS's session is still there");
    }

    #[tokio::test]
    async fn test_simultaneous_use_example_logs_what_it_cannot_count() {
        let schema = [
            "CREATE TABLE IF NOT EXISTS active_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, last_seen INTEGER NOT NULL, nas TEXT NOT NULL, PRIMARY KEY (client, session_id))",
            "CREATE INDEX IF NOT EXISTS active_sessions_user ON active_sessions (username)",
        ];
        let server = server(include_str!("../../rradius-server/examples/simultaneous-use.lua"), &schema).await;
        accounting(&server, no_session("u-unusable-start", 1)).await;
        assert!(logged("u-unusable-start"), "a session that is not counted must be logged");
        accounting(&server, session("u-empty-session", "", 1, 0)).await;
        assert!(logged("u-empty-session"), "an empty session ID is no session ID");
        assert_eq!(login(&server, "").await.code, ACCESS_REJECT, "an empty name is no name");
    }

    #[tokio::test]
    async fn test_simultaneous_use_example_user_with_a_quote() {
        let schema = [
            "CREATE TABLE IF NOT EXISTS active_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, last_seen INTEGER NOT NULL, nas TEXT NOT NULL, PRIMARY KEY (client, session_id))",
            "CREATE INDEX IF NOT EXISTS active_sessions_user ON active_sessions (username)",
        ];
        let server = server(include_str!("../../rradius-server/examples/simultaneous-use.lua"), &schema).await;
        assert_eq!(login(&server, "u-o'brien").await.code, ACCESS_ACCEPT);
        accounting(&server, session("u-o'brien", "u-it's one", 1, 0)).await;
        accounting(&server, session("u-o'brien", "u-it's two", 1, 0)).await;
        assert_eq!(login(&server, "u-o'brien").await.code, ACCESS_REJECT, "both sessions counted");
        accounting(&server, session("u-o'brien", "u-it's one", 2, 0)).await;
        assert_eq!(login(&server, "u-o'brien").await.code, ACCESS_ACCEPT, "the Stop freed a place");
    }

    /// Accounting that says nothing about which session it is.
    fn no_session(user: &str, status: u32) -> Vec<(u8, Vec<u8>)> {
        vec![(1, user.as_bytes().to_vec()), (40, status.to_be_bytes().to_vec()), (46, 60u32.to_be_bytes().to_vec())]
    }

    fn pool_accounting(user: &str, status: u32, address: [u8; 4]) -> Vec<(u8, Vec<u8>)> {
        vec![(1, user.as_bytes().to_vec()), (40, status.to_be_bytes().to_vec()), (8, address.to_vec())]
    }

    /// A spooled request can run twice: the process can die between the script and the progress
    /// file. Old ones say so with Acct-Delay-Time, and must not look like news.
    #[tokio::test]
    async fn test_example_scripts_can_run_a_request_twice() {
        let schema = [
            "CREATE TABLE IF NOT EXISTS quota_users (username TEXT PRIMARY KEY, quota_seconds INTEGER NOT NULL, from_hour INTEGER NOT NULL DEFAULT 0, to_hour INTEGER NOT NULL DEFAULT 24)",
            "CREATE TABLE IF NOT EXISTS quota_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, seconds INTEGER NOT NULL, PRIMARY KEY (client, session_id))",
            "INSERT INTO quota_users (username, quota_seconds) VALUES ('r-quota', 100)",
        ];
        let quota = server(include_str!("../../rradius-server/examples/quota-and-hours.lua"), &schema).await;
        for _ in 0..2 {
            accounting(&quota, session("r-quota", "r-q1", 2, 60)).await;
        }
        assert_eq!(login(&quota, "r-quota").await.attribute(27), Some(&40u32.to_be_bytes()[..]), "used 60 once");

        let schema = [
            "CREATE TABLE IF NOT EXISTS active_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL, username TEXT NOT NULL, last_seen INTEGER NOT NULL, nas TEXT NOT NULL, PRIMARY KEY (client, session_id))",
        ];
        let sessions = server(include_str!("../../rradius-server/examples/simultaneous-use.lua"), &schema).await;
        for session_id in ["r-s1", "r-s1", "r-s2"] {
            accounting(&sessions, session("r-sessions", session_id, 1, 0)).await;
        }
        assert_eq!(login(&sessions, "r-sessions").await.code, ACCESS_REJECT, "two sessions, not three");
        for _ in 0..2 {
            accounting(&sessions, session("r-sessions", "r-s1", 2, 0)).await;
        }
        assert_eq!(login(&sessions, "r-sessions").await.code, ACCESS_ACCEPT, "the second Stop changes nothing");
        // From an hour ago: this says nothing about the session being alive now.
        for session_id in ["r-s3", "r-s4"] {
            let mut old = session("r-old", session_id, 1, 0);
            old.push((41, 3600u32.to_be_bytes().to_vec()));
            accounting(&sessions, old).await;
        }
        assert_eq!(login(&sessions, "r-old").await.code, ACCESS_ACCEPT, "a Start from long ago counted as a session now");
    }

    #[tokio::test]
    async fn test_ip_pool_example() {
        let schema = [
            "CREATE TABLE ip_pool (ip TEXT PRIMARY KEY, holder TEXT, expires INTEGER NOT NULL DEFAULT 0)",
            "INSERT INTO ip_pool (ip) VALUES ('10.9.0.1'), ('10.9.0.2')",
        ];
        let server = server(include_str!("../../rradius-server/examples/ip-pool.lua"), &schema).await;

        let first = login(&server, "p-a").await;
        let second = login(&server, "p-b").await;
        let mut handed_out = [first.attribute(8).unwrap().to_vec(), second.attribute(8).unwrap().to_vec()];
        handed_out.sort();
        assert_eq!(handed_out, [vec![10, 9, 0, 1], vec![10, 9, 0, 2]]);
        assert_eq!(login(&server, "p-c").await.code, ACCESS_REJECT, "the pool ran out");

        // Whoever holds an address, the other one went to the other user.
        let holder_of_first = if first.attribute(8) == Some(&[10, 9, 0, 1][..]) { "p-a" } else { "p-b" };
        accounting(&server, pool_accounting(holder_of_first, 2, [10, 9, 0, 1])).await;
        assert_eq!(login(&server, "p-c").await.attribute(8), Some(&[10, 9, 0, 1][..]), "a Stop frees the address");

        test_db().execute("UPDATE ip_pool SET expires = 1 WHERE ip = '10.9.0.2'").await.unwrap();
        assert_eq!(login(&server, "p-d").await.attribute(8), Some(&[10, 9, 0, 2][..]), "a lease without news runs out");
        accounting(&server, pool_accounting("p-d", 3, [10, 9, 0, 2])).await;
        assert_eq!(login(&server, "p-e").await.code, ACCESS_REJECT);

        // 10.9.0.1 is p-c's now. A NAS resending the Stop of whoever had it before must not free it.
        accounting(&server, pool_accounting(holder_of_first, 2, [10, 9, 0, 1])).await;
        assert_eq!(login(&server, "p-e").await.code, ACCESS_REJECT, "a late Stop freed someone else's address");
        // Nor may a late Interim-Update keep someone else's lease alive.
        test_db().execute("UPDATE ip_pool SET expires = 1 WHERE ip = '10.9.0.1'").await.unwrap();
        accounting(&server, pool_accounting(holder_of_first, 3, [10, 9, 0, 1])).await;
        // A request with no name could not be matched to its Stop.
        let nameless = RadiusPacket::create_access_request(1, [1; 16], vec![], SECRET);
        let nameless = RadiusPacket::parse(&server.handle_auth(&nameless, source()).await.unwrap()).unwrap();
        assert_eq!(nameless.code, ACCESS_REJECT, "no name to record");
        assert_eq!(login(&server, "").await.code, ACCESS_REJECT, "an empty name is no name");
        assert_eq!(login(&server, "p-e").await.attribute(8), Some(&[10, 9, 0, 1][..]), "a late Interim-Update extended someone else's lease");
        // The holder's own Stop still frees the address.
        accounting(&server, pool_accounting("p-e", 2, [10, 9, 0, 1])).await;
        assert_eq!(login(&server, "p-f").await.attribute(8), Some(&[10, 9, 0, 1][..]), "the holder's Stop frees the address");
        // A quote in a name works from login to Stop.
        accounting(&server, pool_accounting("p-f", 2, [10, 9, 0, 1])).await;
        assert_eq!(login(&server, "p-o'brien").await.attribute(8), Some(&[10, 9, 0, 1][..]));
        accounting(&server, pool_accounting("p-o'brien", 2, [10, 9, 0, 1])).await;
        assert_eq!(login(&server, "p-g").await.attribute(8), Some(&[10, 9, 0, 1][..]), "the Stop matched the name with a quote");
        // From an hour ago, out of the accounting spool: not news about the lease.
        test_db().execute("UPDATE ip_pool SET expires = 1 WHERE ip = '10.9.0.1'").await.unwrap();
        let mut old = pool_accounting("p-g", 3, [10, 9, 0, 1]);
        old.push((41, 3600u32.to_be_bytes().to_vec()));
        accounting(&server, old).await;
        assert_eq!(login(&server, "p-h").await.attribute(8), Some(&[10, 9, 0, 1][..]), "an update from long ago extended the lease");
        // The same Stop twice is the same as once.
        for _ in 0..2 {
            accounting(&server, pool_accounting("p-h", 2, [10, 9, 0, 1])).await;
        }
        assert_eq!(login(&server, "p-i").await.attribute(8), Some(&[10, 9, 0, 1][..]));
    }
}

#[cfg(test)]
mod metrics_tests {
    use super::*;
    use crate::test_support::{test_db, test_vms};

    async fn counted_server(script: &str) -> (RadiusServer, Arc<Metrics>) {
        let engine = LuaAuthEngine::new(test_vms(), test_db(), script).unwrap();
        let clients = ClientTable::new(vec![("127.0.0.1", None, b"s".to_vec(), None)]).unwrap();
        let any = "127.0.0.1:0".parse().unwrap();
        let metrics = Arc::new(Metrics::default());
        let server = RadiusServer::new(any, any, Arc::new(clients), Arc::new(engine), share_test_dictionary()).with_metrics(metrics.clone());
        (server, metrics)
    }

    fn request(identifier: u8, user: &[u8]) -> Vec<u8> {
        RadiusPacket::create_access_request(identifier, [identifier; 16], vec![(1, user.to_vec())], b"s")
    }

    const LOCAL: &str = "127.0.0.1:50000";

    #[tokio::test]
    async fn test_accept_tried_again_after_failing() {
        let mut attempts = 0;
        let started = Instant::now();
        let accepted = retry_accept(
            || {
                attempts += 1;
                let attempt = attempts;
                async move {
                    if attempt < 3 {
                        return Err(io::Error::from_raw_os_error(24)); // EMFILE
                    }
                    Ok(attempt)
                }
            },
            "test",
            &mut DiscardLog::default(),
            Duration::from_millis(20),
        )
        .await;
        assert_eq!(accepted, 3);
        assert!(started.elapsed() >= Duration::from_millis(40), "it did not wait between attempts: {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn test_authentication_counted() {
        let (server, metrics) = counted_server("return get_attr('User-Name') == 'good' and 'accept' or 'reject'").await;
        let source = LOCAL.parse().unwrap();
        server.receive(PacketKind::Auth, &request(1, b"good"), source).await.unwrap();
        server.receive(PacketKind::Auth, &request(2, b"bad"), source).await.unwrap();
        assert_eq!(metrics.radius_packets_auth.get(), 2);
        assert_eq!((metrics.radius_access_accept.get(), metrics.radius_access_reject.get()), (1, 1));
    }

    #[tokio::test]
    async fn test_accounting_counted() {
        let (server, metrics) = counted_server("return 'accept'").await;
        let packet = RadiusPacket::create_accounting_request(1, vec![(1, b"a".to_vec())], b"s");
        server.receive(PacketKind::Acct, &packet, LOCAL.parse().unwrap()).await.unwrap();
        assert_eq!((metrics.radius_packets_acct.get(), metrics.radius_accounting_response.get()), (1, 1));
    }

    #[tokio::test]
    async fn test_discards_counted() {
        let (server, metrics) = counted_server("return 'accept'").await;
        assert!(server.receive(PacketKind::Auth, &request(1, b"a"), "192.0.2.1:1".parse().unwrap()).await.is_none());
        let forged = RadiusPacket::create_access_request(2, [2; 16], vec![(1, b"a".to_vec())], b"wrong");
        assert!(server.receive(PacketKind::Auth, &forged, LOCAL.parse().unwrap()).await.is_none());
        assert_eq!((metrics.radius_discarded_unknown_client.get(), metrics.radius_discarded_invalid.get()), (1, 1));
        assert_eq!(metrics.radius_packets_auth.get(), 0);
    }

    #[tokio::test]
    async fn test_retransmission_counted() {
        let (server, metrics) = counted_server("return 'accept'").await;
        let source = LOCAL.parse().unwrap();
        for _ in 0..2 {
            server.receive(PacketKind::Auth, &request(1, b"a"), source).await.unwrap();
        }
        assert_eq!(metrics.radius_duplicates.get(), 1);
        assert_eq!((metrics.radius_packets_auth.get(), metrics.radius_access_accept.get()), (1, 1));
    }

    #[tokio::test]
    async fn test_status_server_counted() {
        let (server, metrics) = counted_server("return 'accept'").await;
        let mut status = RadiusPacket { code: STATUS_SERVER, identifier: 1, authenticator: [1; 16], attributes: vec![] };
        status.push_message_authenticator(b"s");
        server.receive(PacketKind::Auth, &status.encode(), LOCAL.parse().unwrap()).await.unwrap();
        assert_eq!(metrics.radius_status_server.get(), 1);
        assert_eq!((metrics.radius_packets_auth.get(), metrics.radius_access_accept.get()), (0, 0));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) use crate::test_support::radius::{logged_during, CapturedLog};
    use crate::auth_chain::AuthChain;
    use crate::clients::MessageAuthenticatorPolicy as Policy;

    const SECRET: &[u8] = b"xyzzy5461";

    // RFC 2865 section 7.1: Access-Request for user "nemo" and its Access-Accept.
    const RFC_REQUEST: &str = "010000380f403f9473978057bd83d5cb98f4227a01066e656d6f02120dbe708d93d413ce3196e43f782a0aee0406c0a8011005060000 0003";
    const RFC_ACCEPT: &str = "0200002686fe220e7624ba2a1005f6bf9b55e0b20606000000010f06000000000e06c0a80103";

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn test_parse_rfc2865_access_request() {
        let packet = RadiusPacket::parse(&hex(RFC_REQUEST)).unwrap();
        assert_eq!(packet.code, ACCESS_REQUEST);
        assert_eq!(packet.identifier, 0);
        assert_eq!(packet.attribute(1), Some(&b"nemo"[..]));
        assert_eq!(packet.attribute(4), Some(&[192, 168, 1, 16][..]));
        assert_eq!(packet.attribute(5), Some(&[0, 0, 0, 3][..]));
        assert_eq!(packet.attributes.len(), 4);
        assert_eq!(packet.encode(), hex(RFC_REQUEST));
    }

    #[test]
    fn test_rfc2865_response_authenticator() {
        let request = RadiusPacket::parse(&hex(RFC_REQUEST)).unwrap();
        let attributes = vec![
            (6, vec![0, 0, 0, 1]),
            (15, vec![0, 0, 0, 0]),
            (14, vec![192, 168, 1, 3]),
        ];
        let response = RadiusPacket::signed_response(ACCESS_ACCEPT, &request, attributes, SECRET, false);
        assert_eq!(response, hex(RFC_ACCEPT));
    }

    #[test]
    fn test_malformed_packets_rejected() {
        let valid = hex(RFC_REQUEST);

        let mut short_attr = valid.clone();
        short_attr[21] = 1; // User-Name length below 2
        assert!(RadiusPacket::parse(&short_attr).is_err());

        let mut overrun = valid.clone();
        overrun[21] = 60; // User-Name runs past the end of the packet
        assert!(RadiusPacket::parse(&overrun).is_err());

        let mut wrong_length = valid.clone();
        wrong_length[3] = 0x30; // header Length 48, packet is 56 octets
        assert!(RadiusPacket::parse(&wrong_length).is_err());
    }

    const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    fn any_addr() -> SocketAddr {
        ([0, 0, 0, 0], 0).into()
    }

    /// A table with one client, 127.0.0.1, using `secret`. It allows requests without a
    /// Message-Authenticator, as the RFC 2865 example requests have none.
    fn local_clients(secret: &str) -> Arc<ClientTable> {
        clients_with(secret, Policy::No, Policy::Yes)
    }

    fn clients_with(secret: &str, require: Policy, limit: Policy) -> Arc<ClientTable> {
        let mut table = ClientTable::default();
        let options = crate::clients::ClientOptions { require_message_authenticator: require, limit_proxy_state: limit, ..Default::default() };
        table.add("127.0.0.1", Some("local".to_string()), secret.as_bytes().to_vec(), options).unwrap();
        Arc::new(table)
    }

    async fn enforcing_server(require: Policy, limit: Policy) -> RadiusServer {
        test_server("s").await.with_clients(clients_with("s", require, limit))
    }

    /// An Access-Request with no Message-Authenticator, with optional Proxy-State.
    fn bare_request(proxy_state: bool) -> Vec<u8> {
        let mut attributes = vec![(1, b"nemo".to_vec())];
        if proxy_state {
            attributes.push((PROXY_STATE, b"a".to_vec()));
        }
        RadiusPacket { code: ACCESS_REQUEST, identifier: 1, authenticator: [9; 16], attributes }.encode()
    }

    fn signed_request() -> Vec<u8> {
        RadiusPacket::create_access_request(2, [8; 16], vec![(1, b"nemo".to_vec())], b"s")
    }

    fn badly_signed_request() -> Vec<u8> {
        RadiusPacket::create_access_request(3, [7; 16], vec![(1, b"nemo".to_vec())], b"wrong")
    }

    fn encode_attribute(dictionary: &Dictionary, name: &str, value: &AttrValue) -> Result<(u8, Vec<u8>), String> {
        encode_response_attribute(dictionary, name, value, b"s", &[0; 16], &mut Vec::new())
    }

    fn status_request(secret: &[u8]) -> Vec<u8> {
        // A request's Message-Authenticator covers its own bytes, code included.
        let mut packet = RadiusPacket { code: 12, identifier: 5, authenticator: [6; 16], attributes: Vec::new() };
        packet.attributes.push((MESSAGE_AUTHENTICATOR, vec![0; 16]));
        let mut bytes = packet.encode();
        let mac = hmac_md5(secret, &bytes);
        bytes[22..38].copy_from_slice(&mac);
        bytes
    }

    fn source() -> SocketAddr {
        SocketAddr::new(LOCALHOST, 40000)
    }

    #[tokio::test]
    async fn test_status_server_on_authentication_port() {
        // The script rejects everything and records each run, so an answer proves it did not run.
        let server = script_server("s", "sql_execute('CREATE TABLE status_ran (x INTEGER)') return 'reject'").await;
        let bytes = server.receive(PacketKind::Auth, &status_request(b"s"), source()).await.expect("no response");
        let response = RadiusPacket::parse(&bytes).unwrap();
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.identifier, 5);
        assert_eq!(response.attributes.len(), 1);
        assert_signed_response(&bytes, &[6; 16], b"s");
        assert!(crate::test_support::test_db().query("SELECT x FROM status_ran").await.is_err(), "the script ran");
    }

    #[tokio::test]
    async fn test_status_server_on_accounting_port() {
        let server = script_server("s", "return 'reject'").await;
        let bytes = server.receive(PacketKind::Acct, &status_request(b"s"), source()).await.expect("no response");
        let response = RadiusPacket::parse(&bytes).unwrap();
        assert_eq!(response.code, ACCOUNTING_RESPONSE);
        assert!(response.attributes.is_empty());
        let mut for_response = bytes.clone();
        for_response[4..20].copy_from_slice(&[6; 16]);
        for_response.extend_from_slice(b"s");
        assert_eq!(&bytes[4..20], Md5::digest(&for_response).as_slice());
    }

    #[tokio::test]
    async fn test_status_server_discarded() {
        let server = script_server("s", "return 'accept'").await;
        let bare = RadiusPacket { code: 12, identifier: 1, authenticator: [1; 16], attributes: Vec::new() }.encode();
        assert!(server.receive(PacketKind::Auth, &bare, source()).await.is_none(), "missing Message-Authenticator, client allows that");
        assert!(server.receive(PacketKind::Auth, &status_request(b"wrong"), source()).await.is_none(), "invalid Message-Authenticator");
        let unknown = SocketAddr::new("10.9.9.9".parse().unwrap(), 40000);
        assert!(server.receive(PacketKind::Auth, &status_request(b"s"), unknown).await.is_none(), "unknown client");

        let disabled = script_server("s", "return 'accept'").await.with_status_server(false);
        assert!(disabled.receive(PacketKind::Auth, &status_request(b"s"), source()).await.is_none());
        assert!(disabled.receive(PacketKind::Acct, &status_request(b"s"), source()).await.is_none());
    }

    #[tokio::test]
    async fn test_status_server_answers_each_repeat() {
        let server = script_server("s", "return 'reject'").await;
        for _ in 0..3 {
            assert!(server.receive(PacketKind::Auth, &status_request(b"s"), source()).await.is_some());
        }
    }

    /// Recomputes the Message-Authenticator and the Response Authenticator from the
    /// RFC formulas, rather than with the code under test.
    fn assert_signed_response(bytes: &[u8], request_authenticator: &[u8; 16], secret: &[u8]) {
        let response = RadiusPacket::parse(bytes).unwrap();
        assert_eq!(response.attributes[0].0, MESSAGE_AUTHENTICATOR, "not the first attribute");
        let mut for_mac = bytes.to_vec();
        for_mac[4..20].copy_from_slice(request_authenticator);
        for_mac[22..38].fill(0);
        assert_eq!(response.attributes[0].1, hmac_md5(secret, &for_mac));
        let mut for_response = bytes.to_vec();
        for_response[4..20].copy_from_slice(request_authenticator);
        for_response.extend_from_slice(secret);
        assert_eq!(&bytes[4..20], Md5::digest(&for_response).as_slice());
    }

    const MICROSOFT: u32 = 311;
    const MSCHAP_AUTH_CHALLENGE: [u8; 16] = [0x5B, 0x5D, 0x7C, 0x7D, 0x7B, 0x3F, 0x2F, 0x3E, 0x3C, 0x2C, 0x60, 0x21, 0x32, 0x26, 0x26, 0x28];
    const MSCHAP_PEER_CHALLENGE: [u8; 16] = [0x21, 0x40, 0x23, 0x24, 0x25, 0x5E, 0x26, 0x2A, 0x28, 0x29, 0x5F, 0x2B, 0x3A, 0x33, 0x7C, 0x7E];
    const MSCHAP_NT_RESPONSE: [u8; 24] = [
        0x82, 0x30, 0x9E, 0xCD, 0x8D, 0x70, 0x8B, 0x5E, 0xA0, 0x8F, 0xAA, 0x39, 0x81, 0xCD, 0x83, 0x54, 0x42, 0x33, 0x11, 0x4A, 0x3D, 0x85, 0xD6, 0xDF,
    ];

    fn microsoft_vsa(vendor_type: u8, value: &[u8]) -> (u8, Vec<u8>) {
        let mut vsa = MICROSOFT.to_be_bytes().to_vec();
        vsa.extend_from_slice(&[vendor_type, value.len() as u8 + 2]);
        vsa.extend_from_slice(value);
        (VENDOR_SPECIFIC, vsa)
    }

    /// The Microsoft attribute of a response, if there is one.
    fn microsoft_attribute(response: &RadiusPacket, vendor_type: u8) -> Option<Vec<u8>> {
        response.attributes.iter().find_map(|(attr_type, value)| {
            (*attr_type == VENDOR_SPECIFIC && value[..4] == MICROSOFT.to_be_bytes() && value[4] == vendor_type).then(|| value[6..].to_vec())
        })
    }

    fn mschap_request(user: &str, authenticator: [u8; 16], challenge_len: usize, response_len: usize) -> Vec<u8> {
        let mut response = vec![7u8, 0];
        response.extend_from_slice(&MSCHAP_PEER_CHALLENGE);
        response.extend_from_slice(&[0; 8]);
        response.extend_from_slice(&MSCHAP_NT_RESPONSE);
        response.truncate(response_len);
        let attributes = vec![
            (1, user.as_bytes().to_vec()),
            microsoft_vsa(11, &MSCHAP_AUTH_CHALLENGE[..challenge_len]),
            microsoft_vsa(25, &response),
        ];
        RadiusPacket::create_access_request(9, authenticator, attributes, SECRET)
    }

    async fn mschap_response(user: &str) -> RadiusPacket {
        let server = chain_server().await;
        let request = mschap_request(user, [4; 16], 16, 50);
        RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn test_mschapv2_rfc_vector() {
        let response = mschap_response("User").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        let mut expected = vec![7u8];
        expected.extend_from_slice(b"S=407A5589115FD0D6209F510FE9C04566932CDA56");
        assert_eq!(microsoft_attribute(&response, 26).unwrap(), expected);
    }

    #[tokio::test]
    async fn test_mschapv2_stored_formats_and_domains() {
        assert_eq!(mschap_response("NTDOM\\User").await.code, ACCESS_ACCEPT, "{{nthash}}, with a domain");
        assert_eq!(mschap_response("BIGCO\\User").await.code, ACCESS_ACCEPT, "a domain in the user name");
        assert_eq!(mschap_response("SHA\\User").await.code, ACCESS_REJECT, "a hash that cannot be used");
    }

    #[tokio::test]
    async fn test_mschapv2_wrong_password() {
        let response = mschap_response("WRONG\\User").await;
        assert_eq!(response.code, ACCESS_REJECT);
        let error = microsoft_attribute(&response, 2).expect("no MS-CHAP-Error");
        assert_eq!(error[0], 7, "the identifier");
        assert!(error[1..].starts_with(b"E=691 R=0"), "{error:?}");
        assert!(microsoft_attribute(&response, 26).is_none());
    }

    /// RFC 2865 section 5.26 lets one Vendor-Specific carry several vendor attributes.
    #[tokio::test]
    async fn test_mschapv2_attributes_in_one_vendor_specific() {
        let mut response = vec![7u8, 0];
        response.extend_from_slice(&MSCHAP_PEER_CHALLENGE);
        response.extend_from_slice(&[0; 8]);
        response.extend_from_slice(&MSCHAP_NT_RESPONSE);
        let mut vsa = MICROSOFT.to_be_bytes().to_vec();
        vsa.extend_from_slice(&[11, 18]);
        vsa.extend_from_slice(&MSCHAP_AUTH_CHALLENGE);
        vsa.extend_from_slice(&[25, 52]);
        vsa.extend_from_slice(&response);
        let request = RadiusPacket::create_access_request(9, [4; 16], vec![(1, b"User".to_vec()), (VENDOR_SPECIFIC, vsa)], SECRET);
        let server = chain_server().await;
        let reply = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(reply.code, ACCESS_ACCEPT);
    }

    #[tokio::test]
    async fn test_mschapv2_malformed_attributes() {
        let server = chain_server().await;
        for (challenge, response) in [(15, 50), (16, 49)] {
            let request = mschap_request("User", [4; 16], challenge, response);
            let reply = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
            assert_eq!(reply.code, ACCESS_REJECT);
            assert!(microsoft_attribute(&reply, 2).is_none(), "it is not an MS-CHAPv2 request at all");
        }
    }

    /// RFC 2548 section 2.4.2, written out here rather than using the code under test.
    fn decrypt_mppe_key(value: &[u8], authenticator: &[u8; 16]) -> ([u8; 2], Vec<u8>) {
        let salt = [value[0], value[1]];
        assert_ne!(salt[0] & 0x80, 0);
        let mut plain = Vec::new();
        let mut previous = [authenticator.as_slice(), salt.as_slice()].concat();
        for chunk in value[2..].chunks(16) {
            let pad = Md5::digest([SECRET, previous.as_slice()].concat());
            plain.extend(chunk.iter().zip(pad.iter()).map(|(c, b)| c ^ b));
            previous = chunk.to_vec();
        }
        let length = plain[0] as usize;
        (salt, plain[1..1 + length].to_vec())
    }

    #[tokio::test]
    async fn test_mschapv2_mppe_keys() {
        let response = mschap_response("User").await;
        let (send_salt, send) = decrypt_mppe_key(&microsoft_attribute(&response, 16).expect("no send key"), &[4; 16]);
        let (recv_salt, recv) = decrypt_mppe_key(&microsoft_attribute(&response, 17).expect("no receive key"), &[4; 16]);
        assert_ne!(send_salt, recv_salt);
        // RFC 3079 section 3.5.3 gives the send key. It gives no receive key to compare.
        assert_eq!(send, [0x8B, 0x7C, 0xDC, 0x14, 0x9B, 0x99, 0x3A, 0x1B, 0xA1, 0x18, 0xCB, 0x15, 0x3F, 0x56, 0xDC, 0xCB]);
        // Computed separately from the formula in RFC 3079 sections 3.3 and 3.4, by a script that
        // also reproduces the send key above. This guards against changes; whether the two
        // directions are the right way round takes a real device to show.
        assert_eq!(recv, [0xD5, 0xF0, 0xE9, 0x52, 0x1E, 0x3E, 0xA9, 0x58, 0x96, 0x45, 0xE8, 0x60, 0x51, 0xC8, 0x22, 0x26]);
    }

    /// Tables shared with `test_two_steps_with_totp`, whichever runs first.
    async fn two_step_tables() {
        static CREATED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        CREATED
            .get_or_init(|| async {
                let db = crate::test_support::test_db();
                db.execute("CREATE TABLE mfa_users (username TEXT PRIMARY KEY, password TEXT, totp_secret TEXT, last_step INTEGER NOT NULL DEFAULT 0, failed_attempts INTEGER NOT NULL DEFAULT 0, locked_until INTEGER NOT NULL DEFAULT 0)").await.unwrap();
                db.execute("CREATE TABLE mfa_pending (state TEXT PRIMARY KEY, username TEXT, expires INTEGER)").await.unwrap();
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_two_step_user_with_a_quote() {
        const TOTP: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        two_step_tables().await;
        let db = crate::test_support::test_db();
        db.execute(&format!("INSERT INTO mfa_users (username, password, totp_secret) VALUES ('o''brien', 'pw', '{TOTP}')")).await.unwrap();
        let server = script_server("xyzzy5461", include_str!("../../rradius-server/examples/two-step-totp.lua")).await;
        let send = |id: u8, password: String, state: Option<Vec<u8>>| {
            let ra = [id; 16];
            let mut attributes = vec![(1, b"o'brien".to_vec()), (USER_PASSWORD, encrypt_password(&password, &ra))];
            attributes.extend(state.map(|s| (24, s)));
            RadiusPacket::create_access_request(id, ra, attributes, SECRET)
        };
        let challenge = RadiusPacket::parse(&server.handle_auth(&send(1, "pw".into(), None), LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(challenge.code, ACCESS_CHALLENGE);
        let state = challenge.attribute(24).unwrap().to_vec();
        let code = crate::totp::code_at(TOTP, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
        let answer = RadiusPacket::parse(&server.handle_auth(&send(2, code, Some(state)), LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(answer.code, ACCESS_ACCEPT);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_two_step_lockout() {
        const TOTP: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        two_step_tables().await;
        let db = crate::test_support::test_db();
        db.execute(&format!("INSERT INTO mfa_users (username, password, totp_secret) VALUES ('lockme', 'pw', '{TOTP}')")).await.unwrap();
        let server = script_server("xyzzy5461", include_str!("../../rradius-server/examples/two-step-totp.lua")).await;
        let identifier = std::sync::atomic::AtomicU8::new(0);
        let step = async |password: &str, state: Option<Vec<u8>>| {
            let id = identifier.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let ra = [id; 16];
            let mut attributes = vec![(1, b"lockme".to_vec()), (USER_PASSWORD, encrypt_password(password, &ra))];
            attributes.extend(state.map(|s| (24, s)));
            let request = RadiusPacket::create_access_request(id, ra, attributes, SECRET);
            RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap()
        };
        let attempt = async |code: &str| {
            let challenge = step("pw", None).await;
            if challenge.code != ACCESS_CHALLENGE {
                return challenge;
            }
            step(code, challenge.attribute(24).map(<[u8]>::to_vec)).await
        };
        let failures = async || {
            match &db.query("SELECT failed_attempts FROM mfa_users WHERE username = 'lockme'").await.unwrap()[0][0].1 {
                crate::lua_engine::DbValue::Integer(n) => *n,
                other => panic!("{other:?}"),
            }
        };

        for _ in 0..5 {
            assert_eq!(attempt("abcdef").await.code, ACCESS_REJECT);
        }
        // Locked: no challenge, even for the right password.
        assert_eq!(step("pw", None).await.code, ACCESS_REJECT);

        db.execute("UPDATE mfa_users SET locked_until = 0 WHERE username = 'lockme'").await.unwrap();
        assert_eq!(attempt("abcdef").await.code, ACCESS_REJECT);
        assert_eq!(failures().await, 1, "the count starts again after a lockout");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(attempt(&crate::totp::code_at(TOTP, now)).await.code, ACCESS_ACCEPT);
        assert_eq!(failures().await, 0, "success resets the count");
    }

    /// The script in the usage document, run as it stands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_two_steps_with_totp() {
        const TOTP: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        let db = crate::test_support::test_db();
        two_step_tables().await;
        db.execute(&format!("INSERT INTO mfa_users (username, password, totp_secret) VALUES ('nemo', 'pw', '{TOTP}')")).await.unwrap();
        let server = script_server("xyzzy5461", include_str!("../../rradius-server/examples/two-step-totp.lua")).await;

        let send = |id: u8, password: String, state: Option<Vec<u8>>| {
            let ra = [id; 16];
            let mut attributes = vec![(1, b"nemo".to_vec()), (USER_PASSWORD, encrypt_password(&password, &ra))];
            attributes.extend(state.map(|s| (24, s)));
            RadiusPacket::create_access_request(id, ra, attributes, SECRET)
        };
        let response = |bytes: Option<Vec<u8>>| RadiusPacket::parse(&bytes.unwrap()).unwrap();
        let code = crate::totp::code_at(TOTP, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());

        // A wrong password gets no challenge.
        let wrong = response(server.handle_auth(&send(1, "nope".into(), None), LOCALHOST).await);
        assert_eq!(wrong.code, ACCESS_REJECT);

        // Neither does an account without a password, whatever the request carries.
        db.execute(&format!("INSERT INTO mfa_users (username, password, totp_secret) VALUES ('nullpw', NULL, '{TOTP}'), ('emptypw', '', '{TOTP}')")).await.unwrap();
        for (id, user, attributes) in [
            (10, "nullpw", vec![]),
            (11, "emptypw", vec![(USER_PASSWORD, encrypt_password("", &[11; 16]))]),
            (12, "emptypw", vec![]),
        ] {
            let mut all = vec![(1, user.as_bytes().to_vec())];
            all.extend(attributes);
            let request = RadiusPacket::create_access_request(id, [id; 16], all, SECRET);
            assert_eq!(response(server.handle_auth(&request, LOCALHOST).await).code, ACCESS_REJECT, "{user}, request {id}");
        }

        let challenge = response(server.handle_auth(&send(2, "pw".into(), None), LOCALHOST).await);
        assert_eq!(challenge.code, ACCESS_CHALLENGE);
        assert_eq!(challenge.attribute(18), Some(&b"Enter your code"[..]));
        let state = challenge.attribute(24).unwrap().to_vec();

        let accept = response(server.handle_auth(&send(3, code.clone(), Some(state.clone())), LOCALHOST).await);
        assert_eq!(accept.code, ACCESS_ACCEPT);

        // The state is used up, and so is the code, even with a new challenge.
        let again = response(server.handle_auth(&send(4, code.clone(), Some(state)), LOCALHOST).await);
        assert_eq!(again.code, ACCESS_REJECT, "state used twice");
        let challenge = response(server.handle_auth(&send(5, "pw".into(), None), LOCALHOST).await);
        let state = challenge.attribute(24).unwrap().to_vec();
        let replay = response(server.handle_auth(&send(6, code, Some(state)), LOCALHOST).await);
        assert_eq!(replay.code, ACCESS_REJECT, "code used twice");
    }

    #[tokio::test]
    async fn test_access_challenge_sent() {
        let script = "set_attr('Reply-Message', 'Enter your code') set_attr('State', 'abc') return 'challenge'";
        let server = script_server("xyzzy5461", script).await;
        let bytes = server.handle_auth(&hex(RFC_REQUEST), LOCALHOST).await.unwrap();
        let response = RadiusPacket::parse(&bytes).unwrap();
        assert_eq!(response.code, 11);
        assert_eq!(response.identifier, 0);
        assert_eq!(response.attribute(18), Some(&b"Enter your code"[..]));
        assert_eq!(response.attribute(24), Some(&b"abc"[..]));
        assert_signed_response(&bytes, &RadiusPacket::parse(&hex(RFC_REQUEST)).unwrap().authenticator, b"xyzzy5461");
    }

    #[tokio::test]
    async fn test_challenge_copies_proxy_state() {
        let server = script_server("s", "set_attr('Reply-Message', 'code?') return 'challenge'").await;
        let request = RadiusPacket::create_access_request(1, [1; 16], vec![(PROXY_STATE, b"a".to_vec())], b"s");
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_CHALLENGE);
        let types: Vec<u8> = response.attributes.iter().map(|(t, _)| *t).collect();
        assert_eq!(types, [MESSAGE_AUTHENTICATOR, 18, PROXY_STATE]);
    }

    #[tokio::test]
    async fn test_challenge_from_a_chain() {
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![crate::auth_chain::Handler::Lua(
            LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "set_attr('State', 'abc') return 'challenge'").unwrap(),
        )]);
        let server = script_server("s", "return 'reject'").await.with_auth_chains(Arc::new(AuthChains::new(Some(chain), Vec::new())));
        let request = RadiusPacket::create_access_request(1, [1; 16], vec![(1, b"n".to_vec())], b"s");
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_CHALLENGE);
        assert_eq!(response.attribute(24), Some(&b"abc"[..]));
    }

    #[tokio::test]
    async fn test_challenge_with_other_allowed_attributes() {
        let (response, _) = rfc_request_response("
            set_attr('Session-Timeout', 60) set_attr('Idle-Timeout', 30) set_attr('Cisco-AVPair', 'a=b')
            return 'challenge'").await;
        assert_eq!(response.code, 11);
        assert_eq!(response.attribute(27), Some(&[0, 0, 0, 60][..]));
        assert_eq!(response.attribute(28), Some(&[0, 0, 0, 30][..]));
        assert!(response.attribute(VENDOR_SPECIFIC).is_some());
    }

    #[tokio::test]
    async fn test_challenge_attribute_not_allowed() {
        let (response, _) = rfc_request_response("set_attr('Filter-Id', 'x') return 'challenge'").await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert!(response.attribute(11).is_none());
        let log = crate::test_support::logged_errors().lock().unwrap().join("\n");
        assert!(log.contains("Filter-Id"), "the reason was not logged: {log}");
    }

    #[tokio::test]
    async fn test_challenge_with_evs() {
        let (response, _) = rfc_request_response("set_attr('FreeRADIUS-802.1X-Anonce', 'x') return 'challenge'").await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert!(response.attribute(245).is_none());
        let log = crate::test_support::logged_errors().lock().unwrap().join("\n");
        assert!(log.contains("FreeRADIUS-802.1X-Anonce"), "the reason was not logged: {log}");
    }

    #[tokio::test]
    async fn test_challenge_with_a_vendor_attribute_still_allowed() {
        let (response, _) = rfc_request_response("set_attr('WiMAX-Release', 'x') set_attr('Cisco-AVPair', 'a=b') return 'challenge'").await;
        assert_eq!(response.code, ACCESS_CHALLENGE);
    }

    #[tokio::test]
    async fn test_challenge_with_two_states() {
        let (response, _) = rfc_request_response("set_attr('State', 'a') set_attr('State', 'b') return 'challenge'").await;
        assert_eq!(response.code, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_state_comes_back() {
        let server = script_server("s", "return get_attr('State') == 'abc' and 'accept' or 'reject'").await;
        let request = RadiusPacket::create_access_request(1, [1; 16], vec![(24, b"abc".to_vec())], b"s");
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_ACCEPT);
    }

    #[tokio::test]
    async fn test_access_responses_start_with_message_authenticator() {
        for (script, code) in [("set_attr('Filter-Id', 'std.ppp') return 'accept'", ACCESS_ACCEPT), ("return 'reject'", ACCESS_REJECT)] {
            let server = script_server("xyzzy5461", script).await;
            let bytes = server.handle_auth(&hex(RFC_REQUEST), LOCALHOST).await.unwrap();
            assert_eq!(RadiusPacket::parse(&bytes).unwrap().code, code);
            assert_signed_response(&bytes, &RadiusPacket::parse(&hex(RFC_REQUEST)).unwrap().authenticator, b"xyzzy5461");
        }
    }

    #[tokio::test]
    async fn test_proxy_state_copied_in_order() {
        let server = script_server("s", "set_attr('Filter-Id', 'x') return 'accept'").await;
        let request = RadiusPacket::create_access_request(1, [1; 16], vec![(1, b"n".to_vec()), (PROXY_STATE, b"a".to_vec()), (PROXY_STATE, b"b".to_vec())], b"s");
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        let types: Vec<u8> = response.attributes.iter().map(|(t, _)| *t).collect();
        assert_eq!(types, [MESSAGE_AUTHENTICATOR, 11, PROXY_STATE, PROXY_STATE]);
        assert_eq!(response.attributes[2].1, b"a");
        assert_eq!(response.attributes[3].1, b"b");

        let request = RadiusPacket::create_accounting_request(1, vec![(PROXY_STATE, b"a".to_vec()), (PROXY_STATE, b"b".to_vec())], b"s");
        let response = RadiusPacket::parse(&server.handle_acct(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.attributes, [(PROXY_STATE, b"a".to_vec()), (PROXY_STATE, b"b".to_vec())]);
    }

    #[tokio::test]
    async fn test_oversized_accept_becomes_reject() {
        let script = "for i = 1, 20 do set_attr('Filter-Id', string.rep('x', 250)) end return 'accept'";
        let (response, _) = rfc_request_response(script).await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert!(response.attribute(11).is_none());
    }

    #[tokio::test]
    async fn test_request_too_large_to_answer_is_discarded() {
        let server = script_server("s", "return 'accept'").await;
        let server = server.with_clients(clients_with("s", Policy::No, Policy::No));
        let mut attributes = vec![(PROXY_STATE, vec![0u8; 253]); 15];
        attributes.push((PROXY_STATE, vec![0u8; 249]));
        let request = RadiusPacket { code: ACCESS_REQUEST, identifier: 1, authenticator: [1; 16], attributes }.encode();
        assert_eq!(request.len(), MAX_PACKET_LEN);
        assert!(server.handle_auth(&request, LOCALHOST).await.is_none());
    }

    async fn attribute_set_by(script: &str, attr_type: u8) -> Vec<u8> {
        let (response, _) = rfc_request_response(script).await;
        assert_eq!(response.code, ACCESS_ACCEPT, "{script}");
        response.attribute(attr_type).expect("attribute missing").to_vec()
    }

    #[tokio::test]
    async fn test_tagged_attributes() {
        assert_eq!(attribute_set_by("set_attr('Tunnel-Type:1', 'VLAN') return 'accept'", 64).await, [0x01, 0, 0, 0x0D]);
        assert_eq!(attribute_set_by("set_attr('Tunnel-Type', 'VLAN') return 'accept'", 64).await, [0, 0, 0, 0x0D]);
        assert_eq!(attribute_set_by("set_attr('Tunnel-Private-Group-Id:1', '100') return 'accept'", 81).await, [0x01, 0x31, 0x30, 0x30]);
        assert_eq!(attribute_set_by("set_attr('Tunnel-Private-Group-Id', '100') return 'accept'", 81).await, [0x31, 0x30, 0x30]);
        assert_eq!(attribute_set_by("set_attr('Tunnel-Private-Group-Id', '\\5A') return 'accept'", 81).await, [0, 0x05, 0x41]);
    }

    #[tokio::test]
    async fn test_attribute_values_that_raise_errors() {
        for (name, value) in [
            ("Tunnel-Type:0", "'VLAN'"),
            ("Tunnel-Type:32", "'VLAN'"),
            ("Tunnel-Type:x", "'VLAN'"),
            ("Filter-Id:1", "'x'"),
            ("Tunnel-Password", "string.rep('x', 240)"),
            ("Login-IPv6-Host", "'not an address'"),
            ("User-Password", "'x'"),
        ] {
            let script = format!("if pcall(set_attr, '{name}', {value}) then return 'accept' end return 'reject'");
            let (response, _) = rfc_request_response(&script).await;
            assert_eq!(response.code, ACCESS_REJECT, "{name} did not raise an error");
        }
        // A value above what the three octets after a tag can hold.
        let script = "if pcall(set_attr, 'Tunnel-Type:1', 16777216) then return 'accept' end return 'reject'";
        assert_eq!(rfc_request_response(script).await.0.code, ACCESS_REJECT);
    }

    #[test]
    fn test_unsupported_encryption() {
        let mut dictionary = Dictionary::new();
        dictionary.add_attribute("Salted", "string", 0, vec![200], Some("encrypt=2".to_string()));
        dictionary.add_attribute("Ascend-Style", "string", 0, vec![201], Some("encrypt=3".to_string()));
        let value = AttrValue::Bytes(b"x".to_vec());
        assert!(encode_response_attribute(&dictionary, "Ascend-Style", &value, b"s", &[0; 16], &mut Vec::new()).is_err());
        assert!(encode_response_attribute(&dictionary, "Salted", &value, b"s", &[0; 16], &mut Vec::new()).is_ok());
    }

    #[tokio::test]
    async fn test_ipv6_attribute() {
        let expected: [u8; 16] = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        assert_eq!(attribute_set_by("set_attr('Login-IPv6-Host', '2001:db8::1') return 'accept'", 98).await, expected);
    }

    /// RFC 2868 section 3.5, written out here rather than using the code under test.
    fn decrypt_tunnel_password(value: &[u8], secret: &[u8], request_authenticator: &[u8; 16]) -> (u8, [u8; 2], Vec<u8>) {
        let (tag, salt, cipher) = (value[0], [value[1], value[2]], &value[3..]);
        assert_eq!(cipher.len() % 16, 0);
        let mut plain = Vec::new();
        let mut previous: Vec<u8> = [request_authenticator.as_slice(), salt.as_slice()].concat();
        for chunk in cipher.chunks(16) {
            let pad = Md5::digest([secret, previous.as_slice()].concat());
            plain.extend(chunk.iter().zip(pad.iter()).map(|(c, b)| c ^ b));
            previous = chunk.to_vec();
        }
        let length = plain[0] as usize;
        assert!(plain[1 + length..].iter().all(|b| *b == 0), "padding");
        (tag, salt, plain[1..1 + length].to_vec())
    }

    #[tokio::test]
    async fn test_tunnel_password() {
        let request_authenticator = RadiusPacket::parse(&hex(RFC_REQUEST)).unwrap().authenticator;
        let (response, _) = rfc_request_response("
            set_attr('Tunnel-Password:1', 'secret')
            set_attr('Tunnel-Password', string.rep('p', 239))
            return 'accept'").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        let values: Vec<&Vec<u8>> = response.attributes.iter().filter(|(t, _)| *t == 69).map(|(_, v)| v).collect();
        assert_eq!(values.len(), 2);
        let (tag, first_salt, password) = decrypt_tunnel_password(values[0], b"xyzzy5461", &request_authenticator);
        assert_eq!((tag, password.as_slice()), (1, &b"secret"[..]));
        assert_ne!(first_salt[0] & 0x80, 0, "the salt's most significant bit");
        let (tag, second_salt, password) = decrypt_tunnel_password(values[1], b"xyzzy5461", &request_authenticator);
        assert_eq!((tag, password.len()), (0, 239));
        assert_ne!(second_salt[0] & 0x80, 0);
        assert_ne!(first_salt, second_salt);
    }

    #[test]
    fn test_salts_never_repeat_in_one_response() {
        let mut dictionary = Dictionary::new();
        dictionary.add_attribute("Salted", "string", 0, vec![200], Some("encrypt=2".to_string()));
        let mut salts = Vec::new();
        for _ in 0..200 {
            encode_response_attribute(&dictionary, "Salted", &AttrValue::Bytes(b"x".to_vec()), b"s", &[0; 16], &mut salts).unwrap();
        }
        let unique: std::collections::HashSet<_> = salts.iter().collect();
        assert_eq!(unique.len(), 200);
    }

    #[test]
    fn test_too_many_salted_attributes() {
        let mut dictionary = Dictionary::new();
        dictionary.add_attribute("Salted", "string", 0, vec![200], Some("encrypt=2".to_string()));
        let value = AttrValue::Bytes(b"x".to_vec());
        let mut salts = Vec::new();
        for _ in 0..MAX_SALTED_ATTRIBUTES {
            encode_response_attribute(&dictionary, "Salted", &value, b"s", &[0; 16], &mut salts).unwrap();
        }
        assert!(encode_response_attribute(&dictionary, "Salted", &value, b"s", &[0; 16], &mut salts).is_err());
    }

    #[tokio::test]
    async fn test_message_authenticator_required_by_default() {
        let server = test_server("s").await;
        let server = server.with_clients(Arc::new(ClientTable::new(vec![("127.0.0.1", None, b"s".to_vec(), None)]).unwrap()));
        assert!(server.handle_auth(&bare_request(false), LOCALHOST).await.is_none());
        assert!(server.handle_auth(&signed_request(), LOCALHOST).await.is_some());
    }

    #[tokio::test]
    async fn test_message_authenticator_not_required() {
        let server = enforcing_server(Policy::No, Policy::Yes).await;
        assert!(server.handle_auth(&bare_request(false), LOCALHOST).await.is_some());
        assert!(server.handle_auth(&bare_request(true), LOCALHOST).await.is_none(), "Proxy-State without Message-Authenticator");

        let server = enforcing_server(Policy::No, Policy::No).await;
        assert!(server.handle_auth(&bare_request(true), LOCALHOST).await.is_some());
    }

    #[tokio::test]
    async fn test_require_auto_learns_from_first_request() {
        // First request, later request, whether the later one is processed.
        for (first, later, processed) in [
            (signed_request(), bare_request(false), false),
            (bare_request(false), bare_request(false), true),
            (bare_request(false), signed_request(), true),
        ] {
            let server = enforcing_server(Policy::Auto, Policy::Yes).await;
            assert!(server.handle_auth(&first, LOCALHOST).await.is_some());
            assert_eq!(server.handle_auth(&later, LOCALHOST).await.is_some(), processed);
        }
    }

    #[tokio::test]
    async fn test_invalid_message_authenticator_is_not_learned() {
        let server = enforcing_server(Policy::Auto, Policy::Yes).await;
        assert!(server.handle_auth(&badly_signed_request(), LOCALHOST).await.is_none());
        // Still `auto`: this first valid request decides, and has none.
        assert!(server.handle_auth(&bare_request(false), LOCALHOST).await.is_some());
        assert!(server.handle_auth(&bare_request(false), LOCALHOST).await.is_some());
    }

    #[tokio::test]
    async fn test_limit_proxy_state_auto_learns_from_first_request() {
        let server = enforcing_server(Policy::No, Policy::Auto).await;
        assert!(server.handle_auth(&bare_request(true), LOCALHOST).await.is_some(), "becomes `no`");
        assert!(server.handle_auth(&bare_request(true), LOCALHOST).await.is_some());

        let server = enforcing_server(Policy::No, Policy::Auto).await;
        assert!(server.handle_auth(&bare_request(false), LOCALHOST).await.is_some(), "becomes `yes`");
        assert!(server.handle_auth(&bare_request(true), LOCALHOST).await.is_none());

        // Proxy-State with a Message-Authenticator is the safe kind, so `yes` too.
        let server = enforcing_server(Policy::No, Policy::Auto).await;
        let signed = RadiusPacket::create_access_request(4, [5; 16], vec![(PROXY_STATE, b"a".to_vec())], b"s");
        assert!(server.handle_auth(&signed, LOCALHOST).await.is_some(), "becomes `yes`");
        assert!(server.handle_auth(&bare_request(true), LOCALHOST).await.is_none());
    }

    async fn test_server(secret: &str) -> RadiusServer {
        script_server(secret, "return 'accept'").await
    }

    async fn script_server(secret: &str, script: &str) -> RadiusServer {
        let db = crate::test_support::test_db();
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), db, script).unwrap());
        RadiusServer::new(any_addr(), any_addr(), local_clients(secret), engine, share_dictionary())
    }

    fn share_dictionary() -> Arc<Dictionary> {
        share_test_dictionary()
    }

    fn hmac_md5(secret: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = Hmac::<Md5>::new_from_slice(secret).unwrap();
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    #[test]
    fn test_message_authenticator_verification() {
        let request = RadiusPacket::create_access_request(7, [9u8; 16], vec![(1, b"nemo".to_vec())], SECRET);
        let packet = RadiusPacket::parse(&request).unwrap();
        assert_eq!(packet.verify_message_authenticator(SECRET), Some(true));
        assert_eq!(packet.verify_message_authenticator(b"wrong-secret"), Some(false));

        let without = RadiusPacket::parse(&hex(RFC_REQUEST)).unwrap();
        assert_eq!(without.verify_message_authenticator(SECRET), None);
    }

    #[tokio::test]
    async fn test_access_request_with_wrong_secret_is_discarded() {
        let server = test_server("xyzzy5461").await;
        let request = RadiusPacket::create_access_request(7, [9u8; 16], vec![(1, b"nemo".to_vec())], b"other-secret");
        assert!(server.handle_auth(&request, LOCALHOST).await.is_none());
    }

    #[tokio::test]
    async fn test_response_carries_message_authenticator() {
        let server = test_server("xyzzy5461").await;
        let request_bytes = RadiusPacket::create_access_request(7, [9u8; 16], vec![(1, b"nemo".to_vec())], SECRET);
        let response_bytes = server.handle_auth(&request_bytes, LOCALHOST).await.unwrap();
        let response = RadiusPacket::parse(&response_bytes).unwrap();
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.identifier, 7);

        // Recompute independently: HMAC-MD5 over the response with the Request
        // Authenticator in place and the Message-Authenticator value zeroed.
        let received = response.attribute(MESSAGE_AUTHENTICATOR).unwrap().to_vec();
        let mut zeroed = response_bytes.clone();
        zeroed[4..20].copy_from_slice(&[9u8; 16]);
        let ma_offset = zeroed.len() - 16;
        zeroed[ma_offset..].fill(0);
        assert_eq!(received, hmac_md5(SECRET, &zeroed));

        // And the Response Authenticator covers the final attributes.
        let mut for_auth = response_bytes.clone();
        for_auth[4..20].copy_from_slice(&[9u8; 16]);
        assert_eq!(&response_bytes[4..20], &md5_with_secret(&for_auth, SECRET));
    }

    #[test]
    fn test_accounting_request_verification() {
        let attributes = vec![(40, vec![0, 0, 0, 1]), (44, b"session-1".to_vec())]; // Acct-Status-Type Start, Acct-Session-Id
        let good = RadiusPacket::parse(&RadiusPacket::create_accounting_request(3, attributes.clone(), SECRET)).unwrap();
        assert!(good.verify_accounting_request(SECRET));
        let bad = RadiusPacket::parse(&RadiusPacket::create_accounting_request(3, attributes, b"other-secret")).unwrap();
        assert!(!bad.verify_accounting_request(SECRET));
    }

    /// The spec's table of what a record holds: one row a client, so that each finds its own.
    #[tokio::test]
    async fn test_records_of_decisions() {
        use crate::auth_chain::Handler;
        use crate::config::{AllowedNames, AuthPolicy, UsernameSettings};
        use crate::ldap_handler::tests::{mock_server, settings};
        use crate::ldap_handler::{LdapHandler, PasswordCheck};
        let lua = |script: &str| Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap());
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE decision_users (username TEXT, password TEXT)").await.unwrap();
        db.execute("INSERT INTO decision_users VALUES ('alice', 'secret')").await.unwrap();
        let sql = || Handler::Sql { db: db.clone(), user_query: "SELECT password FROM decision_users WHERE username = ?".to_string(), reply_query: None, certificate_query: None, default_user: None, timeout: Duration::from_secs(5) };
        let (url, _log) = mock_server().await;
        let ldap = Handler::Ldap(LdapHandler::new(settings(vec![url], PasswordCheck::Bind)).unwrap());
        let refuse = UsernameSettings { allowed: Some(AllowedNames::new("[a-z]+").unwrap()), ..Default::default() };
        let chain = |policy, handlers| Some(AuthChain::new(policy, handlers));
        // Client, chain, user, password, then the record's chain, handler and decision.
        #[allow(clippy::type_complexity)]
        let rows: Vec<(&str, Option<AuthChain>, &str, &str, &str, &str, &str)> = vec![
            ("row-ldap", chain(AuthPolicy::FirstMatch, vec![ldap]), "alice", "wrong", "default", "ldap", "reject"),
            ("row-challenge", chain(AuthPolicy::FirstMatch, vec![lua("set_attr('State', 'x') return 'challenge'")]), "alice", "x", "default", "lua", "challenge"),
            ("row-all", chain(AuthPolicy::AllAccept, vec![sql(), lua("return 'accept'")]), "alice", "secret", "default", "lua", "accept"),
            ("row-no-check", chain(AuthPolicy::AllAccept, vec![sql(), lua("return 'accept'")]), "nosuchuser", "x", "default", "no_password_check", "reject"),
            ("row-ignore", chain(AuthPolicy::FirstMatch, vec![lua("return 'ignore'")]), "alice", "x", "default", "none", "reject"),
            ("row-name", chain(AuthPolicy::FirstMatch, vec![lua("return 'accept'")]).map(|c| c.with_username(Some(refuse))), "A1", "x", "default", "normalization", "reject"),
            ("row-script", None, "alice", "x", "script", "script", "accept"),
        ];
        for (client, chain, user, password, chain_name, handler, decision) in rows {
            let mut clients = ClientTable::default();
            let options = crate::clients::ClientOptions { require_message_authenticator: Policy::No, ..Default::default() };
            clients.add("127.0.0.1", Some(client.to_string()), SECRET.to_vec(), options).unwrap();
            let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").unwrap());
            let mut server = RadiusServer::new(any_addr(), any_addr(), Arc::new(clients), engine, share_dictionary());
            if chain.is_some() {
                server = server.with_auth_chains(Arc::new(AuthChains::new(chain, Vec::new())));
            }
            let ra = [51u8; 16];
            let request = RadiusPacket::create_access_request(1, ra, vec![(1, user.as_bytes().to_vec()), (USER_PASSWORD, encrypt_password(password, &ra))], SECRET);
            server.handle_auth(&request, LOCALHOST).await.expect(client);
            let records = crate::logging::records_for_client(client);
            assert_eq!(records.len(), 1, "{client}: {records:?}");
            assert_eq!((records[0].chain.as_str(), records[0].handler.as_str(), records[0].decision.as_str()), (chain_name, handler, decision), "{client}");
        }
    }

    /// The names in this test differ from the spec's example only to keep them apart from
    /// other tests' records, which share one buffer.
    #[tokio::test]
    async fn test_record_of_a_chain_decision() {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE record_users (username TEXT, password TEXT)").await.unwrap();
        db.execute("INSERT INTO record_users VALUES ('rec-alice', 'secret')").await.unwrap();
        let handler = crate::auth_chain::Handler::Sql {
            db,
            user_query: "SELECT password FROM record_users WHERE username = ?".to_string(),
            reply_query: None,
            certificate_query: None, default_user: None,
            timeout: Duration::from_secs(5),
        };
        let username = crate::config::UsernameSettings { strip_realm: crate::config::StripRealm::Suffix, lowercase: true, allowed: None };
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![handler]).with_username(Some(username));
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'reject'").unwrap());
        let mut clients = ClientTable::default();
        let options = crate::clients::ClientOptions { auth: Some("vpn".to_string()), require_message_authenticator: Policy::No, ..Default::default() };
        clients.add("10.0.0.1", Some("nas1".to_string()), SECRET.to_vec(), options).unwrap();
        let server = RadiusServer::new(any_addr(), any_addr(), Arc::new(clients), engine, share_dictionary())
            .with_auth_chains(Arc::new(AuthChains::new(None, [("vpn".to_string(), chain)])));
        let ra = [21u8; 16];
        let request = RadiusPacket::create_access_request(1, ra, vec![(1, b"Rec-Alice@Corp".to_vec()), (USER_PASSWORD, encrypt_password("secret", &ra))], SECRET);
        assert_eq!(RadiusPacket::parse(&server.handle_auth(&request, "10.0.0.1".parse().unwrap()).await.unwrap()).unwrap().code, ACCESS_ACCEPT);
        assert_eq!(crate::logging::records_for("Rec-Alice@Corp"), vec![crate::logging::OwnedRecord {
            protocol: "radius".to_string(),
            client: "nas1".to_string(),
            source: "10.0.0.1".parse().unwrap(),
            user: "Rec-Alice@Corp".to_string(),
            account: "rec-alice".to_string(),
            chain: "vpn".to_string(),
            handler: "sql".to_string(),
            decision: "accept".to_string(),
        }]);
    }

    #[tokio::test]
    async fn test_record_of_a_script_decision() {
        let name = |user: &str, id: u8| RadiusPacket::create_access_request(id, [id; 16], vec![(1, user.as_bytes().to_vec())], SECRET);
        let server = script_server("xyzzy5461", "if get_attr('User-Name') == 'rec-chal' then set_attr('State', 'x') return 'challenge' end return get_attr('User-Name') == 'rec-yes' and 'accept' or 'reject'").await;
        for (user, decision) in [("rec-yes", "accept"), ("rec-no", "reject"), ("rec-chal", "challenge")] {
            server.handle_auth(&name(user, 40), LOCALHOST).await.unwrap();
            let records = crate::logging::records_for(user);
            assert_eq!(records.len(), 1, "{records:?}");
            assert_eq!((records[0].chain.as_str(), records[0].handler.as_str(), records[0].decision.as_str()), ("script", "script", decision));
            assert_eq!(records[0].account, user);
        }
    }

    #[tokio::test]
    async fn test_record_of_a_decision_that_cannot_be_sent() {
        let server = script_server("xyzzy5461", "set_attr('Filter-Id', 'x') return 'challenge'").await;
        let request = RadiusPacket::create_access_request(41, [41; 16], vec![(1, b"rec-bad-challenge".to_vec())], SECRET);
        assert_eq!(RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap().code, ACCESS_REJECT);
        assert_eq!(crate::logging::records_for("rec-bad-challenge")[0].decision, "reject");
    }

    #[tokio::test]
    async fn test_no_record_without_a_decision() {
        let server = script_server("xyzzy5461", "return 'accept'").await;
        // Fails verification.
        let forged = RadiusPacket::create_access_request(42, [42; 16], vec![(1, b"rec-forged".to_vec())], b"other-secret");
        assert!(server.handle_auth(&forged, LOCALHOST).await.is_none());
        assert!(crate::logging::records_for("rec-forged").is_empty());
        // Unknown client.
        let unknown = RadiusPacket::create_access_request(43, [43; 16], vec![(1, b"rec-unknown".to_vec())], SECRET);
        assert!(server.handle_auth(&unknown, "192.0.2.77".parse().unwrap()).await.is_none());
        assert!(crate::logging::records_for("rec-unknown").is_empty());
        // Answered from the duplicate cache: one record for two copies.
        let twice = RadiusPacket::create_access_request(44, [44; 16], vec![(1, b"rec-twice".to_vec())], SECRET);
        let source = "127.0.0.1:1812".parse().unwrap();
        let first = server.receive(PacketKind::Auth, &twice, source).await;
        let second = server.receive(PacketKind::Auth, &twice, source).await;
        assert_eq!(first, second);
        assert_eq!(crate::logging::records_for("rec-twice").len(), 1);
    }

    #[tokio::test]
    async fn test_record_without_a_name() {
        let server = script_server("xyzzy5461", "return get_attr('Reply-Message') == 'rec-noname' and 'accept' or 'reject'").await;
        let request = RadiusPacket::create_access_request(45, [45; 16], vec![(18, b"rec-noname".to_vec())], SECRET);
        server.handle_auth(&request, LOCALHOST).await.unwrap();
        let records = crate::logging::records();
        let records = records.lock().unwrap();
        assert!(records.iter().any(|record| record.user.is_empty() && record.account.is_empty() && record.decision == "accept"), "{records:?}");
    }

    fn eap_rig(script: &str) -> (tempfile::TempDir, Arc<EapSettings>, Arc<LuaAuthEngine>) {
        let dir = tempfile::tempdir().unwrap();
        let issuer = crate::test_support::certs::ca(dir.path(), "ca");
        crate::test_support::certs::leaf(dir.path(), "server", &issuer);
        crate::test_support::certs::named_leaf(dir.path(), "alice", "alice.example.com", 20, &issuer);
        let settings = Arc::new(crate::eap::tests::eap_settings(dir.path(), None));
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap());
        (dir, settings, engine)
    }

    fn eap_server(settings: &Arc<EapSettings>, engine: &Arc<LuaAuthEngine>) -> RadiusServer {
        RadiusServer::new(any_addr(), any_addr(), local_clients(std::str::from_utf8(SECRET).unwrap()), engine.clone(), share_dictionary()).with_eap(Some(settings.clone()))
    }

    fn eap_request(identifier: u8, eap: &[u8], state: Option<&[u8]>) -> Vec<u8> {
        let mut attributes = vec![(1, b"alice.example.com".to_vec())];
        attributes.extend(split_message(eap));
        attributes.extend(state.map(|state| (STATE, state.to_vec())));
        RadiusPacket::create_access_request(identifier, [identifier; 16], attributes, SECRET)
    }

    fn joined(packet: &RadiusPacket) -> Vec<u8> {
        join_message(packet.attributes.iter().filter(|(kind, _)| *kind == EAP_MESSAGE).map(|(_, value)| value.as_slice()))
    }

    /// A whole conversation through `handle_auth`, returning the last RADIUS response and the
    /// authenticator of the request it answers.
    async fn eap_conversation(server: &RadiusServer, dir: &std::path::Path, identity: &[u8], version: &'static rustls::SupportedProtocolVersion) -> (RadiusPacket, [u8; 16], crate::eap::tests::Peer) {
        let mut peer = crate::eap::tests::Peer::new(crate::eap::tests::client_connection(dir, Some("alice"), version));
        let mut identifier = 1u8;
        let mut request = eap_request(identifier, &crate::eap::tests::identity_response(1, identity), None);
        for _ in 0..64 {
            let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.expect("no response")).unwrap();
            if response.code != ACCESS_CHALLENGE {
                return (response, [identifier; 16], peer);
            }
            identifier += 1;
            let answer = peer.answer(&joined(&response));
            request = eap_request(identifier, &answer, response.attribute(STATE));
        }
        panic!("the conversation did not end");
    }

    #[tokio::test]
    async fn test_eap_tls_asks_ocsp_before_authorizing() -> Result<(), String> {
        use rustls::pki_types::pem::PemObject;
        for (status, code) in [(0, ACCESS_ACCEPT), (1, ACCESS_REJECT)] {
            let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
            crate::test_support::write_pki(dir.path())?;
            let alice = rustls::pki_types::CertificateDer::from_pem_file(dir.path().join("alice.pem")).map_err(|e| e.to_string())?;
            // Status 1: the revocation check refuses alice's certificate.
            let check = Arc::new(crate::test_support::FakeRevocation::default());
            if status == 1 {
                check.revoke(&alice);
            }
            let settings = Arc::new(EapSettings { revocation: Some(check), ..crate::eap::tests::eap_settings(dir.path(), None) });
            let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'")?);
            let server = eap_server(&settings, &engine);
            let (response, _, _) = eap_conversation(&server, dir.path(), b"alice.example.com", &rustls::version::TLS12).await;
            assert_eq!(response.code, code, "status {status}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_eap_without_the_table_is_an_ordinary_attribute() {
        let (_dir, _, engine) = eap_rig("return get_attr('EAP-Message') and 'accept' or 'reject'");
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients(std::str::from_utf8(SECRET).unwrap()), engine, share_dictionary());
        let request = eap_request(1, &crate::eap::tests::identity_response(1, b"a"), None);
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_ACCEPT, "the script ran");
        assert!(response.attribute(EAP_MESSAGE).is_none());
    }

    #[tokio::test]
    async fn test_eap_needs_a_message_authenticator() {
        let (_dir, settings, engine) = eap_rig("return 'accept'");
        let clients = {
            let mut table = ClientTable::default();
            let options = crate::clients::ClientOptions { require_message_authenticator: MessageAuthenticatorPolicy::No, ..crate::clients::ClientOptions::default() };
            table.add("127.0.0.1", None, SECRET.to_vec(), options).unwrap();
            table
        };
        let server = RadiusServer::new(any_addr(), any_addr(), Arc::new(clients), engine, share_dictionary()).with_eap(Some(settings));
        let packet = RadiusPacket { code: ACCESS_REQUEST, identifier: 1, authenticator: [1; 16], attributes: split_message(&crate::eap::tests::identity_response(1, b"a")) };
        assert!(server.handle_auth(&packet.encode(), LOCALHOST).await.is_none());
        assert_eq!(server.metrics.eap_open.get(), 0);
    }

    #[tokio::test]
    async fn test_eap_start_over_radius() {
        let (_dir, settings, engine) = eap_rig("return 'accept'");
        let server = eap_server(&settings, &engine);
        let request = RadiusPacket::create_access_request(1, [1; 16], vec![(EAP_MESSAGE, Vec::new())], SECRET);
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_CHALLENGE);
        assert_eq!(joined(&response)[4], crate::eap::TYPE_IDENTITY);
        assert!(response.attribute(MESSAGE_AUTHENTICATOR).is_some());
    }

    #[tokio::test]
    async fn test_eap_tls_over_radius_gives_keys_and_reply_attributes() {
        let script = "if eap_method == 'tls' and tls_subject and get_attr('User-Name') == 'alice.example.com' then set_attr('Filter-Id', 'staff') return 'accept' end return 'reject'";
        let (dir, settings, engine) = eap_rig(script);
        for (version, label, context) in [
            (&rustls::version::TLS12, &b"client EAP encryption"[..], None),
            (&rustls::version::TLS13, &b"EXPORTER_EAP_TLS_Key_Material"[..], Some(&[crate::eap::TYPE_TLS][..])),
        ] {
            let server = eap_server(&settings, &engine);
            let (response, authenticator, peer) = eap_conversation(&server, dir.path(), b"alice.example.com", version).await;
            assert_eq!(response.code, ACCESS_ACCEPT);
            assert_eq!(joined(&response)[0], 3, "EAP-Success");
            assert!(response.attribute(MESSAGE_AUTHENTICATOR).is_some());
            assert_eq!(response.attribute(11), Some(&b"staff"[..]), "what the script returned");
            let mut theirs = [0u8; 128];
            peer.client.export_keying_material(&mut theirs[..], label, context).unwrap();
            let (_, recv) = decrypt_mppe_key(&microsoft_attribute(&response, 17).expect("no receive key"), &authenticator);
            let (_, send) = decrypt_mppe_key(&microsoft_attribute(&response, 16).expect("no send key"), &authenticator);
            assert_eq!(recv, theirs[..32]);
            assert_eq!(send, theirs[32..64]);
            assert_eq!(server.metrics.eap_tls.accept.get(), 1);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_eap_tls_repeat_during_the_chain_gets_no_rejection() {
        const SLOW: &str = "sql_query('WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 3000000) SELECT count(*) AS n FROM c')";
        for (decision, code) in [("accept", ACCESS_ACCEPT), ("reject", ACCESS_REJECT)] {
            let (dir, settings, _) = eap_rig("return 'accept'");
            let script = format!("{SLOW} return '{decision}'");
            let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), &script).unwrap().with_query_timeout(Duration::from_secs(120)));
            let server = Arc::new(eap_server(&settings, &engine));
            let mut peer = crate::eap::tests::Peer::new(crate::eap::tests::client_connection(dir.path(), Some("alice"), &rustls::version::TLS12));
            let mut identifier = 1u8;
            let (mut eap, mut state) = (crate::eap::tests::identity_response(1, b"alice.example.com"), None::<Vec<u8>>);
            let decided = loop {
                // Each round with a repeat in a new RADIUS packet, the last one while the chain decides.
                let request = eap_request(identifier, &eap, state.as_deref());
                let repeat = eap_request(identifier + 100, &eap, state.as_deref());
                let answering = tokio::spawn({
                    let server = server.clone();
                    async move { server.handle_auth(&request, LOCALHOST).await }
                });
                tokio::time::sleep(Duration::from_millis(5)).await;
                // Without a State it would be a new conversation.
                let repeated = if state.is_some() { server.handle_auth(&repeat, LOCALHOST).await } else { None };
                let response = RadiusPacket::parse(&answering.await.unwrap().expect("no response")).unwrap();
                if response.code != ACCESS_CHALLENGE {
                    assert!(repeated.is_none(), "{decision}: the repeat during the chain got an answer");
                    break response;
                }
                if let Some(repeated) = repeated {
                    assert_eq!(RadiusPacket::parse(&repeated).unwrap().code, ACCESS_CHALLENGE, "{decision}: a repeat was refused");
                }
                identifier += 1;
                eap = peer.answer(&joined(&response));
                state = response.attribute(STATE).map(<[u8]>::to_vec);
            };
            assert_eq!(decided.code, code, "{decision}");
            assert_eq!(server.metrics.eap_open.get(), 0, "{decision}: the conversation ended");
            let counted = if decision == "accept" { &server.metrics.eap_tls.accept } else { &server.metrics.eap_tls.reject };
            assert_eq!(counted.get(), 1, "{decision}");
        }
    }

    #[tokio::test]
    async fn test_eap_log_holds_no_keys_or_tls_content() {
        let log = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).with_writer(log.clone()).finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        // With a single subscriber in the process, tracing settles a call site by asking whichever
        // thread reaches it first, and in a parallel test run that thread has none: the call site
        // would then stay silent here. A second one makes it ask every subscriber.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let (dir, settings, engine) = eap_rig("return 'accept'");
        let server = eap_server(&settings, &engine);
        let (accepted, _, peer) = eap_conversation(&server, dir.path(), b"alice.example.com", &rustls::version::TLS13).await;
        assert_eq!(accepted.code, ACCESS_ACCEPT);
        let (refused, _, _) = eap_conversation(&server, dir.path(), b"bob.example.com", &rustls::version::TLS13).await;
        assert_eq!(refused.code, ACCESS_REJECT);

        let log = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        // The capture works: both endings are in it.
        assert!(log.contains("alice.example.com"), "{log}");
        assert!(log.contains("refused"), "{log}");
        let mut key = [0u8; 128];
        peer.client.export_keying_material(&mut key[..], b"EXPORTER_EAP_TLS_Key_Material", Some(&[crate::eap::TYPE_TLS][..])).unwrap();
        let server_certificate = peer.client.peer_certificates().unwrap()[0].to_vec();
        for (what, secret) in [("key", &key[..16]), ("certificate from the handshake", &server_certificate[..16])] {
            let hex: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
            let listed = format!("{:?}", secret);
            assert!(!log.contains(&hex), "{what} in the log as hex");
            assert!(!log.contains(listed.trim_matches(['[', ']'])), "{what} in the log as a list");
        }
    }

    #[tokio::test]
    async fn test_eap_identity_does_not_fit() {
        // Would accept: it must not run.
        let (dir, settings, engine) = eap_rig("return 'accept'");
        let server = eap_server(&settings, &engine);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"bob.example.com", &rustls::version::TLS12).await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert_eq!(joined(&response)[0], 4, "EAP-Failure");
        assert_eq!(microsoft_attribute(&response, 17), None, "no keys on a refusal");
        assert_eq!(server.metrics.eap_tls.reject.get(), 1);
    }

    #[tokio::test]
    async fn test_eap_certificate_with_unreadable_alternative_names() {
        let dir = tempfile::tempdir().unwrap();
        let issuer = crate::test_support::certs::ca(dir.path(), "ca");
        crate::test_support::certs::leaf(dir.path(), "server", &issuer);
        crate::test_support::certs::leaf_with_unreadable_alternative_names(dir.path(), "alice", "alice", 21, &issuer);
        let settings = Arc::new(crate::eap::tests::eap_settings(dir.path(), None));
        // Would accept: it must not run.
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").unwrap());
        let server = eap_server(&settings, &engine);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"alice", &rustls::version::TLS12).await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert_eq!(joined(&response)[0], 4, "EAP-Failure");
        assert_eq!(microsoft_attribute(&response, 17), None, "no keys on a refusal");
        let logged = crate::test_support::logged_errors().lock().unwrap().clone();
        assert!(logged.iter().any(|line| line.contains("refused") && line.contains("alternative names")), "it got as far as the names: {logged:?}");
        assert_eq!(server.metrics.eap_tls.reject.get(), 1);
    }

    /// `eap_request` with this much Proxy-State.
    fn eap_request_with_proxy_state(identifier: u8, eap: &[u8], state: Option<&[u8]>, proxy_state: usize) -> Vec<u8> {
        let mut attributes = vec![(1, b"alice.example.com".to_vec())];
        attributes.extend(split_message(eap));
        attributes.extend(state.map(|state| (STATE, state.to_vec())));
        attributes.extend(vec![0u8; proxy_state].chunks(253).map(|chunk| (PROXY_STATE, chunk.to_vec())));
        RadiusPacket::create_access_request(identifier, [identifier; 16], attributes, SECRET)
    }

    #[tokio::test]
    async fn test_eap_response_over_the_limit_ends_the_conversation() {
        let (dir, settings, engine) = eap_rig("return 'accept'");
        let server = eap_server(&settings, &engine);
        let mut peer = crate::eap::tests::Peer::new(crate::eap::tests::client_connection(dir.path(), Some("alice"), &rustls::version::TLS12));
        let (mut eap, mut state) = (crate::eap::tests::identity_response(1, b"alice.example.com"), None::<Vec<u8>>);
        let mut identifier = 1u8;
        let response = loop {
            let request = eap_request_with_proxy_state(identifier, &eap, state.as_deref(), 3700);
            assert!(request.len() <= MAX_PACKET_LEN, "{}", request.len());
            let sent = server.handle_auth(&request, LOCALHOST).await.expect("no response");
            assert!(sent.len() <= MAX_PACKET_LEN, "a response of {} octets", sent.len());
            let response = RadiusPacket::parse(&sent).unwrap();
            if response.code != ACCESS_CHALLENGE {
                break response;
            }
            identifier += 1;
            eap = peer.answer(&joined(&response));
            state = response.attribute(STATE).map(<[u8]>::to_vec);
        };
        assert_eq!(response.code, ACCESS_REJECT);
        assert_eq!(joined(&response)[0], 4, "EAP-Failure");
        assert_eq!(joined(&response)[1], eap[1], "it answers the Response it got");
        assert!(response.attribute(PROXY_STATE).is_some());
        assert_eq!(server.metrics.eap_open.get(), 0);
        assert_eq!(server.metrics.eap_tls.reject.get(), 1);
    }

    #[tokio::test]
    async fn test_eap_response_over_the_limit_without_room_for_a_failure() {
        let (_dir, settings, engine) = eap_rig("return 'accept'");
        let server = eap_server(&settings, &engine);
        // An empty EAP-Message asks to start, and the shortest request fills what remains.
        let base = RadiusPacket::create_access_request(1, [1; 16], vec![(EAP_MESSAGE, Vec::new())], SECRET).len();
        let mut left = MAX_PACKET_LEN - base;
        let mut attributes = vec![(EAP_MESSAGE, Vec::new())];
        while left > 0 {
            let length = if left > 255 && left - 255 < 2 { 200 } else { left.min(255) };
            attributes.push((PROXY_STATE, vec![0u8; length - 2]));
            left -= length;
        }
        let request = RadiusPacket::create_access_request(1, [1; 16], attributes, SECRET);
        assert_eq!(request.len(), MAX_PACKET_LEN);
        let sent = server.handle_auth(&request, LOCALHOST).await.expect("no response");
        assert!(sent.len() <= MAX_PACKET_LEN, "a response of {} octets", sent.len());
        let response = RadiusPacket::parse(&sent).unwrap();
        assert_eq!(response.code, ACCESS_REJECT);
        assert!(response.attribute(EAP_MESSAGE).is_none());
        assert_eq!(server.metrics.eap_open.get(), 0);
    }

    #[tokio::test]
    async fn test_wipe_keys_after_encoding_a_reply() {
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").unwrap());
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients("secret"), engine, share_dictionary());
        let mut attributes = vec![
            ("MS-MPPE-Send-Key".to_string(), AttrValue::Bytes(vec![7; 16])),
            ("MS-MPPE-Recv-Key".to_string(), AttrValue::Bytes(vec![8; 16])),
            ("Filter-Id".to_string(), AttrValue::Bytes(b"staff".to_vec())),
        ];
        let authenticator = [1; 16];
        let encoded = server.encode_reply(&mut attributes, SECRET, &authenticator, &mut Vec::new()).unwrap();
        assert_eq!(attributes[0].1, AttrValue::Bytes(Vec::new()), "overwritten, and emptied");
        assert_eq!(attributes[1].1, AttrValue::Bytes(Vec::new()), "overwritten, and emptied");
        assert_eq!(attributes[2].1, AttrValue::Bytes(b"staff".to_vec()));
        // What goes out still holds them.
        let sent = RadiusPacket { code: ACCESS_ACCEPT, identifier: 1, authenticator, attributes: encoded };
        assert_eq!(decrypt_mppe_key(&microsoft_attribute(&sent, 16).expect("no send key"), &authenticator).1, vec![7; 16]);
        assert_eq!(decrypt_mppe_key(&microsoft_attribute(&sent, 17).expect("no receive key"), &authenticator).1, vec![8; 16]);
        assert_eq!(sent.attribute(11), Some(&b"staff"[..]));
    }

    #[test]
    fn test_wipe_keys_only_touches_keys() {
        let mut attributes = vec![
            ("MS-MPPE-Send-Key".to_string(), AttrValue::Bytes(vec![7; 32])),
            ("MS-MPPE-Recv-Key".to_string(), AttrValue::Bytes(vec![8; 32])),
            ("Filter-Id".to_string(), AttrValue::Bytes(b"staff".to_vec())),
            ("Session-Timeout".to_string(), AttrValue::Number(60.0)),
        ];
        crate::auth_chain::wipe_keys(&mut attributes);
        for key in &attributes[..2] {
            assert_eq!(key.1, AttrValue::Bytes(Vec::new()), "overwritten, and emptied");
        }
        assert_eq!(attributes[2].1, AttrValue::Bytes(b"staff".to_vec()));
        assert_eq!(attributes[3].1, AttrValue::Number(60.0));
    }

    #[tokio::test]
    async fn test_eap_gives_the_device_the_certificate_name() {
        let (dir, mut settings, engine) = eap_rig("return 'accept'");
        Arc::get_mut(&mut settings).unwrap().match_identity = false;
        let server = eap_server(&settings, &engine);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"anonymous", &rustls::version::TLS12).await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attribute(1), Some(&b"alice.example.com"[..]));

        // What a script sets stays, and comes once.
        let (dir, mut settings, engine) = eap_rig("set_attr('User-Name', 'staff-1') return 'accept'");
        Arc::get_mut(&mut settings).unwrap().match_identity = false;
        let server = eap_server(&settings, &engine);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"anonymous", &rustls::version::TLS12).await;
        assert_eq!(response.attributes.iter().filter(|(kind, _)| *kind == 1).map(|(_, value)| value.as_slice()).collect::<Vec<_>>(), vec![&b"staff-1"[..]]);
    }

    #[tokio::test]
    async fn test_eap_script_sees_the_name_as_in_the_certificate() {
        let (dir, settings, engine) = eap_rig("return get_attr('User-Name') == 'alice.example.com' and 'accept' or 'reject'");
        let server = eap_server(&settings, &engine);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"ALICE.example.com", &rustls::version::TLS12).await;
        assert_eq!(response.code, ACCESS_ACCEPT);
    }

    fn eap_chain_server(settings: &Arc<EapSettings>, engine: &Arc<LuaAuthEngine>, handlers: Vec<crate::auth_chain::Handler>) -> RadiusServer {
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, handlers);
        eap_server(settings, engine).with_auth_chains(Arc::new(AuthChains::new(Some(chain), Vec::new())))
    }

    #[tokio::test]
    async fn test_eap_through_a_chain() {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE eap_chain_users (username TEXT, password TEXT)").await.unwrap();
        db.execute("INSERT INTO eap_chain_users VALUES ('alice.example.com', 'unused')").await.unwrap();
        db.execute("CREATE TABLE eap_chain_reply (username TEXT, attribute TEXT, value TEXT)").await.unwrap();
        db.execute("INSERT INTO eap_chain_reply VALUES ('alice.example.com', 'Tunnel-Private-Group-Id', '100')").await.unwrap();
        let sql = |user_query: &str| crate::auth_chain::Handler::Sql {
            db: db.clone(),
            user_query: user_query.to_string(),
            reply_query: Some("SELECT attribute, value FROM eap_chain_reply WHERE username = ?".to_string()),
            certificate_query: None, default_user: None,
            timeout: Duration::from_secs(5),
        };
        // The top-level script would say otherwise each time.
        let (dir, settings, engine) = eap_rig("return 'reject'");
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let server = eap_chain_server(&settings, &engine, vec![sql("SELECT password FROM eap_chain_users WHERE username = ?")]);
            let (response, _, _) = eap_conversation(&server, dir.path(), b"ALICE.example.com", version).await;
            assert_eq!(response.code, ACCESS_ACCEPT);
            assert_eq!(joined(&response)[0], 3);
            assert_eq!(response.attribute(81), Some(&b"100"[..]));
            assert_eq!(response.attribute(1), Some(&b"alice.example.com"[..]));
            assert!(microsoft_attribute(&response, 17).is_some());
        }
        let (dir, settings, engine) = eap_rig("return 'accept'");
        let server = eap_chain_server(&settings, &engine, vec![sql("SELECT password FROM eap_chain_users WHERE username = ? AND 1 = 0")]);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"alice.example.com", &rustls::version::TLS12).await;
        assert_eq!(response.code, ACCESS_REJECT, "the chain does not find the name");
        assert_eq!(joined(&response)[0], 4);
        assert_eq!(response.attribute(81), None);
        assert_eq!(microsoft_attribute(&response, 17), None);
    }

    #[tokio::test]
    async fn test_eap_lua_handler_in_a_chain() {
        let lua = |script: &str| crate::auth_chain::Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap());
        let (dir, settings, engine) = eap_rig("return 'reject'");
        let server = eap_chain_server(&settings, &engine, vec![lua("if eap_method == 'tls' and tls_subject then set_attr('Filter-Id', 'by-lua') return 'accept' end return 'reject'")]);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"alice.example.com", &rustls::version::TLS12).await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attribute(11), Some(&b"by-lua"[..]));
        let server = eap_chain_server(&settings, &engine, vec![lua("return 'reject'")]);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"alice.example.com", &rustls::version::TLS12).await;
        assert_eq!(response.code, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_plain_request_has_no_certificate() {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE eap_plain_users (username TEXT, password TEXT)").await.unwrap();
        db.execute("INSERT INTO eap_plain_users VALUES ('alice.example.com', 'secret')").await.unwrap();
        let (_dir, settings, engine) = eap_rig("return eap_method and 'accept' or tls_subject and 'accept' or 'reject'");
        let attributes = |ra: &[u8; 16]| vec![(1, b"alice.example.com".to_vec()), (USER_PASSWORD, encrypt_password("wrong", ra)), (CALLING_STATION_ID, b"aa-bb".to_vec())];
        // A script hears of no EAP method or certificate.
        let server = eap_server(&settings, &engine);
        let ra = [71u8; 16];
        let request = RadiusPacket::create_access_request(71, ra, attributes(&ra), SECRET);
        assert_eq!(RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap().code, ACCESS_REJECT);
        // A sql handler still wants the password, though the name is one it would accept by certificate.
        let handler = crate::auth_chain::Handler::Sql { db, user_query: "SELECT password FROM eap_plain_users WHERE username = ?".to_string(), reply_query: None, certificate_query: None, default_user: None, timeout: Duration::from_secs(5) };
        let server = eap_chain_server(&settings, &engine, vec![handler]);
        let ra = [72u8; 16];
        let request = RadiusPacket::create_access_request(72, ra, attributes(&ra), SECRET);
        assert_eq!(RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap().code, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_eap_one_record_per_conversation() {
        let (dir, settings, engine) = eap_rig("return 'accept'");
        let mut clients = ClientTable::default();
        clients.add("127.0.0.1", Some("eap-record".to_string()), SECRET.to_vec(), crate::clients::ClientOptions::default()).unwrap();
        let server = RadiusServer::new(any_addr(), any_addr(), Arc::new(clients), engine, share_dictionary()).with_eap(Some(settings));
        let (response, _, _) = eap_conversation(&server, dir.path(), b"alice.example.com", &rustls::version::TLS13).await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        let records = crate::logging::records_for_client("eap-record");
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!((records[0].chain.as_str(), records[0].handler.as_str(), records[0].decision.as_str()), ("eap", "eap-tls", "accept"));
    }

    #[tokio::test]
    async fn test_eap_record_only_when_a_conversation_ends() {
        let (dir, settings, engine) = eap_rig("return 'accept'");
        let mut clients = ClientTable::default();
        clients.add("127.0.0.1", Some("eap-no-record".to_string()), SECRET.to_vec(), crate::clients::ClientOptions::default()).unwrap();
        let server = RadiusServer::new(any_addr(), any_addr(), Arc::new(clients), engine, share_dictionary()).with_eap(Some(settings));
        let response = |request: Vec<u8>| { let server = &server; async move { RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap() } };
        let identity = crate::eap::tests::identity_response(1, b"alice.example.com");
        // No such conversation: refused, and nothing to record.
        let stranger = response(eap_request(1, &identity, Some(&[9u8; 16]))).await;
        assert_eq!((stranger.code, joined(&stranger)[0]), (ACCESS_REJECT, 4));
        assert_eq!(crate::logging::records_for_client("eap-no-record").len(), 0);
        // A conversation under way, which then ends badly: one record.
        let first = response(eap_request(2, &identity, None)).await;
        assert_eq!(first.code, ACCESS_CHALLENGE);
        assert_eq!(response(eap_request(3, &identity, Some(&[8u8; 16]))).await.code, ACCESS_REJECT);
        assert_eq!(crate::logging::records_for_client("eap-no-record").len(), 0);
        let wrong_type = crate::eap::EapPacket { code: 2, id: joined(&first)[1], body: vec![crate::eap::TYPE_PEAP, 0] }.encode();
        assert_eq!(response(eap_request(4, &wrong_type, first.attribute(STATE))).await.code, ACCESS_REJECT);
        let records = crate::logging::records_for_client("eap-no-record");
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!((records[0].handler.as_str(), records[0].decision.as_str()), ("eap-tls", "reject"));
        let _ = dir;
    }

    fn peap_request(identifier: u8, outer: &[u8], eap: &[u8], state: Option<&[u8]>) -> Vec<u8> {
        let mut attributes = vec![(1, outer.to_vec())];
        attributes.extend(split_message(eap));
        attributes.extend(state.map(|state| (STATE, state.to_vec())));
        RadiusPacket::create_access_request(identifier, [identifier; 16], attributes, SECRET)
    }

    /// A PEAP conversation over RADIUS: the last response, its request's authenticator, the
    /// peer, and what the peer heard inside the tunnel.
    async fn peap_conversation(server: &RadiusServer, dir: &std::path::Path, outer: &[u8], name: &str, password: &str) -> (RadiusPacket, [u8; 16], crate::eap::tests::Peer, Vec<String>) {
        let heard = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut peer = crate::eap::tests::peap_peer(dir, crate::eap::tests::mschap_inner(name, password, heard.clone()));
        let mut identifier = 1u8;
        let mut request = peap_request(identifier, outer, &crate::eap::tests::identity_response(1, outer), None);
        for _ in 0..64 {
            let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.expect("no response")).unwrap();
            if response.code != ACCESS_CHALLENGE {
                let heard = heard.lock().clone();
                return (response, [identifier; 16], peer, heard);
            }
            identifier += 1;
            let answer = peer.answer(&joined(&response));
            request = peap_request(identifier, outer, &answer, response.attribute(STATE));
        }
        panic!("the conversation did not end");
    }

    /// A server whose only client, under this name, has a `sql` chain over `users`.
    async fn peap_server(client: &str, table: &str, script: &str) -> (tempfile::TempDir, RadiusServer) {
        let db = crate::test_support::test_db();
        db.execute(&format!("CREATE TABLE {table}_users (username TEXT, password TEXT)")).await.unwrap();
        db.execute(&format!("INSERT INTO {table}_users VALUES ('alice', 'secret')")).await.unwrap();
        db.execute(&format!("CREATE TABLE {table}_reply (username TEXT, attribute TEXT, value TEXT)")).await.unwrap();
        db.execute(&format!("INSERT INTO {table}_reply VALUES ('alice', 'Tunnel-Private-Group-Id', '100')")).await.unwrap();
        let handler = crate::auth_chain::Handler::Sql {
            db,
            user_query: format!("SELECT password FROM {table}_users WHERE username = ?"),
            certificate_query: None,
            reply_query: Some(format!("SELECT attribute, value FROM {table}_reply WHERE username = ?")),
            default_user: None,
            timeout: Duration::from_secs(5),
        };
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![handler]);
        peap_server_with(client, script, Some(chain))
    }

    fn peap_server_with(client: &str, script: &str, chain: Option<AuthChain>) -> (tempfile::TempDir, RadiusServer) {
        let (dir, _, engine) = eap_rig(script);
        let settings = Arc::new(crate::eap::tests::peap_settings(dir.path()));
        let mut clients = ClientTable::default();
        clients.add("127.0.0.1", Some(client.to_string()), SECRET.to_vec(), crate::clients::ClientOptions::default()).unwrap();
        let mut server = RadiusServer::new(any_addr(), any_addr(), Arc::new(clients), engine, share_dictionary()).with_eap(Some(settings));
        if let Some(chain) = chain {
            server = server.with_auth_chains(Arc::new(AuthChains::new(Some(chain), Vec::new())));
        }
        (dir, server)
    }

    #[tokio::test]
    async fn test_peap_right_password() {
        let (dir, server) = peap_server("peap-right", "peap_right", "return 'reject'").await;
        let (response, authenticator, peer, heard) = peap_conversation(&server, dir.path(), b"anonymous", "alice", "secret").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(joined(&response)[0], 3, "EAP-Success");
        assert_eq!(heard, vec!["success".to_string(), "tlv 1".to_string()]);
        let mut theirs = [0u8; 64];
        peer.client.export_keying_material(&mut theirs[..], b"client EAP encryption", None).unwrap();
        let (_, recv) = decrypt_mppe_key(&microsoft_attribute(&response, 17).expect("no receive key"), &authenticator);
        let (_, send) = decrypt_mppe_key(&microsoft_attribute(&response, 16).expect("no send key"), &authenticator);
        assert_eq!(recv, theirs[..32], "from the tunnel");
        assert_eq!(send, theirs[32..64], "from the tunnel");
        assert_eq!(server.metrics.eap_peap.accept.get(), 1);
        assert_eq!(server.metrics.eap_tls.accept.get(), 0);
        assert_eq!(server.metrics.eap_open.get(), 0);
    }

    #[tokio::test]
    async fn test_peap_wrong_password() {
        let (dir, server) = peap_server("peap-wrong", "peap_wrong", "return 'accept'").await;
        let (response, _, _, heard) = peap_conversation(&server, dir.path(), b"anonymous", "alice", "wrong").await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert_eq!(joined(&response)[0], 4, "EAP-Failure");
        assert_eq!(microsoft_attribute(&response, 17), None);
        assert_eq!(heard.len(), 2, "{heard:?}");
        assert!(heard[0].starts_with("E=691 R=0"), "{heard:?}");
        assert_eq!(heard[1], "tlv 2");
        assert_eq!(server.metrics.eap_peap.reject.get(), 1);
        let records = crate::logging::records_for_client("peap-wrong");
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!((records[0].account.as_str(), records[0].handler.as_str(), records[0].decision.as_str()), ("alice", "eap-peap", "reject"));
    }

    /// A peer of PEAP that says what `lie` makes of its honest answer.
    fn lying_inner(password: &str, lie: fn(&[u8], Vec<u8>) -> Vec<u8>) -> crate::eap::tests::Inner {
        let mut honest = crate::eap::tests::mschap_inner("alice", password, Arc::new(parking_lot::Mutex::new(Vec::new())));
        Box::new(move |message: &[u8]| {
            let reply = honest(message);
            lie(message, reply)
        })
    }

    #[tokio::test]
    async fn test_peap_peer_lies_about_the_result() {
        const TLV: u8 = 33;
        fn says_success(_: &[u8], mut reply: Vec<u8>) -> Vec<u8> {
            if reply.len() > 4 && reply[4] == TLV {
                *reply.last_mut().unwrap() = 1;
            }
            reply
        }
        fn says_failure(_: &[u8], mut reply: Vec<u8>) -> Vec<u8> {
            if reply.len() > 4 && reply[4] == TLV {
                *reply.last_mut().unwrap() = 2;
            }
            reply
        }
        fn wrong_identifier(_: &[u8], mut reply: Vec<u8>) -> Vec<u8> {
            if reply.len() > 4 && reply[4] == TLV {
                reply[1] = reply[1].wrapping_add(1);
            }
            reply
        }
        fn acknowledges_a_success_it_did_not_get(message: &[u8], reply: Vec<u8>) -> Vec<u8> {
            if message.starts_with(&[26, 4]) { vec![26, 3] } else { reply }
        }
        fn does_not_acknowledge(message: &[u8], reply: Vec<u8>) -> Vec<u8> {
            if message.starts_with(&[26, 3]) { vec![26, 4] } else { reply }
        }
        for (name, password, lie) in [
            ("lie-tlv-success", "wrong", says_success as fn(&[u8], Vec<u8>) -> Vec<u8>),
            ("lie-mschap-success", "wrong", acknowledges_a_success_it_did_not_get),
            ("lie-tlv-failure", "secret", says_failure),
            ("lie-tlv-identifier", "secret", wrong_identifier),
            ("lie-no-ack", "secret", does_not_acknowledge),
        ] {
            let table = name.replace('-', "_");
            let (dir, server) = peap_server(name, &table, "return 'accept'").await;
            let mut peer = crate::eap::tests::peap_peer(dir.path(), lying_inner(password, lie));
            let mut identifier = 1u8;
            let mut request = peap_request(identifier, b"anonymous", &crate::eap::tests::identity_response(1, b"anonymous"), None);
            let last = loop {
                let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.expect("no response")).unwrap();
                if response.code != ACCESS_CHALLENGE {
                    break response;
                }
                identifier += 1;
                assert!(identifier < 64, "{name}: it went on");
                let answer = peer.answer(&joined(&response));
                request = peap_request(identifier, b"anonymous", &answer, response.attribute(STATE));
            };
            assert_eq!(last.code, ACCESS_REJECT, "{name}");
            assert_eq!(joined(&last)[0], 4, "{name}");
            assert_eq!(microsoft_attribute(&last, 16), None, "{name}");
            assert_eq!(microsoft_attribute(&last, 17), None, "{name}");
            assert_eq!(crate::logging::records_for_client(name).len(), 1, "{name}: one record");
        }
    }

    #[tokio::test]
    async fn test_peap_malformed_packet_after_a_refusal_is_not_recorded_twice() {
        let (dir, server) = peap_server("peap-twice", "peap_twice", "return 'reject'").await;
        let heard = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut peer = crate::eap::tests::peap_peer(dir.path(), crate::eap::tests::mschap_inner("alice", "wrong", heard.clone()));
        let mut identifier = 1u8;
        let mut request = peap_request(identifier, b"anonymous", &crate::eap::tests::identity_response(1, b"anonymous"), None);
        loop {
            let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.expect("no response")).unwrap();
            assert_eq!(response.code, ACCESS_CHALLENGE);
            identifier += 1;
            let answer = peer.answer(&joined(&response));
            if !heard.lock().is_empty() {
                // Told of the refusal, it sends something else.
                let bad = peap_request(identifier, b"anonymous", &[2, 1], response.attribute(STATE));
                let last = RadiusPacket::parse(&server.handle_auth(&bad, LOCALHOST).await.expect("no response")).unwrap();
                assert_eq!(last.code, ACCESS_REJECT);
                break;
            }
            request = peap_request(identifier, b"anonymous", &answer, response.attribute(STATE));
        }
        assert_eq!(server.metrics.eap_peap.reject.get(), 1);
        assert_eq!(crate::logging::records_for_client("peap-twice").len(), 1);
        assert_eq!(server.metrics.eap_open.get(), 0);
    }

    #[tokio::test]
    async fn test_peap_anonymous_outer_name() {
        let (dir, server) = peap_server("peap-outer", "peap_outer", "return 'reject'").await;
        let (response, _, _, _) = peap_conversation(&server, dir.path(), b"anonymous", "alice", "secret").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attribute(1), Some(&b"alice"[..]), "the device gets the name that was proved");
        let records = crate::logging::records_for_client("peap-outer");
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!((records[0].user.as_str(), records[0].account.as_str(), records[0].handler.as_str(), records[0].decision.as_str()), ("anonymous", "alice", "eap-peap", "accept"));
    }

    #[tokio::test]
    async fn test_peap_script_accepts_without_a_proof() {
        let lua = crate::auth_chain::Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").unwrap());
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![lua]);
        let (dir, server) = peap_server_with("peap-noproof", "return 'accept'", Some(chain));
        let (response, _, _, heard) = peap_conversation(&server, dir.path(), b"anonymous", "alice", "secret").await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert!(heard[0].starts_with("E=691"), "{heard:?}");
    }

    #[tokio::test]
    async fn test_peap_reply_attributes() {
        let (dir, server) = peap_server("peap-reply", "peap_reply", "return 'reject'").await;
        let (response, _, _, _) = peap_conversation(&server, dir.path(), b"anonymous", "alice", "secret").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attribute(81), Some(&b"100"[..]));
        assert_eq!(microsoft_attribute(&response, 26), None, "the inner proof stays inside");
        let vendor = |kind: u8| response.attributes.iter().filter(|(attribute, value)| *attribute == 26 && value.len() > 4 && value[4] == kind).count();
        assert_eq!((vendor(16), vendor(17)), (1, 1), "only the keys from the tunnel");
    }

    /// A TTLS conversation over RADIUS, from what `peer` says.
    async fn ttls_conversation(server: &RadiusServer, mut peer: crate::eap::tests::Peer, outer: &[u8]) -> (RadiusPacket, [u8; 16], crate::eap::tests::Peer) {
        let mut identifier = 1u8;
        let mut request = peap_request(identifier, outer, &crate::eap::tests::identity_response(1, outer), None);
        for _ in 0..64 {
            let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.expect("no response")).unwrap();
            if response.code != ACCESS_CHALLENGE {
                return (response, [identifier; 16], peer);
            }
            identifier += 1;
            let answer = peer.answer(&joined(&response));
            request = peap_request(identifier, outer, &answer, response.attribute(STATE));
        }
        panic!("the conversation did not end");
    }

    fn ttls_pap_peer(dir: &std::path::Path, name: Option<&str>, password: &str) -> crate::eap::tests::Peer {
        let opening = crate::eap::tests::pap_avps(name, password);
        crate::eap::tests::ttls_peer(dir, Box::new(move |_| opening), Box::new(|_| Vec::new()))
    }

    fn ttls_server(client: &str, script: &str, chain: Option<AuthChain>) -> (tempfile::TempDir, RadiusServer) {
        let (dir, _, engine) = eap_rig(script);
        let settings = Arc::new(crate::eap::tests::ttls_settings(dir.path()));
        let mut clients = ClientTable::default();
        clients.add("127.0.0.1", Some(client.to_string()), SECRET.to_vec(), crate::clients::ClientOptions::default()).unwrap();
        let mut server = RadiusServer::new(any_addr(), any_addr(), Arc::new(clients), engine, share_dictionary()).with_eap(Some(settings));
        if let Some(chain) = chain {
            server = server.with_auth_chains(Arc::new(AuthChains::new(Some(chain), Vec::new())));
        }
        (dir, server)
    }

    #[tokio::test]
    async fn test_ttls_pap_right_password() {
        let (dir, server) = ttls_server("ttls-right", "return 'reject'", Some(peap_chain("ttls_right", None).await));
        let (response, authenticator, peer) = ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "secret"), b"anonymous").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(joined(&response)[0], 3, "EAP-Success");
        assert_eq!(response.attribute(1), Some(&b"alice"[..]));
        let mut theirs = [0u8; 64];
        peer.client.export_keying_material(&mut theirs[..], b"ttls keying material", None).unwrap();
        let (_, recv) = decrypt_mppe_key(&microsoft_attribute(&response, 17).expect("no receive key"), &authenticator);
        let (_, send) = decrypt_mppe_key(&microsoft_attribute(&response, 16).expect("no send key"), &authenticator);
        assert_eq!(recv, theirs[..32]);
        assert_eq!(send, theirs[32..64]);
        assert_eq!(server.metrics.eap_ttls.accept.get(), 1);
        let records = crate::logging::records_for_client("ttls-right");
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!((records[0].user.as_str(), records[0].account.as_str(), records[0].handler.as_str(), records[0].decision.as_str()), ("anonymous", "alice", "eap-ttls", "accept"));
    }

    #[tokio::test]
    async fn test_ttls_pap_reply_attributes() {
        let (dir, server) = peap_server("ttls-reply", "ttls_reply", "return 'reject'").await;
        let settings = Arc::new(crate::eap::tests::ttls_settings(dir.path()));
        server.replace_eap(Some(settings));
        let (response, _, _) = ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "secret"), b"anonymous").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attribute(81), Some(&b"100"[..]), "what the chain returned");
    }

    #[tokio::test]
    async fn test_ttls_pap_ldap_bind() {
        use crate::ldap_handler::{tests::{mock_server, settings}, LdapHandler, PasswordCheck};
        let (url, log) = mock_server().await;
        let handler = crate::auth_chain::Handler::Ldap(LdapHandler::new(settings(vec![url], PasswordCheck::Bind)).unwrap());
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![handler]);
        let (dir, server) = ttls_server("ttls-bind", "return 'reject'", Some(chain));
        assert_eq!(ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "secret"), b"anonymous").await.0.code, ACCESS_ACCEPT);
        assert!(log.lock().unwrap().binds.iter().any(|dn| dn.contains("alice")), "bound as the user");
        assert_eq!(ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "wrong"), b"anonymous").await.0.code, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_ttls_pap_wrong_password() {
        let (dir, server) = ttls_server("ttls-wrong", "return 'accept'", Some(peap_chain("ttls_wrong", None).await));
        let (response, _, _) = ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "wrong"), b"anonymous").await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert_eq!(joined(&response)[0], 4);
        assert_eq!(microsoft_attribute(&response, 17), None);
        assert_eq!(server.metrics.eap_ttls.reject.get(), 1);
        assert_eq!(server.metrics.eap_open.get(), 0);
        let records = crate::logging::records_for_client("ttls-wrong");
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!((records[0].account.as_str(), records[0].handler.as_str(), records[0].decision.as_str()), ("alice", "eap-ttls", "reject"));
    }

    #[tokio::test]
    async fn test_ttls_device_gets_the_account_name() {
        let username = crate::config::UsernameSettings { strip_realm: crate::config::StripRealm::Suffix, lowercase: false, allowed: None };
        let chain = peap_chain("ttls_realm", None).await.with_username(Some(username));
        let (dir, server) = ttls_server("ttls-realm", "return 'reject'", Some(chain));
        let (response, _, _) = ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice@corp"), "secret"), b"anonymous").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attribute(1), Some(&b"alice"[..]));
        assert_eq!(crate::logging::records_for_client("ttls-realm")[0].account, "alice");
    }

    #[tokio::test]
    async fn test_ttls_unknown_name_with_a_default() {
        let (dir, server) = ttls_server("ttls-default", "return 'accept'", Some(peap_chain("ttls_default", Some("DEFAULT")).await));
        let (response, _, _) = ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("ceo"), "guest-pass"), b"anonymous").await;
        assert_eq!(response.code, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_ttls_script_decides_a_password() {
        let script = "if eap_method == 'ttls' and get_attr('User-Name') == 'alice' and get_attr('User-Password') == 'secret' then return 'accept' end return 'reject'";
        let lua = crate::auth_chain::Handler::LuaVerifying(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap());
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![lua]);
        let (dir, server) = ttls_server("ttls-script", "return 'reject'", Some(chain));
        assert_eq!(ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "secret"), b"anonymous").await.0.code, ACCESS_ACCEPT);
        assert_eq!(ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "wrong"), b"anonymous").await.0.code, ACCESS_REJECT);
    }

    fn accepting_script() -> crate::auth_chain::Handler {
        crate::auth_chain::Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").unwrap())
    }

    #[tokio::test]
    async fn test_ttls_verifies_password_from_settings() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("handler.lua");
        std::fs::write(&script, "return 'accept'").unwrap();
        let chain = |verifies_password: bool| {
            let settings = crate::config::ChainSettings { policy: crate::config::AuthPolicy::FirstMatch, handlers: vec![crate::config::AuthHandlerSettings::Lua { script: script.clone(), verifies_password }], username: None };
            AuthChain::from_settings(&settings, &crate::test_support::test_db(), &crate::test_support::test_vms(), &crate::auth_chain::LuaLimits::default()).unwrap()
        };
        let (dir, server) = ttls_server("ttls-settings-yes", "return 'reject'", Some(chain(true)));
        assert_eq!(ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "anything"), b"anonymous").await.0.code, ACCESS_ACCEPT, "the script says it compared");
        let (dir, server) = ttls_server("ttls-settings-no", "return 'reject'", Some(chain(false)));
        assert_eq!(ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "anything"), b"anonymous").await.0.code, ACCESS_REJECT, "it did not");
    }

    #[tokio::test]
    async fn test_ttls_script_that_has_not_said_it_compares() {
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![accepting_script()]);
        let (dir, server) = ttls_server("ttls-unsaid", "return 'accept'", Some(chain));
        assert_eq!(ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "anything"), b"anonymous").await.0.code, ACCESS_REJECT);
        assert!(crate::test_support::logged_errors().lock().unwrap().iter().any(|line| line.contains("ttls-unsaid") && line.contains("without checking a password")));
    }

    #[tokio::test]
    async fn test_all_accept_unknown_user_with_any_password() {
        let chain = sql_then_script("all_accept_unknown", crate::config::AuthPolicy::AllAccept).await;
        let (_dir, _, engine) = eap_rig("return 'accept'");
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients(std::str::from_utf8(SECRET).unwrap()), engine, share_dictionary()).with_auth_chains(Arc::new(AuthChains::new(Some(chain), Vec::new())));
        let ask = |identifier: u8, user: &[u8], password: &str| {
            let ra = [identifier; 16];
            RadiusPacket::create_access_request(identifier, ra, vec![(1, user.to_vec()), (USER_PASSWORD, encrypt_password(password, &ra))], SECRET)
        };
        let code = |response: Vec<u8>| RadiusPacket::parse(&response).unwrap().code;
        assert_eq!(code(server.handle_auth(&ask(1, b"nosuchuser", "anything"), LOCALHOST).await.unwrap()), ACCESS_REJECT);
        assert_eq!(code(server.handle_auth(&ask(2, b"alice", "wrong"), LOCALHOST).await.unwrap()), ACCESS_REJECT);
        assert_eq!(code(server.handle_auth(&ask(3, b"alice", "secret"), LOCALHOST).await.unwrap()), ACCESS_ACCEPT);
    }

    /// `sql` over one user, then a script that accepts anyone.
    async fn sql_then_script(table: &str, policy: crate::config::AuthPolicy) -> AuthChain {
        let db = crate::test_support::test_db();
        db.execute(&format!("CREATE TABLE {table} (username TEXT, password TEXT)")).await.unwrap();
        db.execute(&format!("INSERT INTO {table} VALUES ('alice', 'secret')")).await.unwrap();
        let sql = crate::auth_chain::Handler::Sql { db, user_query: format!("SELECT password FROM {table} WHERE username = ?"), certificate_query: None, reply_query: None, default_user: None, timeout: Duration::from_secs(5) };
        AuthChain::new(policy, vec![sql, accepting_script()])
    }

    #[tokio::test]
    async fn test_ttls_script_alongside_sql() {
        use crate::config::AuthPolicy::{AllAccept, FirstMatch};
        let (dir, server) = ttls_server("ttls-both", "return 'accept'", Some(sql_then_script("ttls_both", AllAccept).await));
        let talk = |name: &'static str, password: &'static str| {
            let (server, peer) = (&server, ttls_pap_peer(dir.path(), Some(name), password));
            async move { ttls_conversation(server, peer, b"anonymous").await.0.code }
        };
        assert_eq!(talk("alice", "secret").await, ACCESS_ACCEPT, "sql compared it");
        assert_eq!(talk("alice", "wrong").await, ACCESS_REJECT);
        assert_eq!(talk("mallory", "anything").await, ACCESS_REJECT, "sql did not find it, and the script has not said it compares");
        // The same with the first that decides: sql ignores, and it is the script's accept.
        let (dir, server) = ttls_server("ttls-first", "return 'accept'", Some(sql_then_script("ttls_first", FirstMatch).await));
        assert_eq!(ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("mallory"), "anything"), b"anonymous").await.0.code, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_ttls_client_without_a_chain() {
        let (dir, server) = ttls_server("ttls-nochain", "return 'accept'", None);
        assert_eq!(ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "secret"), b"anonymous").await.0.code, ACCESS_REJECT);
        assert!(crate::test_support::logged_errors().lock().unwrap().iter().any(|line| line.contains("ttls-nochain") && line.contains("no chain")));
    }

    #[tokio::test]
    async fn test_ttls_password_in_the_log() {
        let log = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).with_writer(log.clone()).finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let chain = peap_chain("ttls_log", None).await;
        let (dir, server) = ttls_server("ttls-log", "return 'reject'", Some(chain));
        assert_eq!(ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "secret"), b"anonymous").await.0.code, ACCESS_ACCEPT);
        assert_eq!(ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "ThisIsAWrongOne"), b"anonymous").await.0.code, ACCESS_REJECT);
        let log = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(log.contains("alice"), "{log}");
        assert!(!log.contains("secret"), "{log}");
        assert!(!log.contains("ThisIsAWrongOne"), "{log}");
    }

    #[tokio::test]
    async fn test_ttls_mschap_right_password() {
        let (dir, server) = ttls_server("ttls-mschap", "return 'reject'", Some(peap_chain("ttls_mschap", None).await));
        let heard = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let peer = crate::eap::tests::ttls_mschap_peer(dir.path(), "alice", "secret", heard.clone());
        let (response, authenticator, peer) = ttls_conversation(&server, peer, b"anonymous").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        let heard = heard.lock();
        assert_eq!(heard.len(), 1, "the proof, then the acknowledgement, then the accept");
        let avps = crate::eap::parse_avps(&heard[0]).unwrap();
        assert_eq!((avps[0].code, avps[0].vendor), (26, Some(311)), "MS-CHAP2-Success");
        assert!(avps[0].data[1..].starts_with(b"S="));
        let mut theirs = [0u8; 64];
        peer.client.export_keying_material(&mut theirs[..], b"ttls keying material", None).unwrap();
        let (_, recv) = decrypt_mppe_key(&microsoft_attribute(&response, 17).unwrap(), &authenticator);
        assert_eq!(recv, theirs[..32], "from the tunnel");
        assert_eq!(microsoft_attribute(&response, 26), None, "the inner proof stays inside");
        assert_eq!(server.metrics.eap_ttls.accept.get(), 1);
    }

    #[tokio::test]
    async fn test_ttls_mschap_wrong_password() {
        let (dir, server) = ttls_server("ttls-mschap-wrong", "return 'accept'", Some(peap_chain("ttls_mschap_wrong", None).await));
        let heard = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let peer = crate::eap::tests::ttls_mschap_peer(dir.path(), "alice", "wrong", heard.clone());
        let (response, _, _) = ttls_conversation(&server, peer, b"anonymous").await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert!(heard.lock().is_empty(), "nothing more in the tunnel");
        assert_eq!(crate::logging::records_for_client("ttls-mschap-wrong").len(), 1);
    }

    #[tokio::test]
    async fn test_ttls_mschap_script_without_a_proof() {
        for script in ["return 'accept'", "set_attr('MS-CHAP2-Success', 'S=00') return 'accept'"] {
            let lua = crate::auth_chain::Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap());
            let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![lua]);
            let (dir, server) = ttls_server("ttls-noproof", "return 'accept'", Some(chain));
            let peer = crate::eap::tests::ttls_mschap_peer(dir.path(), "alice", "secret", Default::default());
            assert_eq!(ttls_conversation(&server, peer, b"anonymous").await.0.code, ACCESS_REJECT, "{script}");
        }
    }

    /// A handler over one table of `users`, with `default_user` and normalization to try.
    async fn peap_chain(table: &str, default_user: Option<&str>) -> AuthChain {
        let db = crate::test_support::test_db();
        db.execute(&format!("CREATE TABLE {table} (username TEXT, password TEXT)")).await.unwrap();
        db.execute(&format!("INSERT INTO {table} VALUES ('alice', 'secret'), ('DEFAULT', 'guest-pass')")).await.unwrap();
        let handler = crate::auth_chain::Handler::Sql {
            db,
            user_query: format!("SELECT password FROM {table} WHERE username = ?"),
            certificate_query: None,
            reply_query: None,
            default_user: default_user.map(str::to_string),
            timeout: Duration::from_secs(5),
        };
        AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![handler])
    }

    #[tokio::test]
    async fn test_peap_device_gets_the_account_name() {
        let username = crate::config::UsernameSettings { strip_realm: crate::config::StripRealm::Suffix, lowercase: false, allowed: None };
        let chain = peap_chain("peap_realm", None).await.with_username(Some(username));
        let (dir, server) = peap_server_with("peap-realm", "return 'reject'", Some(chain));
        for name in ["alice@a", "alice@b"] {
            let (response, _, _, heard) = peap_conversation(&server, dir.path(), b"bob", name, "secret").await;
            assert_eq!(response.code, ACCESS_ACCEPT, "{name}: {heard:?}");
            assert_eq!(response.attribute(1), Some(&b"alice"[..]), "{name}");
        }
        let records = crate::logging::records_for_client("peap-realm");
        assert!(records.iter().all(|record| record.account == "alice"), "{records:?}");
    }

    #[tokio::test]
    async fn test_peap_unknown_name_with_a_default() {
        let chain = peap_chain("peap_default", Some("DEFAULT")).await;
        let (dir, server) = peap_server_with("peap-default", "return 'accept'", Some(chain));
        let (response, _, _, heard) = peap_conversation(&server, dir.path(), b"anonymous", "ceo", "guest-pass").await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert!(heard[0].starts_with("E=691"), "{heard:?}");
    }

    #[tokio::test]
    async fn test_peap_script_cannot_supply_the_proof() {
        let script = "set_attr('MS-CHAP2-Success', 'S=0000000000000000000000000000000000000000') return 'accept'";
        let lua = crate::auth_chain::Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap());
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![lua]);
        let (dir, server) = peap_server_with("peap-forged", "return 'accept'", Some(chain));
        let (response, _, _, heard) = peap_conversation(&server, dir.path(), b"anonymous", "alice", "any").await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert!(heard[0].starts_with("E=691"), "{heard:?}");
    }

    #[tokio::test]
    async fn test_client_methods_reach_the_conversation() {
        let (dir, _, engine) = eap_rig("return 'accept'");
        let settings = Arc::new(EapSettings { methods: vec![crate::eap::TYPE_TLS, crate::eap::TYPE_PEAP], ..crate::eap::tests::peap_settings(dir.path()) });
        let mut clients = ClientTable::default();
        let options = crate::clients::ClientOptions { eap_methods: Some(vec![crate::eap::TYPE_PEAP]), ..Default::default() };
        clients.add("127.0.0.1", Some("peap-only".to_string()), SECRET.to_vec(), options).unwrap();
        let server = RadiusServer::new(any_addr(), any_addr(), Arc::new(clients), engine, share_dictionary()).with_eap(Some(settings));
        let first = RadiusPacket::parse(&server.handle_auth(&eap_request(1, &crate::eap::tests::identity_response(1, b"a"), None), LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(joined(&first)[4], crate::eap::TYPE_PEAP, "not the first the server lists");
    }

    #[tokio::test]
    async fn test_peap_refusal_recorded_when_the_peer_stops() {
        let (dir, server) = peap_server("peap-stops", "peap_stops", "return 'accept'").await;
        let heard = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut peer = crate::eap::tests::peap_peer(dir.path(), crate::eap::tests::mschap_inner("alice", "wrong", heard.clone()));
        let mut request = peap_request(1, b"anonymous", &crate::eap::tests::identity_response(1, b"anonymous"), None);
        for identifier in 2..64u8 {
            let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
            assert_eq!(response.code, ACCESS_CHALLENGE);
            let answer = peer.answer(&joined(&response));
            if !heard.lock().is_empty() {
                break; // It has its E=691, and says no more.
            }
            request = peap_request(identifier, b"anonymous", &answer, response.attribute(STATE));
        }
        assert!(heard.lock()[0].starts_with("E=691"));
        let records = crate::logging::records_for_client("peap-stops");
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!((records[0].account.as_str(), records[0].handler.as_str(), records[0].decision.as_str()), ("alice", "eap-peap", "reject"));
        assert_eq!(server.metrics.eap_peap.reject.get(), 1);
        assert!(crate::test_support::logged_errors().lock().unwrap().iter().any(|line| line.contains("peap-stops") && line.contains("did not accept")));
    }

    #[tokio::test]
    async fn test_peap_refusal_under_the_account_name() {
        let username = crate::config::UsernameSettings { strip_realm: crate::config::StripRealm::Suffix, lowercase: false, allowed: None };
        let chain = peap_chain("peap_realm_wrong", None).await.with_username(Some(username));
        let (dir, server) = peap_server_with("peap-realm-wrong", "return 'accept'", Some(chain));
        let (response, _, _, _) = peap_conversation(&server, dir.path(), b"anonymous", "alice@corp", "wrong").await;
        assert_eq!(response.code, ACCESS_REJECT);
        let records = crate::logging::records_for_client("peap-realm-wrong");
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].account, "alice");
    }

    #[tokio::test]
    async fn test_peap_client_without_a_chain() {
        let (dir, server) = peap_server_with("peap-nochain", "return 'accept'", None);
        let (response, _, _, _) = peap_conversation(&server, dir.path(), b"anonymous", "alice", "secret").await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert_eq!(joined(&response)[0], 4);
        assert!(crate::test_support::logged_errors().lock().unwrap().iter().any(|line| line.contains("peap-nochain") && line.contains("no chain")));
    }

    /// Up to the first response, so that a test can reload before going on.
    async fn eap_begin(server: &RadiusServer, dir: &std::path::Path, identity: &[u8]) -> (RadiusPacket, crate::eap::tests::Peer) {
        let peer = crate::eap::tests::Peer::new(crate::eap::tests::client_connection(dir, Some("alice"), &rustls::version::TLS12));
        let first = server.handle_auth(&eap_request(1, &crate::eap::tests::identity_response(1, identity), None), LOCALHOST).await.unwrap();
        (RadiusPacket::parse(&first).unwrap(), peer)
    }

    async fn eap_finish(server: &RadiusServer, mut response: RadiusPacket, peer: &mut crate::eap::tests::Peer) -> RadiusPacket {
        let mut identifier = 1u8;
        for _ in 0..64 {
            if response.code != ACCESS_CHALLENGE {
                return response;
            }
            identifier += 1;
            let answer = peer.answer(&joined(&response));
            let request = eap_request(identifier, &answer, response.attribute(STATE));
            response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        }
        panic!("the conversation did not end");
    }

    #[tokio::test]
    async fn test_eap_match_identity_stays_with_the_conversation() {
        let (dir, settings, engine) = eap_rig("return 'accept'");
        let server = eap_server(&settings, &engine);
        let (first, mut peer) = eap_begin(&server, dir.path(), b"anonymous").await;
        server.replace_eap(Some(Arc::new(EapSettings { match_identity: false, ..crate::eap::tests::eap_settings(dir.path(), None) })));
        assert_eq!(eap_finish(&server, first, &mut peer).await.code, ACCESS_REJECT, "begun with the check on");
    }

    #[tokio::test]
    async fn test_eap_table_removed_during_a_conversation() {
        let (dir, settings, engine) = eap_rig("return 'accept'");
        let server = eap_server(&settings, &engine);
        let (first, mut peer) = eap_begin(&server, dir.path(), b"alice.example.com").await;
        server.replace_eap(None);
        let last = eap_finish(&server, first, &mut peer).await;
        assert_eq!(last.code, ACCESS_ACCEPT);
        assert_eq!(joined(&last)[0], 3, "EAP-Success: it went through EAP");
        assert!(microsoft_attribute(&last, 17).is_some());
        assert_eq!(server.metrics.eap_open.get(), 0);
        // A new one is an ordinary attribute again.
        let (response, _) = eap_begin(&server, dir.path(), b"alice.example.com").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attribute(EAP_MESSAGE), None);
    }

    #[tokio::test]
    async fn test_eap_sessions_swept_while_running() {
        let (dir, _, engine) = eap_rig("return 'accept'");
        let settings = Arc::new(EapSettings { session_timeout: Duration::from_millis(20), ..crate::eap::tests::eap_settings(dir.path(), None) });
        let server = Arc::new(eap_server(&settings, &engine));
        let _ = eap_begin(&server, dir.path(), b"alice.example.com").await;
        assert_eq!(server.metrics.eap_open.get(), 1);
        let running = tokio::spawn(server.clone().run());
        let began = Instant::now();
        while server.metrics.eap_open.get() != 0 {
            assert!(began.elapsed() < Duration::from_secs(5), "still open with no request");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        running.abort();
    }

    #[tokio::test]
    async fn test_eap_anonymous_identity() {
        let (dir, _, engine) = eap_rig("return get_attr('User-Name') == 'alice.example.com' and 'accept' or 'reject'");
        let settings = Arc::new(EapSettings { match_identity: false, ..crate::eap::tests::eap_settings(dir.path(), None) });
        let server = eap_server(&settings, &engine);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"anonymous", &rustls::version::TLS12).await;
        assert_eq!(response.code, ACCESS_ACCEPT);
    }

    /// A request of EAP from this device, under this RADIUS name.
    fn eap_request_from(identifier: u8, name: &[u8], station: Option<&[u8]>, eap: &[u8], state: Option<&[u8]>) -> Vec<u8> {
        let mut attributes = vec![(1, name.to_vec())];
        attributes.extend(station.map(|station| (CALLING_STATION_ID, station.to_vec())));
        attributes.extend(split_message(eap));
        attributes.extend(state.map(|state| (STATE, state.to_vec())));
        RadiusPacket::create_access_request(identifier, [identifier; 16], attributes, SECRET)
    }

    #[tokio::test]
    async fn test_eap_tls_authorizes_the_certificate_name_not_the_radius_name() {
        // The device's name is not proof of anything, and with no match against the identity,
        // nothing else would stop it naming someone else.
        let (dir, _, engine) = eap_rig("return get_attr('User-Name') == 'alice.example.com' and 'accept' or 'reject'");
        let settings = Arc::new(EapSettings { match_identity: false, ..crate::eap::tests::eap_settings(dir.path(), None) });
        let server = eap_server(&settings, &engine);
        let mut peer = crate::eap::tests::Peer::new(crate::eap::tests::client_connection(dir.path(), Some("alice"), &rustls::version::TLS12));
        let mut identifier = 1u8;
        let mut request = eap_request_from(identifier, b"ceo", None, &crate::eap::tests::identity_response(1, b"anonymous"), None);
        let last = loop {
            let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.expect("no response")).unwrap();
            if response.code != ACCESS_CHALLENGE {
                break response;
            }
            identifier += 1;
            assert!(identifier < 64, "it went on");
            let answer = peer.answer(&joined(&response));
            request = eap_request_from(identifier, b"ceo", None, &answer, response.attribute(STATE));
        };
        assert_eq!(last.code, ACCESS_ACCEPT);
        assert_eq!(last.attribute(1), Some(&b"alice.example.com"[..]), "the device gets the name that was proved");
    }

    #[tokio::test]
    async fn test_eap_conversation_belongs_to_a_device() {
        let (dir, _, engine) = eap_rig("return 'accept'");
        let settings = Arc::new(EapSettings { max_sessions_per_device: 1, ..crate::eap::tests::eap_settings(dir.path(), None) });
        let server = eap_server(&settings, &engine);
        let identity = crate::eap::tests::identity_response(1, b"alice.example.com");
        let ask = |request: Vec<u8>| {
            let server = &server;
            async move { RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.expect("no response")).unwrap() }
        };
        let opened = ask(eap_request_from(1, b"a", Some(b"aa"), &identity, None)).await;
        assert_eq!(opened.code, ACCESS_CHALLENGE);
        let state = opened.attribute(STATE).unwrap().to_vec();
        let mut peer = crate::eap::tests::Peer::new(crate::eap::tests::client_connection(dir.path(), Some("alice"), &rustls::version::TLS12));
        let hello = peer.answer(&joined(&opened));
        // Not from another device, nor from none at all.
        assert_eq!(ask(eap_request_from(2, b"a", Some(b"bb"), &hello, Some(&state))).await.code, ACCESS_REJECT);
        assert_eq!(ask(eap_request_from(3, b"a", None, &hello, Some(&state))).await.code, ACCESS_REJECT);
        assert_eq!(server.metrics.eap_open.get(), 1, "and it goes on");
        assert_eq!(ask(eap_request_from(4, b"a", Some(b"aa"), &hello, Some(&state))).await.code, ACCESS_CHALLENGE);
        // Under way, it is not replaced, and the device is at its limit: no answer.
        assert!(server.handle_auth(&eap_request_from(5, b"a", Some(b"aa"), &identity, None), LOCALHOST).await.is_none());
        assert_eq!(server.metrics.eap_tls.overload.get(), 1);
        // Another device starting over before anything has been said takes its own place.
        let opened = ask(eap_request_from(6, b"a", Some(b"bb"), &identity, None)).await;
        let state = opened.attribute(STATE).unwrap().to_vec();
        assert_eq!(ask(eap_request_from(7, b"a", Some(b"bb"), &identity, None)).await.code, ACCESS_CHALLENGE);
        assert_eq!(server.metrics.eap_tls.replaced.get(), 1);
        let hello = crate::eap::tests::Peer::new(crate::eap::tests::client_connection(dir.path(), Some("alice"), &rustls::version::TLS12)).answer(&joined(&opened));
        assert_eq!(ask(eap_request_from(8, b"a", Some(b"bb"), &hello, Some(&state))).await.code, ACCESS_REJECT, "the old one is gone");
    }

    #[tokio::test]
    async fn test_scripts_see_a_common_name_that_looks_like_another_attribute() {
        let script = r"return (tls_subject == [[CN=x\, OU=admins]] and tls_subject_attrs.OU == nil and tls_subject_attrs.CN[1] == 'x, OU=admins') and 'accept' or 'reject'";
        let dir = tempfile::tempdir().unwrap();
        let issuer = crate::test_support::certs::ca(dir.path(), "ca");
        crate::test_support::certs::leaf(dir.path(), "server", &issuer);
        crate::test_support::certs::named_leaf_with_common_name(dir.path(), "alice", "alice.example.com", "x, OU=admins", 20, &issuer);
        let settings = Arc::new(crate::eap::tests::eap_settings(dir.path(), None));
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap());
        let server = eap_server(&settings, &engine);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"alice.example.com", &rustls::version::TLS13).await;
        assert_eq!(response.code, ACCESS_ACCEPT);
    }

    #[tokio::test]
    async fn test_scripts_see_subject_attributes() {
        // The test certificate has one CN; OU is not in it.
        let script = "if tls_subject_attrs and tls_subject_attrs.CN and tls_subject_attrs.CN[1] and not tls_subject_attrs.OU then return 'accept' end return 'reject'";
        let (dir, settings, engine) = eap_rig(script);
        let server = eap_server(&settings, &engine);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"alice.example.com", &rustls::version::TLS13).await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        // Nothing of the kind outside EAP-TLS.
        let (_dir, settings, engine) = eap_rig("return tls_subject_attrs and 'accept' or 'reject'");
        let server = eap_server(&settings, &engine);
        let ra = [73u8; 16];
        let request = RadiusPacket::create_access_request(73, ra, vec![(1, b"alice".to_vec()), (USER_PASSWORD, encrypt_password("x", &ra))], SECRET);
        assert_eq!(RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap().code, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_scripts_see_several_values_of_an_attribute() {
        let script = "local ou = tls_subject_attrs.OU return (#ou == 2 and ou[1] == 'a' and ou[2] == 'b') and 'accept' or 'reject'";
        let engine = LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap();
        let mut request = LuaRequest::new("radius", "authentication", Vec::new());
        request.tls_subject_attrs = Some(vec![("OU".to_string(), "a".to_string()), ("CN".to_string(), "c".to_string()), ("OU".to_string(), "b".to_string())]);
        assert_eq!(engine.authenticate(request).await.outcome, Outcome::Accept);
    }

    #[tokio::test]
    async fn test_eap_script_refuses() {
        let (dir, settings, engine) = eap_rig("set_attr('Filter-Id', 'x') return 'reject'");
        let server = eap_server(&settings, &engine);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"alice.example.com", &rustls::version::TLS12).await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert_eq!(joined(&response)[0], 4);
        assert_eq!(response.attribute(11), None, "attributes go with an accept only");
    }

    #[tokio::test]
    async fn test_eap_settings_replaced_for_new_conversations() {
        let (dir, settings, engine) = eap_rig("return 'accept'");
        let server = eap_server(&settings, &engine);
        // Begins under the first settings.
        let first = RadiusPacket::parse(&server.handle_auth(&eap_request(1, &crate::eap::tests::identity_response(1, b"alice.example.com"), None), LOCALHOST).await.unwrap()).unwrap();
        let mut peer = crate::eap::tests::Peer::new(crate::eap::tests::client_connection(dir.path(), Some("alice"), &rustls::version::TLS12));
        // A new certificate from now on, and a CA the old conversation's peer does not trust.
        let other_issuer = crate::test_support::certs::ca(dir.path(), "new-ca");
        crate::test_support::certs::leaf(dir.path(), "new-server", &other_issuer);
        server.replace_eap(Some(Arc::new(EapSettings {
            tls: Some(crate::eap::tls_config(&dir.path().join("new-server.pem"), &dir.path().join("new-server.key"), &dir.path().join("new-ca.pem"), None).unwrap()),
            ..crate::eap::tests::eap_settings(dir.path(), None)
        })));
        let mut response = first;
        let mut identifier = 1;
        for _ in 0..64 {
            if response.code != ACCESS_CHALLENGE {
                break;
            }
            identifier += 1;
            let answer = peer.answer(&joined(&response));
            let request = eap_request(identifier, &answer, response.attribute(STATE));
            response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        }
        assert_eq!(response.code, ACCESS_ACCEPT, "the conversation under way kept the old certificate");
        // The next one gets the new certificate, which that peer does not trust.
        let mut peer = crate::eap::tests::Peer::new(crate::eap::tests::client_connection(dir.path(), Some("alice"), &rustls::version::TLS12));
        let mut response = RadiusPacket::parse(&server.handle_auth(&eap_request(100, &crate::eap::tests::identity_response(1, b"alice.example.com"), None), LOCALHOST).await.unwrap()).unwrap();
        for identifier in 101..165 {
            if response.code != ACCESS_CHALLENGE || peer.refused {
                break;
            }
            let answer = peer.answer(&joined(&response));
            let request = eap_request(identifier, &answer, response.attribute(STATE));
            response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        }
        assert!(peer.refused, "the new conversation got the new certificate");
    }

    /// A server whose scripts see queries time out, as during an outage, with a spool in `dir`.
    async fn spooling_server(dir: &std::path::Path, script: &str) -> Arc<RadiusServer> {
        let metrics = Arc::new(crate::metrics::Metrics::default());
        let spool = Arc::new(Spool::open(dir, 1 << 20, metrics.acct_spool.clone()).unwrap());
        let server = script_server("xyzzy5461", script).await.with_metrics(metrics).with_spool(spool);
        Arc::new(server)
    }

    fn engine_with(script: &str, query_timeout: Duration) -> Arc<LuaAuthEngine> {
        Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap().with_query_timeout(query_timeout))
    }

    /// A login the database could not decide goes unanswered and is counted, where one the data
    /// refuses gets its Access-Reject.
    #[tokio::test]
    async fn test_unavailable_database_leaves_login_unanswered() -> Result<(), String> {
        let slow = "local ok = pcall(sql_query, [[WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5000000) SELECT max(x) FROM c]]) return 'reject'";
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients("xyzzy5461"), engine_with(slow, Duration::from_millis(5)), share_dictionary());
        let request = RadiusPacket::create_access_request(1, [1; 16], vec![(1, b"alice".to_vec())], b"xyzzy5461");
        assert!(server.handle_auth(&request, LOCALHOST).await.is_none(), "not answered");
        assert_eq!(server.metrics.radius_discarded_database.get(), 1);
        assert!(server.metrics.render().contains("rradius_radius_discarded_total{reason=\"database\"} 1"));
        let refusing = RadiusServer::new(any_addr(), any_addr(), local_clients("xyzzy5461"), engine_with("return 'reject'", Duration::from_secs(5)), share_dictionary());
        let answer = refusing.handle_auth(&request, LOCALHOST).await.ok_or("no answer")?;
        assert_eq!(RadiusPacket::parse(&answer)?.code, ACCESS_REJECT);
        Ok(())
    }

    const OUTAGE: &str = "sql_query([[WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 200000) SELECT count(*) AS n FROM c]]) return 'accept'";

    #[test]
    fn test_settling_a_window() {
        use Replayed::{Accepted, Refused, TryAgain};
        assert_eq!(settled(&[Accepted, Accepted, Accepted]), 3);
        assert_eq!(settled(&[Accepted, Refused("gone"), TryAgain, Accepted]), 2);
        assert_eq!(settled(&[TryAgain, Accepted]), 0);
        assert_eq!(settled(&[]), 0);
    }

    fn replace_engine(server: &RadiusServer, engine: Arc<LuaAuthEngine>) {
        server.replace_state(RadiusState::new(server.state().clients.clone(), engine, None));
    }

    fn acct_request(id: u8, delay: Option<u32>) -> Vec<u8> {
        let mut attributes = vec![(40, vec![0, 0, 0, 1]), (44, format!("spool-{id}").into_bytes())];
        if let Some(delay) = delay {
            attributes.push((41, delay.to_be_bytes().to_vec()));
        }
        RadiusPacket::create_accounting_request(id, attributes, SECRET)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_spool_answers_during_an_outage_and_replays_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let server = spooling_server(dir.path(), "return 'accept'").await;
        replace_engine(&server, engine_with(OUTAGE, Duration::from_millis(1)));
        for id in 1..=3 {
            let response = server.handle_acct(&acct_request(id, None), LOCALHOST).await.expect("answered once on disk");
            assert_eq!(RadiusPacket::parse(&response).unwrap().code, ACCOUNTING_RESPONSE);
        }
        assert_eq!(server.metrics.acct_spool.written.get(), 3);
        assert!(server.metrics.acct_spool.bytes.get() > 0);

        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE spool_replayed (id TEXT, replayed INTEGER)").await.unwrap();
        // Still down: nothing moves.
        let replaying = tokio::spawn(server.clone().replay_spool(Duration::from_millis(20), 1));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(server.metrics.acct_spool.replayed.get(), 0);

        // Back up: a script that records them.
        replace_engine(&server, engine_with("sql_execute('INSERT INTO spool_replayed VALUES (?, ?)', {get_attr('Acct-Session-Id'), replayed}) return 'accept'", Duration::from_secs(5)));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        // Counted first, then the progress is saved and the bytes go down.
        while server.metrics.acct_spool.replayed.get() < 3 || server.metrics.acct_spool.bytes.get() > 0 {
            assert!(std::time::Instant::now() < deadline, "not replayed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        replaying.abort();
        let rows = db.query("SELECT id, replayed FROM spool_replayed ORDER BY rowid").await.unwrap();
        let text = |row: &crate::lua_engine::DbRow, column: usize| format!("{:?}", row[column].1);
        assert_eq!(rows.iter().map(|row| text(row, 0)).collect::<Vec<_>>(), ["spool-1", "spool-2", "spool-3"].map(|id| format!("{:?}", crate::lua_engine::DbValue::Text(id.to_string()))));
        assert!(rows.iter().all(|row| text(row, 1) == format!("{:?}", crate::lua_engine::DbValue::Integer(1))), "{rows:?}");
    }

    /// Spools eight requests, then replays them with `window` through a script that waits on
    /// the database for each, and returns the most scripts that ran at the same time: each one
    /// writes a row when it starts and, before it ends, counts the runs started and not ended.
    async fn local_replay_overlap(window: usize, table: &str) -> Result<usize, String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let server = spooling_server(dir.path(), "return 'accept'").await;
        replace_engine(&server, engine_with(OUTAGE, Duration::from_millis(1)));
        for id in 1..=8 {
            server.handle_acct(&acct_request(id, None), LOCALHOST).await.ok_or("not answered during the outage")?;
        }
        let db = crate::test_support::test_db();
        db.execute(&format!("CREATE TABLE {table} (id TEXT, kind TEXT, running INTEGER)")).await.map_err(|e| e.to_string())?;
        let script = format!(
            "local id = get_attr('Acct-Session-Id') \
             sql_execute('INSERT INTO {table} VALUES (?, ?, 0)', {{id, 'start'}}) \
             sql_query([[WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 300000) SELECT count(*) AS n FROM c]]) \
             local rows = sql_query([[SELECT sum(kind = 'start') - sum(kind = 'end') AS running FROM {table}]]) \
             sql_execute('INSERT INTO {table} VALUES (?, ?, ?)', {{id, 'end', rows[1].running}}) return 'accept'"
        );
        replace_engine(&server, engine_with(&script, Duration::from_secs(60)));
        let replaying = tokio::spawn(server.clone().replay_spool(Duration::from_millis(20), window));
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while server.metrics.acct_spool.replayed.get() < 8 || server.metrics.acct_spool.bytes.get() > 0 {
            if std::time::Instant::now() > deadline {
                replaying.abort();
                return Err(format!("replayed {} of 8", server.metrics.acct_spool.replayed.get()));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        replaying.abort();
        let rows = db.query(&format!("SELECT running FROM {table} WHERE kind = 'end'")).await.map_err(|e| e.to_string())?;
        let counts = rows
            .iter()
            .map(|row| match row.first().map(|(_, value)| value) {
                Some(crate::lua_engine::DbValue::Integer(running)) => usize::try_from(*running).ok(),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .ok_or("unreadable counts")?;
        if counts.len() < 8 {
            return Err(format!("{} runs recorded", counts.len()));
        }
        Ok(counts.into_iter().max().unwrap_or(0))
    }

    /// With a window of 4 up to four scripts run at once, and more than one does; by default
    /// one at a time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_local_spool_replays_a_window_at_once() -> Result<(), String> {
        let together = local_replay_overlap(4, "local_window_four").await?;
        assert!((2..=4).contains(&together), "{together} scripts at once with a window of 4");
        assert_eq!(local_replay_overlap(1, "local_window_one").await?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_nothing_is_spooled_without_a_database_failure() {
        for script in ["error('a bug')", "return 'reject'", "sql_query('SELEKT 1') return 'accept'", "return"] {
            let dir = tempfile::tempdir().unwrap();
            let server = spooling_server(dir.path(), script).await;
            assert!(server.handle_acct(&acct_request(1, None), LOCALHOST).await.is_none(), "{script}");
            assert_eq!(server.metrics.acct_spool.written.get(), 0, "{script}");
        }
    }

    #[tokio::test]
    async fn test_caught_database_failure_is_spooled() {
        let dir = tempfile::tempdir().unwrap();
        let server = spooling_server(dir.path(), "return 'accept'").await;
        let caught = "pcall(function() ".to_string() + OUTAGE.trim_end_matches(" return 'accept'") + " end) return 'reject'";
        replace_engine(&server, engine_with(&caught, Duration::from_millis(1)));
        assert!(server.handle_acct(&acct_request(1, None), LOCALHOST).await.is_some());
        assert_eq!(server.metrics.acct_spool.written.get(), 1);
    }

    /// A script that catches the failure and accepts may not have recorded anything: on disk
    /// before the answer, as the replay would see it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_an_accept_after_a_caught_outage_is_spooled() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let server = spooling_server(dir.path(), "return 'accept'").await;
        let caught = "pcall(function() ".to_string() + OUTAGE.trim_end_matches(" return 'accept'") + " end) return 'accept'";
        replace_engine(&server, engine_with(&caught, Duration::from_millis(1)));
        assert!(server.handle_acct(&acct_request(1, None), LOCALHOST).await.is_some());
        assert_eq!(server.metrics.acct_spool.written.get(), 1, "spooled before the answer");
        // Without a database failure nothing goes to the spool.
        replace_engine(&server, engine_with("return 'accept'", Duration::from_secs(5)));
        assert!(server.handle_acct(&acct_request(2, None), LOCALHOST).await.is_some());
        assert_eq!(server.metrics.acct_spool.written.get(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_without_a_spool_an_outage_gets_no_answer() {
        let server = script_server("xyzzy5461", "return 'accept'").await;
        replace_engine(&server, engine_with(OUTAGE, Duration::from_millis(1)));
        assert!(server.handle_acct(&acct_request(1, None), LOCALHOST).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_without_a_spool_an_accept_after_a_caught_outage_is_answered() {
        let server = script_server("xyzzy5461", "return 'accept'").await;
        let caught = "pcall(function() ".to_string() + OUTAGE.trim_end_matches(" return 'accept'") + " end) return 'accept'";
        replace_engine(&server, engine_with(&caught, Duration::from_millis(1)));
        assert!(server.handle_acct(&acct_request(1, None), LOCALHOST).await.is_some(), "the script has accepted");
    }

    #[tokio::test]
    async fn test_full_spool_gets_no_answer() {
        let dir = tempfile::tempdir().unwrap();
        let metrics = Arc::new(crate::metrics::Metrics::default());
        let spool = Arc::new(Spool::open(dir.path(), 10, metrics.acct_spool.clone()).unwrap());
        let server = script_server("xyzzy5461", "return 'accept'").await.with_metrics(metrics).with_spool(spool);
        replace_engine(&server, engine_with(OUTAGE, Duration::from_millis(1)));
        assert!(server.handle_acct(&acct_request(1, None), LOCALHOST).await.is_none());
        assert_eq!(server.metrics.acct_spool.full.get(), 1);
    }

    /// Nothing sends these again once they were answered, so only a verdict that would be the
    /// same next time takes one out of the queue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_replay_keeps_what_may_work_later() {
        for (script, limit) in [("error('a bug')", Duration::from_secs(5)), ("while true do end", Duration::from_millis(20))] {
            let dir = tempfile::tempdir().unwrap();
            let server = spooling_server(dir.path(), "return 'accept'").await;
            let name = server.state().clients.lookup(LOCALHOST).unwrap().name.clone();
            let request = SpooledRequest { client: name, source: LOCALHOST, received: now_unix() as u64, packet: acct_request(1, None), radsec: false };
            server.spool.clone().unwrap().write(&request).await.unwrap();
            let failing = LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap().with_max_execution(limit);
            replace_engine(&server, Arc::new(failing));
            let replaying = tokio::spawn(server.clone().replay_spool(Duration::from_millis(10), 1));
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(server.metrics.acct_spool.rejected.get(), 0, "{script}");
            replace_engine(&server, engine_with("return 'accept'", Duration::from_secs(5)));
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while server.metrics.acct_spool.replayed.get() < 1 {
                assert!(std::time::Instant::now() < deadline, "{script} was not replayed");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            replaying.abort();
        }
    }

    /// What replaying one request through `script` comes to.
    async fn replayed_by(script: &str, query_timeout: Duration) -> Replayed {
        let dir = tempfile::tempdir().unwrap();
        let server = spooling_server(dir.path(), "return 'accept'").await;
        let name = server.state().clients.lookup(LOCALHOST).unwrap().name.clone();
        let request = SpooledRequest { client: name, source: LOCALHOST, received: now_unix() as u64, packet: acct_request(1, None), radsec: false };
        replace_engine(&server, engine_with(script, query_timeout));
        server.replay_one(&request).await
    }

    /// The connection it came over is long gone.
    #[tokio::test]
    async fn test_replay_has_no_radsec_subject() {
        let dir = tempfile::tempdir().unwrap();
        let server = spooling_server(dir.path(), "return 'accept'").await;
        let name = server.state().clients.lookup(LOCALHOST).unwrap().name.clone();
        let request = SpooledRequest { client: name, source: LOCALHOST, received: now_unix() as u64, packet: acct_request(1, None), radsec: true };
        replace_engine(&server, engine_with("return (replayed and radsec_subject == nil and radsec_subject_attrs == nil) and 'accept' or 'reject'", Duration::from_secs(5)));
        assert!(matches!(server.replay_one(&request).await, Replayed::Accepted));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_replay_accepted_while_a_call_failed_goes_on_trying() {
        let caught = "pcall(function() ".to_string() + OUTAGE.trim_end_matches(" return 'accept'") + " end) return 'accept'";
        assert!(matches!(replayed_by(&caught, Duration::from_millis(1)).await, Replayed::TryAgain));
        assert!(matches!(replayed_by("return 'accept'", Duration::from_secs(5)).await, Replayed::Accepted));
        // A wrong statement the script chose to live with is no failure of the database.
        assert!(matches!(replayed_by("pcall(sql_query, 'SELEKT 1') return 'accept'", Duration::from_secs(5)).await, Replayed::Accepted));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_replay_of_an_accept_with_a_failed_call_waits_for_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let server = spooling_server(dir.path(), "return 'accept'").await;
        let name = server.state().clients.lookup(LOCALHOST).unwrap().name.clone();
        let request = SpooledRequest { client: name, source: LOCALHOST, received: now_unix() as u64, packet: acct_request(1, None), radsec: false };
        server.spool.clone().unwrap().write(&request).await.unwrap();
        let caught = "pcall(function() ".to_string() + OUTAGE.trim_end_matches(" return 'accept'") + " end) return 'accept'";
        replace_engine(&server, engine_with(&caught, Duration::from_millis(1)));
        let replaying = tokio::spawn(server.clone().replay_spool(Duration::from_millis(10), 1));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(server.metrics.acct_spool.replayed.get(), 0, "it does not count as replayed");
        assert_eq!(server.metrics.acct_spool.rejected.get(), 0);
        replace_engine(&server, engine_with("return 'accept'", Duration::from_secs(5)));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while server.metrics.acct_spool.replayed.get() < 1 {
            assert!(std::time::Instant::now() < deadline, "the same request is tried again");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        replaying.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_replay_gives_up_only_for_the_error_that_ended_the_script() {
        let limit = Duration::from_secs(5);
        for script in [
            "pcall(sql_query, 'SELEKT 1') error('a bug')",
            "pcall(sql_execute, 'INSERT INTO no_such_table VALUES (1)') error('a bug')",
            "pcall(sql_query, 'SELECT ?', {1, 2}) error('a bug')",
            "pcall(sql_execute, 'INSERT INTO t VALUES (random())') error('a bug')",
        ] {
            assert!(matches!(replayed_by(script, limit).await, Replayed::TryAgain), "{script}");
        }
        for script in [
            "local _, e = pcall(sql_query, 'SELEKT 1') error(e)",
            "local _, e = pcall(sql_query, 'SELEKT 1') error(e, 0)",
            "local _, e = pcall(sql_query, 'SELEKT 1') pcall(sql_query, 'SELEKT 2') error(e)",
            "local _, e = pcall(sql_query, 'SELECT ?', {1, 2}) error(e)",
            "local _, e = pcall(sql_execute, 'INSERT INTO t VALUES (random())') error(e)",
        ] {
            assert!(matches!(replayed_by(script, limit).await, Replayed::Refused(_)), "{script}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_replay_gives_up_on_a_violated_constraint() {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE IF NOT EXISTS replay_unique (id INTEGER PRIMARY KEY)").await.unwrap();
        db.execute("INSERT OR IGNORE INTO replay_unique VALUES (1)").await.unwrap();
        assert!(matches!(replayed_by("sql_execute('INSERT INTO replay_unique VALUES (1)') return 'accept'", Duration::from_secs(5)).await, Replayed::Refused(_)));
        db.execute("CREATE TABLE IF NOT EXISTS replay_child (parent INTEGER NOT NULL REFERENCES replay_unique(id))").await.unwrap();
        assert!(matches!(replayed_by("sql_execute('INSERT INTO replay_child VALUES (918273645)') return 'accept'", Duration::from_secs(5)).await, Replayed::Refused(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_replay_refuses_what_would_fail_the_same_way() {
        for script in ["return 'reject'", "return", "sql_query('SELEKT 1') return 'accept'", "sql_execute('INSERT INTO no_such_table VALUES (1)') return 'accept'", "sql_query('SELECT ?', {1, 2})"] {
            let dir = tempfile::tempdir().unwrap();
            let server = spooling_server(dir.path(), "return 'accept'").await;
            let name = server.state().clients.lookup(LOCALHOST).unwrap().name.clone();
            let request = SpooledRequest { client: name, source: LOCALHOST, received: now_unix() as u64, packet: acct_request(1, None), radsec: false };
            server.spool.clone().unwrap().write(&request).await.unwrap();
            replace_engine(&server, engine_with(script, Duration::from_secs(5)));
            let replaying = tokio::spawn(server.clone().replay_spool(Duration::from_millis(10), 1));
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while server.metrics.acct_spool.rejected.get() < 1 {
                assert!(std::time::Instant::now() < deadline, "{script} was not refused");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            replaying.abort();
        }
    }

    /// Hidden attributes are decoded with the secret of the path the request came in on.
    #[tokio::test]
    async fn test_replay_uses_the_secret_of_radsec() {
        let dir = tempfile::tempdir().unwrap();
        let server = spooling_server(dir.path(), "return 'accept'").await;
        let name = server.state().clients.lookup(LOCALHOST).unwrap().name.clone();
        let authenticator = [7; 16];
        let hidden = encrypt_with(b"pw", b"radsec", &authenticator);
        let mut packet = RadiusPacket::create_accounting_request(1, vec![(40, vec![0, 0, 0, 1]), (2, hidden)], b"radsec");
        // The hidden attribute was made with this authenticator.
        packet[4..20].copy_from_slice(&authenticator);
        replace_engine(&server, engine_with("return get_attr('User-Password') == 'pw' and 'accept' or 'reject'", Duration::from_secs(5)));
        let request = SpooledRequest { client: name, source: LOCALHOST, received: now_unix() as u64, packet, radsec: true };
        assert!(matches!(server.replay_one(&request).await, Replayed::Accepted));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_replay_delay_time_and_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let server = spooling_server(dir.path(), "return 'accept'").await;
        let spool = server.spool.clone().unwrap();
        let old = |id: u8, delay: Option<u32>| SpooledRequest { client: "local-client".to_string(), source: LOCALHOST, received: now_unix() as u64 - 60, packet: acct_request(id, delay), radsec: false };
        let name = server.state().clients.lookup(LOCALHOST).unwrap().name.clone();
        for request in [old(1, Some(5)), old(2, None), SpooledRequest { client: "gone".to_string(), ..old(3, None) }, old(4, Some(5))] {
            let request = if request.client == "gone" { request } else { SpooledRequest { client: name.clone(), ..request } };
            spool.write(&request).await.unwrap();
        }
        // Refuses spool-2, for moving on; checks the delay of the others.
        let script = "local delay = get_attr('Acct-Delay-Time') if get_attr('Acct-Session-Id') == 'spool-2' then return 'reject' end return (delay >= 65 and delay <= 67) and 'accept' or 'reject'";
        replace_engine(&server, engine_with(script, Duration::from_secs(5)));
        let replaying = tokio::spawn(server.clone().replay_spool(Duration::from_millis(20), 1));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while server.metrics.acct_spool.bytes.get() > 0 {
            assert!(std::time::Instant::now() < deadline, "not finished");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        replaying.abort();
        assert_eq!((server.metrics.acct_spool.replayed.get(), server.metrics.acct_spool.rejected.get()), (2, 2), "spool-2, and the client that is gone");
    }

    #[tokio::test]
    async fn test_accounting_response() {
        let server = test_server("xyzzy5461").await;
        let request_bytes = RadiusPacket::create_accounting_request(3, vec![(40, vec![0, 0, 0, 1])], SECRET);
        let request = RadiusPacket::parse(&request_bytes).unwrap();
        let response_bytes = server.handle_acct(&request_bytes, LOCALHOST).await.unwrap();
        let response = RadiusPacket::parse(&response_bytes).unwrap();
        assert_eq!(response.code, ACCOUNTING_RESPONSE);
        assert_eq!(response.identifier, 3);

        let mut for_auth = response_bytes.clone();
        for_auth[4..20].copy_from_slice(&request.authenticator);
        assert_eq!(&response_bytes[4..20], &md5_with_secret(&for_auth, SECRET));

        let wrong = RadiusPacket::create_accounting_request(3, vec![(40, vec![0, 0, 0, 1])], b"other-secret");
        assert!(server.handle_acct(&wrong, LOCALHOST).await.is_none());
    }

    /// Runs the RFC 2865 section 7.1 Access-Request through a server with `script`
    /// and returns the parsed response.
    async fn rfc_request_response(script: &str) -> (RadiusPacket, Vec<u8>) {
        let server = script_server("xyzzy5461", script).await;
        let bytes = server.handle_auth(&hex(RFC_REQUEST), LOCALHOST).await.expect("request was discarded");
        (RadiusPacket::parse(&bytes).unwrap(), bytes)
    }

    #[tokio::test]
    async fn test_lua_reads_rfc2865_request_attributes() {
        let (response, _) = rfc_request_response("
            if get_attr('User-Name') ~= 'nemo' then return 'reject' end
            if get_attr('User-Password') ~= 'arctangent' then return 'reject' end
            if get_attr('NAS-IP-Address') ~= '192.168.1.16' then return 'reject' end
            if get_attr('NAS-Port') ~= 3 then return 'reject' end
            if get_attr('Filter-Id') ~= nil then return 'reject' end
            return 'accept'").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
    }

    #[test]
    fn test_decrypt_user_password() {
        let packet = RadiusPacket::parse(&hex(RFC_REQUEST)).unwrap();
        let cipher = packet.attribute(USER_PASSWORD).unwrap();
        assert_eq!(decrypt_user_password(cipher, SECRET, &packet.authenticator), b"arctangent");
    }

    #[tokio::test]
    async fn test_lua_reads_vendor_specific_attribute() {
        let server = script_server("xyzzy5461", "
            if get_attr('Cisco-AVPair') == 'shell:priv-lvl=15' then return 'accept' end
            return 'reject'").await;
        let mut vsa = 9u32.to_be_bytes().to_vec(); // Cisco
        let avpair = b"shell:priv-lvl=15";
        vsa.extend_from_slice(&[1, (avpair.len() + 2) as u8]);
        vsa.extend_from_slice(avpair);
        let request = RadiusPacket::create_access_request(1, [3u8; 16], vec![(1, b"nemo".to_vec()), (VENDOR_SPECIFIC, vsa)], SECRET);
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_ACCEPT);
    }

    #[tokio::test]
    async fn test_lua_response_attributes_are_encoded() {
        let (response, bytes) = rfc_request_response("
            set_attr('Filter-Id', 'std.ppp')
            set_attr('Service-Type', 'Framed-User')
            return 'accept'").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attribute(11), Some(&b"std.ppp"[..]));
        assert_eq!(response.attribute(6), Some(&[0, 0, 0, 2][..]));
        assert_eq!(response.attributes[0].0, MESSAGE_AUTHENTICATOR);

        let request = RadiusPacket::parse(&hex(RFC_REQUEST)).unwrap();
        let mut for_auth = bytes.clone();
        for_auth[4..20].copy_from_slice(&request.authenticator);
        assert_eq!(&bytes[4..20], &md5_with_secret(&for_auth, SECRET));
    }

    /// A server with clients `nas-a` and `nas-b` at 127.0.0.1 and 127.0.0.2, which `trace` covers.
    fn traced_server(script: &str, trace: Option<crate::config::TraceSettings>) -> RadiusServer {
        let mut clients = ClientTable::default();
        for (address, name) in [("127.0.0.1", "nas-a"), ("127.0.0.2", "nas-b")] {
            clients.add(address, Some(name.to_string()), SECRET.to_vec(), crate::clients::ClientOptions { require_message_authenticator: Policy::No, ..crate::clients::ClientOptions::default() }).unwrap();
        }
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap());
        let state = RadiusState::new(Arc::new(clients), engine, None).with_trace(trace.as_ref());
        RadiusServer::from_state(any_addr(), any_addr(), state, share_dictionary())
    }

    fn trace_of(users: &[&str], clients: &[&str]) -> Option<crate::config::TraceSettings> {
        Some(crate::config::TraceSettings { users: users.iter().map(|name| name.to_string()).collect(), clients: clients.iter().map(|name| name.to_string()).collect(), duration: Duration::from_secs(60) })
    }

    fn pap(identifier: u8, user: &str, password: &str) -> Vec<u8> {
        let ra = [identifier; 16];
        RadiusPacket::create_access_request(identifier, ra, vec![(1, user.as_bytes().to_vec()), (USER_PASSWORD, encrypt_password(password, &ra)), (31, b"aa-bb".to_vec())], SECRET)
    }

    fn from(address: &str) -> SocketAddr {
        SocketAddr::new(address.parse().unwrap(), 40000)
    }

    /// A client source for tests that says the same of every source: `answer` makes what it
    /// says, with `ap-9` at 127.0.0.9 for `Found`.
    struct FixedSource(fn(Arc<ClientTable>) -> crate::client_source::Answer);

    impl FixedSource {
        fn answer(&self) -> Result<crate::client_source::Answer, String> {
            let mut table = ClientTable::default();
            table.add("127.0.0.9", Some("ap-9".to_string()), SECRET.to_vec(), crate::clients::ClientOptions { require_message_authenticator: Policy::No, ..crate::clients::ClientOptions::default() }).map_err(|e| e.to_string())?;
            Ok((self.0)(Arc::new(table)))
        }
    }

    impl crate::client_source::ClientSource for FixedSource {
        fn table_for<'a>(&'a self, _source: IpAddr) -> futures::future::BoxFuture<'a, Option<Arc<ClientTable>>> {
            Box::pin(async move {
                match self.answer() {
                    Ok(crate::client_source::Answer::Found(table)) => Some(table),
                    _ => None,
                }
            })
        }

        fn answer_for<'a>(&'a self, _source: IpAddr) -> futures::future::BoxFuture<'a, crate::client_source::Answer> {
            Box::pin(async move { self.answer().unwrap_or(crate::client_source::Answer::Unknown) })
        }
    }

    /// `traced_server`, with `source` as its client source.
    fn server_with_source(source: FixedSource) -> RadiusServer {
        let server = traced_server("return 'accept'", None);
        let state = server.state();
        server.replace_state(RadiusState::new(state.clients.clone(), state.engine.clone(), None).with_client_source(Some(Arc::new(source))));
        server
    }

    /// A source no configured client covers is identified in what the client source gives, or
    /// discarded as unknown when it gives nothing; a configured client never asks it.
    #[tokio::test]
    async fn test_client_source_for_unlisted_sources() -> Result<(), String> {
        let found = server_with_source(FixedSource(crate::client_source::Answer::Found));
        let answer = found.receive(PacketKind::Auth, &pap(1, "alice", "x"), from("127.0.0.9")).await.ok_or("no answer for the source found")?;
        assert_eq!(answer.first(), Some(&ACCESS_ACCEPT));
        assert!(found.receive(PacketKind::Auth, &pap(2, "alice", "x"), from("127.0.0.1")).await.is_some(), "the configured client nas-a");
        for nothing in [FixedSource(|_| crate::client_source::Answer::NotFound), FixedSource(|_| crate::client_source::Answer::Unknown)] {
            let server = server_with_source(nothing);
            let unknown = server.metrics.radius_discarded_unknown_client.get();
            assert!(server.receive(PacketKind::Auth, &pap(3, "alice", "x"), from("127.0.0.9")).await.is_none());
            assert_eq!(server.metrics.radius_discarded_unknown_client.get(), unknown + 1);
        }
        Ok(())
    }

    /// A spooled request of a client the configuration does not list: replayed when the client
    /// source finds the client under the same name, refused when it finds another name or none,
    /// and kept for later when it cannot tell now.
    #[tokio::test]
    async fn test_client_source_for_replays() -> Result<(), String> {
        let source: IpAddr = "127.0.0.9".parse().map_err(|e: std::net::AddrParseError| e.to_string())?;
        let spooled = SpooledRequest { client: "ap-9".to_string(), source, received: now_unix() as u64 - 3, packet: acct_request(1, None), radsec: false };
        let found = server_with_source(FixedSource(crate::client_source::Answer::Found));
        assert!(matches!(found.replay_one(&spooled).await, Replayed::Accepted));
        let renamed = SpooledRequest { client: "ap-old".to_string(), ..spooled.clone() };
        assert!(matches!(found.replay_one(&renamed).await, Replayed::Refused(_)));
        assert!(matches!(server_with_source(FixedSource(|_| crate::client_source::Answer::NotFound)).replay_one(&spooled).await, Replayed::Refused(_)));
        assert!(matches!(server_with_source(FixedSource(|_| crate::client_source::Answer::Unknown)).replay_one(&spooled).await, Replayed::TryAgain));
        Ok(())
    }

    /// Takes requests whose User-Name starts with `pre-outcome-`, with what the rest of it names.
    struct FakePreChain;

    impl crate::pre_chain::PreChainAuthenticator for FakePreChain {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn take<'a>(&'a self, request: &'a crate::pre_chain::PreChainRequest<'a>) -> Option<BoxFuture<'a, crate::pre_chain::Identified>> {
            use crate::pre_chain::Identified;
            let outcome = request.user.strip_prefix("pre-outcome-")?;
            let identified = match outcome {
                "found" | "found-rejected" => Identified::Found { identity: format!("proven-{outcome}"), reply: vec![("Tunnel-Password".to_string(), AttrValue::Bytes(b"the-key".to_vec()))] },
                "refused" => Identified::Refused { log: Some(format!("fake refused {} from {}", request.user, request.client.name)) },
                "unavailable" => Identified::Unavailable { log: "fake could not read its table".to_string() },
                _ => Identified::Busy,
            };
            Some(Box::pin(async move { identified }))
        }
    }

    #[tokio::test]
    async fn test_pre_chain_outcomes() -> Result<(), String> {
        let server = traced_server("if eap_method == 'fake' then return get_attr('User-Name') == 'proven-found' and 'accept' or 'reject' end return 'accept'", None);
        let state = server.state();
        server.replace_state(RadiusState::new(state.clients.clone(), state.engine.clone(), None).with_pre_chain(Some(Arc::new(FakePreChain))));
        let ask = |identifier: u8, user: &'static str| {
            let server = &server;
            async move { server.receive(PacketKind::Auth, &pap(identifier, user, "unused"), from("127.0.0.1")).await }
        };
        let (mut found, mut rejected, mut refused) = (None, None, None);
        let log = logged_during(async {
            found = ask(1, "pre-outcome-found").await;
            rejected = ask(2, "pre-outcome-found-rejected").await;
            refused = ask(3, "pre-outcome-refused").await;
        })
        .await;

        // Found and accepted by the chain: the reply carries what the authenticator gave.
        let found = RadiusPacket::parse(&found.ok_or("no answer to found")?).map_err(|e| format!("{e:?}"))?;
        assert_eq!(found.code, ACCESS_ACCEPT, "the chain saw the proven identity with the authenticator's method");
        let (_, _, key) = decrypt_tunnel_password(found.attribute(69).ok_or("no Tunnel-Password")?, SECRET, &[1; 16]);
        assert_eq!(key, b"the-key");
        let record = crate::logging::records_for("pre-outcome-found");
        assert_eq!((record.len(), record[0].handler.as_str(), record[0].account.as_str(), record[0].decision.as_str()), (1, "fake", "proven-found", "accept"));
        // Found, refused by the chain: no key in the rejection.
        let rejected = RadiusPacket::parse(&rejected.ok_or("no answer to found-rejected")?).map_err(|e| format!("{e:?}"))?;
        assert_eq!(rejected.code, ACCESS_REJECT);
        assert!(rejected.attribute(69).is_none(), "a key in a rejection");
        assert_eq!(crate::logging::records_for("pre-outcome-found-rejected")[0].handler, "fake");
        // Refused: rejected without the chain, with the authenticator's log line.
        assert_eq!(refused.ok_or("no answer to refused")?[0], ACCESS_REJECT);
        let record = crate::logging::records_for("pre-outcome-refused");
        assert_eq!((record[0].handler.as_str(), record[0].account.as_str(), record[0].decision.as_str()), ("fake", "", "reject"));
        assert!(log.contains("fake refused pre-outcome-refused from nas-a"), "{log}");

        // Unavailable: not answered, counted, logged; busy: not answered, not counted.
        let discarded = server.metrics.radius_discarded_database.get();
        let log = logged_during(async {
            assert!(ask(4, "pre-outcome-unavailable").await.is_none());
        })
        .await;
        assert_eq!(server.metrics.radius_discarded_database.get(), discarded + 1);
        assert!(log.contains("fake could not read its table"), "{log}");
        assert!(ask(5, "pre-outcome-busy").await.is_none());
        assert_eq!(server.metrics.radius_discarded_database.get(), discarded + 1);
        assert!(crate::logging::records_for("pre-outcome-unavailable").is_empty() && crate::logging::records_for("pre-outcome-busy").is_empty());

        // Not taken: the script decides, as without an authenticator.
        assert_eq!(ask(6, "pre-not-taken").await.ok_or("no answer to a request not taken")?[0], ACCESS_ACCEPT);
        assert_ne!(crate::logging::records_for("pre-not-taken")[0].handler, "fake");

        // Found, but the chain could not use the database: not answered, counted, and logged under
        // the authenticator's name.
        let slow = "local ok = pcall(sql_query, [[WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5000000) SELECT max(x) FROM c]]) return 'reject'";
        server.replace_state(RadiusState::new(state.clients.clone(), engine_with(slow, Duration::from_millis(5)), None).with_pre_chain(Some(Arc::new(FakePreChain))));
        let discarded = server.metrics.radius_discarded_database.get();
        let log = logged_during(async {
            assert!(ask(7, "pre-outcome-found").await.is_none());
        })
        .await;
        assert_eq!(server.metrics.radius_discarded_database.get(), discarded + 1);
        assert!(log.contains("RADIUS FAKE request from client nas-a not answered: the cluster database could not be used"), "{log}");
        Ok(())
    }

    /// Without a pre-chain authenticator, as in this program, a request carrying a 4-way
    /// handshake is an ordinary one: the script decides it.
    #[tokio::test]
    async fn test_handshake_without_an_authenticator() -> Result<(), String> {
        let server = traced_server("return get_attr('User-Name') == 'handshake-plain' and 'accept' or 'reject'", None);
        let attributes = [
            ("User-Name", AttrValue::Bytes(b"handshake-plain".to_vec())),
            ("Called-Station-Id", AttrValue::Bytes(b"02-00-00-00-00-01:office".to_vec())),
            ("FreeRADIUS-802.1X-Anonce", AttrValue::Bytes(vec![0x10; 32])),
            ("FreeRADIUS-802.1X-EAPoL-Key-Msg", AttrValue::Bytes(vec![0x02; 121])),
        ];
        let encoded = encode_response_attributes(&share_dictionary(), attributes.iter().map(|(name, value)| (*name, value)), SECRET, &[0; 16], &mut Vec::new())?;
        let request = RadiusPacket::create_access_request(8, [8; 16], encoded, SECRET);
        let answer = server.receive(PacketKind::Auth, &request, from("127.0.0.1")).await.ok_or("no answer")?;
        assert_eq!(answer[0], ACCESS_ACCEPT);
        let record = crate::logging::records_for("handshake-plain");
        assert!(record.len() == 1 && record[0].handler != "dpsk", "{record:?}");
        Ok(())
    }

    #[tokio::test]
    async fn test_trace_of_a_listed_user() {
        let server = traced_server("set_attr('Filter-Id', 'staff') set_attr('Tunnel-Password', 'tunnel-secret') return 'accept'", trace_of(&["alice"], &[]));
        let log = logged_during(async {
            assert!(server.receive(PacketKind::Auth, &pap(1, "alice", "hunter2"), from("127.0.0.1")).await.is_some());
            assert!(server.receive(PacketKind::Auth, &pap(2, "bob", "other-secret"), from("127.0.0.1")).await.is_some());
        })
        .await;
        assert!(log.contains("rradius_trace") && log.contains("User-Name = alice") && log.contains("Calling-Station-Id = aa-bb"), "{log}");
        assert!(log.contains("User-Password = <hidden>"), "{log}");
        assert!(log.contains("Filter-Id = staff"), "the response: {log}");
        assert!(log.contains("Tunnel-Password = <hidden>"), "{log}");
        assert!(!log.contains("hunter2"), "{log}");
        assert!(!log.contains("User-Name = bob") && !log.contains("other-secret"), "{log}");
        assert_eq!(log.lines().filter(|line| line.contains("rradius_trace")).count(), 2, "{log}");
    }

    #[tokio::test]
    async fn test_trace_hides_what_proves_something() {
        let server = traced_server("return 'accept'", trace_of(&["alice"], &[]));
        let attributes = vec![(1, b"alice".to_vec()), (3, vec![7; 17]), (60, b"chap-challenge-xx".to_vec()), (24, b"state-state".to_vec()), (79, vec![2, 1, 0, 5, 1])];
        let request = RadiusPacket::create_access_request(1, [1; 16], attributes, SECRET);
        let log = logged_during(async {
            server.receive(PacketKind::Auth, &request, from("127.0.0.1")).await;
        })
        .await;
        for name in ["CHAP-Password", "CHAP-Challenge", "State", "EAP-Message", "Message-Authenticator"] {
            assert!(log.contains(&format!("{name} = <hidden>")), "{name}: {log}");
        }
        assert!(!log.contains("chap-challenge-xx") && !log.contains("state-state"), "{log}");

        // Microsoft's, in a Vendor-Specific, and what changes or carries a password elsewhere.
        let microsoft = |kind: u8, value: &[u8]| (26u8, [&311u32.to_be_bytes()[..], &[kind, value.len() as u8 + 2], value].concat());
        let attributes = vec![
            (1, b"alice".to_vec()),
            microsoft(11, &[1; 8]),
            microsoft(25, &[2; 50]),
            microsoft(27, &[3; 68]),
            microsoft(6, &[4; 20]),
            (70, vec![5; 16]),
            (206, b"digest-response-value".to_vec()),
        ];
        let request = RadiusPacket::create_access_request(2, [2; 16], attributes, SECRET);
        let log = logged_during(async {
            server.receive(PacketKind::Auth, &request, from("127.0.0.1")).await;
        })
        .await;
        for name in ["MS-CHAP-Challenge", "MS-CHAP2-Response", "MS-CHAP2-CPW", "MS-CHAP-NT-Enc-PW", "ARAP-Password", "Digest-Response"] {
            assert!(log.contains(&format!("{name} = <hidden>")), "{name}: {log}");
        }
        assert!(!log.contains("digest-response-value") && !log.contains("0x0202") && !log.contains("0x0303") && !log.contains("0x0505"), "{log}");
    }

    #[test]
    fn test_trace_hides_whatever_the_dictionary_says() {
        // The password is decrypted by its number, so a dictionary without the option must not show it.
        let mut dictionary = Dictionary::new();
        dictionary.add_attribute("UserPwd", "string", 0, vec![2], None);
        dictionary.add_attribute("Session", "string", 0, vec![24], None);
        dictionary.add_attribute("Plain", "string", 0, vec![30], None);
        dictionary.add_attribute("Marked", "octets", 0, vec![4], Some("secret".to_string()));
        dictionary.add_attribute("With-Others", "octets", 0, vec![5], Some("has_tag,encrypt=2".to_string()));
        let text = |name: &str| (name.to_string(), AttrValue::Bytes(b"visible-or-not".to_vec()));
        let shown = trace_attributes(&dictionary, &[text("UserPwd"), text("Session"), text("Marked"), text("With-Others"), text("Plain")]);
        assert_eq!(shown, "UserPwd = <hidden>, Session = <hidden>, Marked = <hidden>, With-Others = <hidden>, Plain = visible-or-not");
        // Decrypted by its number, and through the whole request path.
        let packet = RadiusPacket { code: 1, identifier: 0, authenticator: [0; 16], attributes: vec![(2, vec![9; 16])] };
        assert_eq!(trace_attributes(&dictionary, &request_attributes(&packet, &dictionary, b"s")), "UserPwd = <hidden>");
        let bundled = share_test_dictionary();
        let shown = trace_attributes(&bundled, &[text("Acc-Tunnel-Secret"), text("3GPP2-Pre-Shared-Secret"), text("Filter-Id"), text("User-Name"), text("Calling-Station-Id")]);
        assert_eq!(shown, "Acc-Tunnel-Secret = <hidden>, 3GPP2-Pre-Shared-Secret = <hidden>, Filter-Id = visible-or-not, User-Name = visible-or-not, Calling-Station-Id = visible-or-not");
        for name in ["Mikrotik-Wireless-PSK", "Ruckus-DPSK", "Extreme-PPSK-PMK", "Aruba-AS-Credential-Hash", "Aruba-MPSK-Passphrase", "User-Password", "Tunnel-Password"] {
            assert_eq!(trace_attributes(&bundled, &[text(name)]), format!("{name} = <hidden>"));
        }
    }

    #[tokio::test]
    async fn test_trace_of_a_listed_client() {
        let server = traced_server("return 'accept'", trace_of(&[], &["nas-b"]));
        let accounting = |identifier, user: &[u8]| RadiusPacket::create_accounting_request(identifier, vec![(1, user.to_vec()), (40, 1u32.to_be_bytes().to_vec())], SECRET);
        let log = logged_during(async {
            assert!(server.receive(PacketKind::Acct, &accounting(1, b"seen"), from("127.0.0.2")).await.is_some());
            assert!(server.receive(PacketKind::Acct, &accounting(2, b"unseen"), from("127.0.0.1")).await.is_some());
        })
        .await;
        assert!(log.contains("User-Name = seen"), "{log}");
        assert!(!log.contains("unseen"), "{log}");
        assert_eq!(log.lines().filter(|line| line.contains("rradius_trace")).count(), 2, "{log}");
    }

    #[tokio::test]
    async fn test_trace_stops_when_time_is_up() {
        let mut settings = trace_of(&["alice"], &[]).unwrap();
        settings.duration = Duration::from_millis(500);
        let server = traced_server("return 'accept'", Some(settings));
        let log = logged_during(async {
            server.receive(PacketKind::Auth, &pap(1, "alice", "x"), from("127.0.0.1")).await;
        })
        .await;
        assert!(log.contains("rradius_trace"), "on at first: {log}");
        tokio::time::sleep(Duration::from_millis(600)).await;
        let log = logged_during(async {
            server.receive(PacketKind::Auth, &pap(2, "alice", "x"), from("127.0.0.1")).await;
        })
        .await;
        assert!(!log.contains("rradius_trace"), "{log}");
    }

    #[tokio::test]
    async fn test_trace_with_no_response() {
        let server = traced_server("return 'reject'", trace_of(&[], &["nas-a"]));
        let request = RadiusPacket::create_accounting_request(1, vec![(1, b"alice".to_vec()), (40, 1u32.to_be_bytes().to_vec())], SECRET);
        // A script that does not accept accounting gets no answer.
        let log = logged_during(async {
            assert!(server.receive(PacketKind::Acct, &request, from("127.0.0.1")).await.is_none());
        })
        .await;
        assert!(log.contains("no response"), "{log}");
    }

    #[tokio::test]
    async fn test_trace_shows_binary_values_as_hex() {
        let dictionary = share_test_dictionary();
        let shown = trace_attributes(&dictionary, &[("Class".to_string(), AttrValue::Bytes(vec![0, 255])), ("Filter-Id".to_string(), AttrValue::Bytes(vec![b'a'; 300]))]);
        assert!(shown.contains("Class = 0x00ff"), "{shown}");
        assert!(shown.contains(&format!("Filter-Id = {}", "a".repeat(128))) && !shown.contains(&"a".repeat(129)), "{shown}");
    }

    #[tokio::test]
    async fn test_client_responses() {
        let server = traced_server("return get_attr('User-Name') == 'good' and 'accept' or 'reject'", None);
        assert!(server.metrics.render().contains("rradius_radius_client_responses_total{client=\"nas-b\",code=\"access_accept\"} 0"), "from the start");
        assert!(server.receive(PacketKind::Auth, &pap(1, "good", "x"), from("127.0.0.1")).await.is_some());
        assert!(server.receive(PacketKind::Auth, &pap(2, "bad", "x"), from("127.0.0.2")).await.is_some());
        let rendered = server.metrics.render();
        for (client, code, value) in [("nas-a", "access_accept", 1), ("nas-a", "access_reject", 0), ("nas-b", "access_accept", 0), ("nas-b", "access_reject", 1), ("nas-a", "accounting_response", 0), ("nas-a", "access_challenge", 0)] {
            let line = format!("rradius_radius_client_responses_total{{client=\"{client}\",code=\"{code}\"}} {value}");
            assert!(rendered.contains(&line), "{line}: {rendered}");
        }
        // A retransmission is not another response, and a request answered is counted once.
        assert!(server.receive(PacketKind::Auth, &pap(1, "good", "x"), from("127.0.0.1")).await.is_some());
        assert_eq!(server.metrics.radius_client("nas-a").unwrap().access_accept.get(), 1);
        // A reload takes away who is gone, and adds who is new.
        let mut clients = ClientTable::default();
        clients.add("127.0.0.3", Some("nas-c".to_string()), SECRET.to_vec(), crate::clients::ClientOptions::default()).unwrap();
        server.replace_state(RadiusState::new(Arc::new(clients), server.state().engine.clone(), None));
        let rendered = server.metrics.render();
        assert!(!rendered.contains("client=\"nas-a\""), "{rendered}");
        assert!(rendered.contains("client=\"nas-c\",code=\"access_accept\"} 0"), "{rendered}");
    }

    #[tokio::test]
    async fn test_client_responses_of_a_client_removed_during_a_request() {
        let server = traced_server("return 'accept'", None);
        // What a reload does while this client's request is still in progress.
        server.metrics.set_radius_clients(["nas-b"]);
        assert!(server.receive(PacketKind::Auth, &pap(1, "alice", "x"), from("127.0.0.1")).await.is_some());
        assert!(!server.metrics.render().contains("client=\"nas-a\""));
        assert_eq!(server.metrics.radius_access_accept.get(), 1, "still counted overall");
    }

    #[test]
    fn test_client_responses_escapes_names() {
        let metrics = Metrics::default();
        metrics.set_radius_clients(["a\"b\\c"]);
        assert!(metrics.render().contains("client=\"a\\\"b\\\\c\",code="), "{}", metrics.render());
    }

    /// Sends a request to `target` from `local` until there is an answer, and returns its source.
    async fn answered_from(local: &str, target: SocketAddr, request: &[u8], running: &tokio::task::JoinHandle<io::Result<()>>) -> (SocketAddr, u8) {
        let socket = UdpSocket::bind(local).await.unwrap();
        let mut buffer = [0u8; 4096];
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                assert!(!running.is_finished(), "the server stopped: it could not bind");
                socket.send_to(request, target).await.unwrap();
                if let Ok(Ok((size, from))) = tokio::time::timeout(Duration::from_millis(100), socket.recv_from(&mut buffer)).await {
                    return (from, RadiusPacket::parse(&buffer[..size]).unwrap().code);
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no answer from {target}"))
    }

    /// A server on two addresses, with both ports of them to send to. `None` with no IPv6 here.
    async fn server_on(first: &str, second: &str) -> Option<(u16, u16, tokio::task::JoinHandle<io::Result<()>>)> {
        UdpSocket::bind("[::1]:0").await.ok()?;
        let free = || std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (auth_port, acct_port) = (free(), free());
        let mut clients = ClientTable::default();
        for address in ["127.0.0.1", "::1"] {
            clients.add(address, Some(address.to_string()), SECRET.to_vec(), crate::clients::ClientOptions { require_message_authenticator: Policy::No, ..crate::clients::ClientOptions::default() }).unwrap();
        }
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").unwrap());
        let ip: IpAddr = first.parse().unwrap();
        let server = RadiusServer::new(SocketAddr::new(ip, auth_port), SocketAddr::new(ip, acct_port), Arc::new(clients), engine, share_dictionary()).with_more_binds(vec![second.parse().unwrap()]);
        Some((auth_port, acct_port, tokio::spawn(Arc::new(server).run())))
    }

    #[tokio::test]
    async fn test_several_addresses() {
        // Both wildcards, on one port: only if `::` keeps to IPv6. Linux refuses it otherwise.
        let Some((auth_port, _, running)) = server_on("0.0.0.0", "::").await else { return };
        for (local, target) in [("127.0.0.1:0", "127.0.0.1"), ("[::1]:0", "::1")] {
            let target = SocketAddr::new(target.parse().unwrap(), auth_port);
            assert_eq!(answered_from(local, target, &pap(1, "alice", "x"), &running).await.1, ACCESS_ACCEPT);
        }
    }

    #[tokio::test]
    async fn test_two_addresses_each_serve_both_ports() {
        let Some((auth_port, acct_port, running)) = server_on("127.0.0.1", "::1").await else { return };
        let accounting = RadiusPacket::create_accounting_request(2, vec![(1, b"alice".to_vec()), (40, 1u32.to_be_bytes().to_vec())], SECRET);
        for (local, address) in [("127.0.0.1:0", "127.0.0.1"), ("[::1]:0", "::1")] {
            let ip: IpAddr = address.parse().unwrap();
            let target = SocketAddr::new(ip, auth_port);
            assert_eq!(answered_from(local, target, &pap(1, "alice", "x"), &running).await, (target, ACCESS_ACCEPT), "from the address asked");
            let target = SocketAddr::new(ip, acct_port);
            assert_eq!(answered_from(local, target, &accounting, &running).await, (target, ACCOUNTING_RESPONSE), "from the address asked");
        }
    }

    /// A server on 127.0.0.1 with `sockets` sockets on each port, whose script puts a fresh
    /// token in every reply, so a reply sent again from the cache shows.
    fn server_with_sockets(sockets: usize) -> (u16, u16, RadiusServer) {
        let free = || std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (auth_port, acct_port) = (free(), free());
        let mut clients = ClientTable::default();
        clients.add("127.0.0.1", Some("local".to_string()), SECRET.to_vec(), crate::clients::ClientOptions { require_message_authenticator: Policy::No, ..crate::clients::ClientOptions::default() }).unwrap();
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "set_attr('Reply-Message', random_token()) return 'accept'").unwrap());
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let server = RadiusServer::new(SocketAddr::new(ip, auth_port), SocketAddr::new(ip, acct_port), Arc::new(clients), engine, share_dictionary()).with_sockets(sockets);
        (auth_port, acct_port, server)
    }

    /// A request that has to wait for the database does not hold up the next one on the same
    /// socket: neither when it arrives alone and is tried on the receive loop first, nor when
    /// the next one is already waiting and it goes straight to a task of its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_slow_request_does_not_hold_up_the_socket() -> Result<(), String> {
        let free = || std::net::UdpSocket::bind("127.0.0.1:0").and_then(|socket| socket.local_addr()).map(|address| address.port()).map_err(|e| e.to_string());
        let (auth_port, acct_port) = (free()?, free()?);
        let mut clients = ClientTable::default();
        clients.add("127.0.0.1", Some("local".to_string()), SECRET.to_vec(), crate::clients::ClientOptions { require_message_authenticator: Policy::No, ..crate::clients::ClientOptions::default() }).map_err(|e| e.to_string())?;
        let slow = "if get_attr('User-Name') == 'slow' then sql_query('WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 3000000) SELECT count(*) AS n FROM c') end return 'accept'";
        let engine = LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), slow).map_err(|e| e.to_string())?.with_query_timeout(Duration::from_secs(120));
        let ip: IpAddr = "127.0.0.1".parse().map_err(|e| format!("{e}"))?;
        let server = RadiusServer::new(SocketAddr::new(ip, auth_port), SocketAddr::new(ip, acct_port), Arc::new(clients), Arc::new(engine), share_dictionary()).with_sockets(1);
        let running = tokio::spawn(Arc::new(server).run());
        let target = SocketAddr::new(ip, auth_port);
        answered_from("127.0.0.1:0", target, &pap(200, "quick", "x"), &running).await;
        for pause in [Some(Duration::from_millis(30)), None] {
            let socket = UdpSocket::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
            socket.send_to(&pap(1, "slow", "x"), target).await.map_err(|e| e.to_string())?;
            if let Some(pause) = pause {
                tokio::time::sleep(pause).await;
            }
            socket.send_to(&pap(2, "quick", "x"), target).await.map_err(|e| e.to_string())?;
            let mut buffer = [0u8; 4096];
            let mut identifiers = Vec::new();
            for _ in 0..2 {
                let (size, _) = tokio::time::timeout(Duration::from_secs(60), socket.recv_from(&mut buffer)).await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
                identifiers.push(buffer.get(1).copied().filter(|_| size > 1).ok_or("short answer")?);
            }
            assert_eq!(identifiers, [2, 1], "{pause:?}");
        }
        running.abort();
        Ok(())
    }

    #[tokio::test]
    async fn test_sockets_share_each_port() {
        let (auth_port, acct_port, server) = server_with_sockets(4);
        let sockets = server.bind_sockets().await.unwrap();
        let addresses: Vec<(PacketKind, SocketAddr)> = sockets.iter().map(|(kind, socket)| (*kind, socket.local_addr().unwrap())).collect();
        let on = |kind: PacketKind, port: u16| addresses.iter().filter(|(k, address)| *k == kind && address.port() == port).count();
        assert_eq!((addresses.len(), on(PacketKind::Auth, auth_port), on(PacketKind::Acct, acct_port)), (8, 4, 4), "{addresses:?}");

        let (auth_port, acct_port, server) = server_with_sockets(1);
        let sockets = server.bind_sockets().await.unwrap();
        assert_eq!(sockets.len(), 2);
        // Without SO_REUSEPORT, the port is not shared, even with another socket that asks to.
        for port in [auth_port, acct_port] {
            let other = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP)).unwrap();
            other.set_reuse_port(true).unwrap();
            let address: SocketAddr = SocketAddr::new("127.0.0.1".parse().unwrap(), port);
            assert!(other.bind(&address.into()).is_err(), "port {port} was shared");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_requests_over_several_sockets() {
        let (auth_port, _, server) = server_with_sockets(4);
        let running = tokio::spawn(Arc::new(server).run());
        let target = SocketAddr::new("127.0.0.1".parse().unwrap(), auth_port);
        // The first answer says it serves.
        let (from, code) = answered_from("127.0.0.1:0", target, &pap(200, "alice", "x"), &running).await;
        assert_eq!((from, code), (target, ACCESS_ACCEPT));
        let clients = (0..50u8).map(|client| {
            tokio::spawn(async move {
                let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let mut buffer = [0u8; 4096];
                let mut replies = Vec::new();
                for identifier in 0..4u8 {
                    let request = pap(client.wrapping_mul(4).wrapping_add(identifier), "alice", "x");
                    socket.send_to(&request, target).await.unwrap();
                    let (size, from) = tokio::time::timeout(Duration::from_secs(5), socket.recv_from(&mut buffer)).await.expect("no answer").unwrap();
                    replies.push((from, buffer[..size].to_vec()));
                }
                // Sent again: the reply from the cache, with the token the script gave the first time.
                let request = pap(client.wrapping_mul(4), "alice", "x");
                socket.send_to(&request, target).await.unwrap();
                let (size, from) = tokio::time::timeout(Duration::from_secs(5), socket.recv_from(&mut buffer)).await.expect("no answer").unwrap();
                (replies, (from, buffer[..size].to_vec()))
            })
        });
        let mut answered = 0;
        for client in futures::future::join_all(clients).await {
            let (replies, again) = client.unwrap();
            for (from, reply) in &replies {
                assert_eq!((*from, RadiusPacket::parse(reply).unwrap().code), (target, ACCESS_ACCEPT));
                answered += 1;
            }
            assert_eq!(again, replies[0], "the original reply, not a new run of the script");
        }
        assert_eq!(answered, 200);
    }

    /// What these attributes come to on the wire, with the bundled dictionary or `dictionary`.
    fn wire(dictionary: &Dictionary, attributes: &[(&str, AttrValue)]) -> Result<Vec<(u8, Vec<u8>)>, String> {
        encode_response_attributes(dictionary, attributes.iter().map(|(name, value)| (*name, value)), b"s", &[0; 16], &mut Vec::new())
    }

    /// What a script would read from a request holding these attributes.
    fn read(dictionary: &Dictionary, attributes: Vec<(u8, Vec<u8>)>) -> Vec<(String, AttrValue)> {
        let packet = RadiusPacket { code: 1, identifier: 0, authenticator: [0; 16], attributes };
        request_attributes(&packet, dictionary, b"s")
    }

    fn bytes(value: &str) -> AttrValue {
        AttrValue::Bytes(value.as_bytes().to_vec())
    }

    fn named(name: &str, value: AttrValue) -> (String, AttrValue) {
        (name.to_string(), value)
    }

    fn format_dictionary() -> Dictionary {
        let mut dictionary = Dictionary::new();
        dictionary
            .load_from_str(
                "/nonexistent/dictionary",
                "ATTRIBUTE Filter-Id 11 string\n\
                 ATTRIBUTE Top 173 tlv\n\
                 ATTRIBUTE Top-One 173.1 integer\n\
                 ATTRIBUTE Top-Text 173.2 string\n\
                 ATTRIBUTE Top-Inner 173.3 tlv\n\
                 ATTRIBUTE Top-Inner-One 173.3.1 byte\n\
                 ATTRIBUTE Top-Inner-Two 173.3.2 byte\n\
                 ATTRIBUTE Top-Inner-Text 173.3.3 string\n\
                 ATTRIBUTE Top-Secret 173.4 string encrypt=2\n\
                 ATTRIBUTE Not-A-Tlv 174 string\n\
                 ATTRIBUTE Not-A-Tlv-Child 174.1 string\n\
                 ATTRIBUTE Ext 241 extended\n\
                 ATTRIBUTE Ext-One 241.1 integer\n\
                 ATTRIBUTE Ext-Tagged 241.2 string has_tag\n\
                 ATTRIBUTE Ext-Tlv 241.5 tlv\n\
                 ATTRIBUTE Ext-Tlv-One 241.5.1 integer\n\
                 ATTRIBUTE Ext-Text 241.8 string\n\
                 ATTRIBUTE Ext-Evs 241.26 evs\n\
                 ATTRIBUTE Long 245 long-extended\n\
                 ATTRIBUTE Long-Text 245.1 octets\n\
                 ATTRIBUTE Long-Other 245.2 octets\n\
                 ATTRIBUTE Long-Evs 245.26 evs\n\
                 VENDOR Plain 9\n\
                 VENDOR Two 637 format=2,1\n\
                 VENDOR Big 8164 format=2,2\n\
                 VENDOR None 429 format=4,0\n\
                 VENDOR Cont 24757 format=1,1,c\n\
                 BEGIN-VENDOR Plain\n\
                 ATTRIBUTE Plain-Text 1 string\n\
                 ATTRIBUTE Plain-Tlv 2 tlv\n\
                 ATTRIBUTE Plain-Tlv-One 2.1 byte\n\
                 END-VENDOR Plain\n\
                 BEGIN-VENDOR Plain format=Ext-Evs\n\
                 ATTRIBUTE Evs-Short 1 integer\n\
                 END-VENDOR Plain\n\
                 BEGIN-VENDOR Plain format=Long-Evs\n\
                 ATTRIBUTE Evs-Long 1 octets\n\
                 END-VENDOR Plain\n\
                 BEGIN-VENDOR Two\n\
                 ATTRIBUTE Two-Text 5 string\n\
                 ATTRIBUTE Two-High 300 string\n\
                 END-VENDOR Two\n\
                 BEGIN-VENDOR Big\n\
                 ATTRIBUTE Big-Text 5 string\n\
                 END-VENDOR Big\n\
                 BEGIN-VENDOR None\n\
                 ATTRIBUTE None-Text 5 string\n\
                 END-VENDOR None\n\
                 BEGIN-VENDOR Cont\n\
                 ATTRIBUTE Cont-Text 3 string\n\
                 ATTRIBUTE Cont-Tlv 1 tlv\n\
                 ATTRIBUTE Cont-Tlv-Text 1.1 string\n\
                 ATTRIBUTE Cont-Tlv-Byte 1.2 byte\n\
                 END-VENDOR Cont\n",
            )
            .unwrap();
        dictionary
    }

    #[test]
    fn test_extended_read() {
        let dictionary = share_test_dictionary();
        assert_eq!(read(&dictionary, vec![(241, vec![1, 0, 0, 0, 1])]), vec![named("Frag-Status", AttrValue::Number(1.0))]);
        // One the dictionary does not define, and one with no Extended-Type at all.
        assert_eq!(read(&dictionary, vec![(241, vec![200, 1]), (241, vec![]), (1, b"a".to_vec())]), vec![named("User-Name", bytes("a"))]);
    }

    #[test]
    fn test_extended_write() {
        let dictionary = share_test_dictionary();
        assert_eq!(wire(&dictionary, &[("Response-Length", AttrValue::Number(4096.0))]).unwrap(), vec![(241, vec![3, 0, 0, 0x10, 0])]);
        let dictionary = format_dictionary();
        assert_eq!(wire(&dictionary, &[("Ext-Text", bytes(&"a".repeat(252)))]).unwrap()[0].1.len(), 253);
        let error = wire(&dictionary, &[("Ext-Text", bytes(&"a".repeat(253)))]).unwrap_err();
        assert!(error.contains("Ext-Text") && error.contains("too long"), "{error}");
    }

    #[test]
    fn test_extended_containers_cannot_be_set() {
        let dictionary = share_test_dictionary();
        for name in ["Extended-Attribute-1", "Extended-Attribute-5", "Extended-Vendor-Specific-5", "IPv6-6rd-Configuration"] {
            let error = wire(&dictionary, &[(name, bytes("x"))]).unwrap_err();
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn test_extended_long_fragments() {
        let dictionary = format_dictionary();
        let value: Vec<u8> = (0..300u32).map(|n| n as u8).collect();
        let attributes = wire(&dictionary, &[("Long-Text", AttrValue::Bytes(value.clone()))]).unwrap();
        assert_eq!(attributes.len(), 2);
        assert_eq!((attributes[0].0, attributes[0].1.len(), attributes[0].1[0], attributes[0].1[1]), (245, 253, 1, 0x80), "255 on the wire, with the More flag");
        assert_eq!((attributes[1].0, attributes[1].1[0], attributes[1].1[1]), (245, 1, 0));
        assert_eq!(&attributes[1].1[2..], &value[251..]);
        assert_eq!(read(&dictionary, attributes), vec![named("Long-Text", AttrValue::Bytes(value))], "once, joined");
        // Exactly what one holds, then one more.
        assert_eq!(wire(&dictionary, &[("Long-Text", AttrValue::Bytes(vec![1; 251]))]).unwrap().len(), 1);
        assert_eq!(wire(&dictionary, &[("Long-Text", AttrValue::Bytes(vec![1; 252]))]).unwrap().len(), 2);
        assert_eq!(wire(&dictionary, &[("Long-Text", AttrValue::Bytes(vec![1; 502]))]).unwrap().len(), 2);
        assert_eq!(wire(&dictionary, &[("Long-Text", AttrValue::Bytes(vec![1; 503]))]).unwrap().len(), 3);
        assert_eq!(wire(&dictionary, &[("Long-Text", AttrValue::Bytes(Vec::new()))]).unwrap(), vec![(245, vec![1, 0])]);
    }

    #[test]
    fn test_extended_invalid_fragments() {
        let dictionary = format_dictionary();
        let filter = (11u8, b"f".to_vec());
        let full = |flags: u8| [&[1u8, flags][..], &[7u8; 251]].concat();
        let cases = [
            ("More on a short one", vec![(245, vec![1, 0x80, 7]), (245, vec![1, 0, 8]), filter.clone()]),
            ("More with nothing after", vec![(245, full(0x80)), filter.clone()]),
            ("More, then another attribute of the type", vec![(245, full(0x80)), (245, vec![2, 0, 8]), filter.clone()]),
            ("no Flags", vec![(245, vec![1]), filter.clone()]),
        ];
        for (what, attributes) in cases {
            let names: Vec<String> = read(&dictionary, attributes).into_iter().map(|(name, _)| name).collect();
            assert!(names.contains(&"Filter-Id".to_string()), "{what}: the rest goes on: {names:?}");
            assert!(!names.contains(&"Long-Text".to_string()), "{what}: {names:?}");
        }
        // A fragment that fails takes what follows of the same attribute with it.
        assert_eq!(read(&dictionary, vec![(245, vec![1, 0x80, 7]), (245, vec![1, 0x80, 8]), (245, vec![1, 0, 9])]), vec![]);
        // Reserved flags are ignored.
        assert_eq!(read(&dictionary, vec![(245, vec![1, 0x40, 7])]), vec![named("Long-Text", AttrValue::Bytes(vec![7]))]);
    }

    #[test]
    fn test_extended_evs() {
        let dictionary = format_dictionary();
        assert_eq!(wire(&dictionary, &[("Evs-Short", AttrValue::Number(1.0))]).unwrap(), vec![(241, vec![26, 0, 0, 0, 9, 1, 0, 0, 0, 1])]);
        assert_eq!(read(&dictionary, vec![(241, vec![26, 0, 0, 0, 9, 1, 0, 0, 0, 1])]), vec![named("Evs-Short", AttrValue::Number(1.0))]);
        let value = vec![3u8; 300];
        let attributes = wire(&dictionary, &[("Evs-Long", AttrValue::Bytes(value.clone()))]).unwrap();
        assert_eq!(attributes[0].1[..8], [26, 0x80, 0, 0, 0, 9, 1, 3], "the vendor once");
        assert_eq!(attributes[1].1[..3], [26, 0, 3], "and not in the next");
        assert_eq!(read(&dictionary, attributes), vec![named("Evs-Long", AttrValue::Bytes(value))]);
        // Another vendor's, and too short to hold a vendor.
        assert_eq!(read(&dictionary, vec![(241, vec![26, 0, 0, 0, 10, 1, 0, 0, 0, 1]), (241, vec![26, 0, 0, 0])]), vec![]);
        // The bundled one.
        let dictionary = share_test_dictionary();
        assert_eq!(wire(&dictionary, &[("FreeRADIUS-802.1X-Anonce", AttrValue::Bytes(vec![1]))]).unwrap(), vec![(245, vec![26, 0, 0, 0, 0x2c, 0x50, 1, 1])]);
    }

    #[test]
    fn test_extended_and_tlv_refuse_tags_and_encryption() {
        let dictionary = format_dictionary();
        for name in ["Ext-Tagged", "Top-Secret"] {
            let error = wire(&dictionary, &[(name, bytes("x"))]).unwrap_err();
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn test_tlv_write_together() {
        let dictionary = share_test_dictionary();
        let attributes = wire(&dictionary, &[("IPv6-6rd-IPv4MaskLen", AttrValue::Number(8.0)), ("IPv6-6rd-BR-IPv4-Address", bytes("192.0.2.1"))]).unwrap();
        assert_eq!(attributes, vec![(173, vec![1, 6, 0, 0, 0, 8, 3, 6, 192, 0, 2, 1])]);
        assert_eq!(read(&dictionary, attributes), vec![named("IPv6-6rd-IPv4MaskLen", AttrValue::Number(8.0)), named("IPv6-6rd-BR-IPv4-Address", bytes("192.0.2.1"))]);
    }

    #[test]
    fn test_tlv_something_in_between() {
        let dictionary = format_dictionary();
        let attributes = wire(&dictionary, &[("Top-One", AttrValue::Number(1.0)), ("Filter-Id", bytes("f")), ("Top-One", AttrValue::Number(2.0))]).unwrap();
        assert_eq!(attributes, vec![(173, vec![1, 6, 0, 0, 0, 1]), (11, b"f".to_vec()), (173, vec![1, 6, 0, 0, 0, 2])]);
        // Different ones do not share, either.
        let attributes = wire(&dictionary, &[("Top-One", AttrValue::Number(1.0)), ("Ext-Tlv-One", AttrValue::Number(2.0)), ("Plain-Tlv-One", AttrValue::Number(3.0))]).unwrap();
        assert_eq!(attributes, vec![(173, vec![1, 6, 0, 0, 0, 1]), (241, vec![5, 1, 6, 0, 0, 0, 2]), (26, vec![0, 0, 0, 9, 2, 5, 1, 3, 3])]);
    }

    #[test]
    fn test_tlv_nested() {
        let dictionary = format_dictionary();
        let attributes = wire(&dictionary, &[("Top-One", AttrValue::Number(1.0)), ("Top-Inner-One", AttrValue::Number(2.0)), ("Top-Inner-Two", AttrValue::Number(3.0)), ("Top-Text", bytes("x"))]).unwrap();
        assert_eq!(attributes, vec![(173, vec![1, 6, 0, 0, 0, 1, 3, 8, 1, 3, 2, 2, 3, 3, 2, 3, b'x'])]);
        assert_eq!(
            read(&dictionary, attributes),
            vec![named("Top-One", AttrValue::Number(1.0)), named("Top-Inner-One", AttrValue::Number(2.0)), named("Top-Inner-Two", AttrValue::Number(3.0)), named("Top-Text", bytes("x"))]
        );
    }

    #[test]
    fn test_tlv_new_attribute_when_they_do_not_fit() {
        let dictionary = format_dictionary();
        let long = "a".repeat(200);
        let attributes = wire(&dictionary, &[("Top-Text", bytes(&long)), ("Top-Text", bytes(&long))]).unwrap();
        assert_eq!(attributes.iter().map(|(kind, value)| (*kind, value.len())).collect::<Vec<_>>(), vec![(173, 202), (173, 202)], "no TLV cut in two");
        assert_eq!(wire(&dictionary, &[("Top-Text", bytes(&"a".repeat(251)))]).unwrap()[0].1.len(), 253);
        let error = wire(&dictionary, &[("Top-Text", bytes(&"a".repeat(252)))]).unwrap_err();
        assert!(error.contains("Top-Text") && error.contains("too long"), "{error}");
        // Nothing would be left of a TLV with no value: RFC 6929 section 2.3.
        assert!(wire(&dictionary, &[("Top-Text", bytes(""))]).is_err());
    }

    #[test]
    fn test_tlv_nested_that_do_not_fit() {
        let dictionary = format_dictionary();
        let long = "a".repeat(200);
        // Each fits on its own, so together they must too: the script was told so when it set them.
        let single = |value: &str| wire(&dictionary, &[("Top-Inner-Text", bytes(value))]);
        assert!(single(&long).is_ok());
        let attributes = wire(&dictionary, &[("Top-Inner-Text", bytes(&long)), ("Top-Inner-Text", bytes(&long))]).unwrap();
        assert_eq!(attributes.len(), 2);
        assert_eq!(attributes[0].1[..4], [3, 204, 3, 202], "the inner TLV in each");
        assert_eq!(read(&dictionary, attributes), vec![named("Top-Inner-Text", bytes(&long)), named("Top-Inner-Text", bytes(&long))]);
        // Too much for even one: the error names it, not the one before it.
        let error = wire(&dictionary, &[("Top-One", AttrValue::Number(1.0)), ("Top-Inner-Text", bytes(&"a".repeat(250)))]).unwrap_err();
        assert!(error.contains("Top-Inner-Text") && !error.contains("Top-One"), "{error}");
        let error = wire(&dictionary, &[("Top-Inner-One", AttrValue::Number(1.0)), ("Top-Inner-Text", bytes(&"a".repeat(252)))]).unwrap_err();
        assert!(error.contains("Top-Inner-Text") && !error.contains("Top-Inner-One"), "{error}");
        let error = wire(&dictionary, &[("Top-One", AttrValue::Number(1.0)), ("Top-Text", bytes(&"a".repeat(252)))]).unwrap_err();
        assert!(error.contains("Top-Text") && !error.contains("Top-One"), "{error}");
        // A TLV's own length holds no more, even where the attribute could go on.
        let error = wire(&dictionary, &[("Cont-Tlv-Text", bytes(&"a".repeat(254)))]).unwrap_err();
        assert!(error.contains("Cont-Tlv-Text") && error.contains("too long"), "{error}");
    }

    #[test]
    fn test_tlv_nested_close_to_the_limit() {
        let dictionary = format_dictionary();
        // Each fits alone, and together they are too much for one attribute by a few octets.
        for (one, two) in [(125, 124), (124, 125), (249, 1), (1, 249), (123, 124), (247, 2)] {
            let (one, two) = ("a".repeat(one), "b".repeat(two));
            assert!(wire(&dictionary, &[("Top-Inner-Text", bytes(&one))]).is_ok());
            assert!(wire(&dictionary, &[("Top-Inner-Text", bytes(&two))]).is_ok());
            let attributes = wire(&dictionary, &[("Top-Inner-Text", bytes(&one)), ("Top-Inner-Text", bytes(&two))]).unwrap_or_else(|e| panic!("{} {}: {e}", one.len(), two.len()));
            assert!(attributes.iter().all(|(_, value)| value.len() <= 253));
            assert_eq!(read(&dictionary, attributes), vec![named("Top-Inner-Text", bytes(&one)), named("Top-Inner-Text", bytes(&two))]);
        }
        let split = wire(&dictionary, &[("Top-Inner-Text", bytes(&"a".repeat(125))), ("Top-Inner-Text", bytes(&"b".repeat(124)))]).unwrap();
        assert_eq!(split.len(), 2);
        // The longest one that fits, for how deep it is.
        assert_eq!(wire(&dictionary, &[("Top-Inner-Text", bytes(&"a".repeat(249)))]).unwrap()[0].1.len(), 253);
        let error = wire(&dictionary, &[("Top-Inner-Text", bytes(&"a".repeat(250)))]).unwrap_err();
        assert!(error.contains("Top-Inner-Text") && error.contains("too long"), "{error}");
    }

    #[test]
    fn test_encrypt_option_however_separated() {
        for options in ["has_tag encrypt=2", "has_tag,encrypt=2", "encrypt=2 has_tag", "has_tag, encrypt=2"] {
            let mut dictionary = Dictionary::new();
            dictionary.add_attribute("Spaced", "string", 0, vec![200], Some(options.to_string()));
            assert_eq!(attribute_is_encrypted(&dictionary, "Spaced"), Ok(true), "{options}");
            let sent = wire(&dictionary, &[("Spaced:1", bytes("plain-text-value"))]).unwrap_or_else(|e| panic!("{options}: {e}"));
            assert_eq!(sent[0].1[0], 1, "{options}: the tag");
            assert!(!sent[0].1.windows(5).any(|window| window == b"plain"), "{options}: sent in the clear");
        }
        // As a dictionary file gives them.
        let mut dictionary = Dictionary::new();
        dictionary.load_from_str("spaced", "ATTRIBUTE Spaced 200 string has_tag encrypt=2\n").unwrap();
        let sent = wire(&dictionary, &[("Spaced:1", bytes("plain-text-value"))]).unwrap();
        assert_eq!(sent[0].1[0], 1, "the tag");
        assert!(!sent[0].1.windows(5).any(|window| window == b"plain"), "sent in the clear");
        for one in ["encrypt=1", "encrypt=3"] {
            let mut dictionary = Dictionary::new();
            dictionary.add_attribute("Spaced", "string", 0, vec![200], Some(format!("has_tag {one}")));
            assert!(wire(&dictionary, &[("Spaced", bytes("plain-text-value"))]).is_err(), "{one} cannot be sent, however separated");
        }
    }

    #[test]
    fn test_tlv_nested_in_a_vendor_specific() {
        // The bundled dictionary's: a default vendor holds 247, less than a TLV could.
        let dictionary = share_test_dictionary();
        let value = "a".repeat(120);
        for name in ["FreeRADIUS-EAP-FAST-PAC-Opaque-PAC-Key", "FreeRADIUS-EAP-FAST-PAC-Opaque-I-ID"] {
            assert!(wire(&dictionary, &[(name, bytes(&value))]).is_ok(), "{name}");
        }
        let attributes = wire(&dictionary, &[("FreeRADIUS-EAP-FAST-PAC-Opaque-PAC-Key", bytes(&value)), ("FreeRADIUS-EAP-FAST-PAC-Opaque-I-ID", bytes(&value))]).unwrap();
        assert!(attributes.iter().all(|(kind, value)| *kind == VENDOR_SPECIFIC && value.len() <= 253));
        assert_eq!(read(&dictionary, attributes), vec![named("FreeRADIUS-EAP-FAST-PAC-Opaque-PAC-Key", bytes(&value)), named("FreeRADIUS-EAP-FAST-PAC-Opaque-I-ID", bytes(&value))]);
        // Too long on its own, for a default vendor and how deep it is.
        let name = "FreeRADIUS-EAP-FAST-PAC-Opaque-I-ID";
        assert!(wire(&dictionary, &[(name, bytes(&"a".repeat(241)))]).is_ok());
        let error = wire(&dictionary, &[(name, bytes(&"a".repeat(242)))]).unwrap_err();
        assert!(error.contains(name) && error.contains("too long"), "{error}");
    }

    #[test]
    fn test_tlv_invalid_is_ignored_whole() {
        let dictionary = format_dictionary();
        let filter = (11u8, b"f".to_vec());
        for (what, value) in [
            ("length 2", vec![1, 6, 0, 0, 0, 1, 2, 2]),
            ("length 1", vec![1, 6, 0, 0, 0, 1, 2, 1]),
            ("length 0", vec![1, 6, 0, 0, 0, 1, 2, 0]),
            ("beyond the attribute", vec![1, 6, 0, 0, 0, 1, 2, 9, 1]),
            ("half a header", vec![1, 6, 0, 0, 0, 1, 2]),
            ("type 0", vec![1, 6, 0, 0, 0, 1, 0, 3, 1]),
            ("bad inside a good one", vec![1, 6, 0, 0, 0, 1, 3, 4, 1, 1]),
        ] {
            assert_eq!(read(&dictionary, vec![(173, value), filter.clone()]), vec![named("Filter-Id", bytes("f"))], "{what}");
        }
        // A good one first: it must not be seen for the bad one after it.
        assert_eq!(read(&dictionary, vec![(26, vec![0, 0, 0, 9, 2, 6, 1, 3, 5, 2])]), vec![], "in a Vendor-Specific");
        assert_eq!(read(&dictionary, vec![(241, vec![5, 1, 6, 0, 0, 0, 2, 1, 2])]), vec![], "in an extended attribute");
    }

    #[test]
    fn test_tlv_unknown_type_is_skipped() {
        let dictionary = format_dictionary();
        assert_eq!(read(&dictionary, vec![(173, vec![99, 3, 7, 1, 6, 0, 0, 0, 1])]), vec![named("Top-One", AttrValue::Number(1.0))]);
    }

    #[test]
    fn test_tlv_child_of_something_else_cannot_be_set() {
        let error = wire(&format_dictionary(), &[("Not-A-Tlv-Child", bytes("x"))]).unwrap_err();
        assert!(error.contains("Not-A-Tlv-Child"), "{error}");
    }

    #[test]
    fn test_vendor_format_two_octet_type() {
        let dictionary = format_dictionary();
        assert_eq!(read(&dictionary, vec![(26, vec![0, 0, 2, 0x7d, 0, 5, 4, b'x'])]), vec![named("Two-Text", bytes("x"))]);
        assert_eq!(wire(&dictionary, &[("Two-High", bytes("x"))]).unwrap(), vec![(26, vec![0, 0, 2, 0x7d, 1, 0x2c, 4, b'x'])]);
        // Two of them in one, and a length that does not fit.
        assert_eq!(read(&dictionary, vec![(26, vec![0, 0, 2, 0x7d, 0, 5, 4, b'x', 1, 0x2c, 4, b'y'])]), vec![named("Two-Text", bytes("x")), named("Two-High", bytes("y"))]);
        assert_eq!(read(&dictionary, vec![(26, vec![0, 0, 2, 0x7d, 0, 5, 2, b'x'])]), vec![]);
    }

    #[test]
    fn test_vendor_format_two_octet_length() {
        let dictionary = format_dictionary();
        let value = vec![0, 0, 0x1f, 0xe4, 0, 5, 0, 5, b'x'];
        assert_eq!(read(&dictionary, vec![(26, value.clone())]), vec![named("Big-Text", bytes("x"))]);
        assert_eq!(wire(&dictionary, &[("Big-Text", bytes("x"))]).unwrap(), vec![(26, value)]);
    }

    #[test]
    fn test_vendor_format_no_length() {
        let dictionary = format_dictionary();
        let value = vec![0, 0, 1, 0xad, 0, 0, 0, 5, b'x', b'y'];
        assert_eq!(read(&dictionary, vec![(26, value.clone())]), vec![named("None-Text", bytes("xy"))]);
        assert_eq!(wire(&dictionary, &[("None-Text", bytes("xy"))]).unwrap(), vec![(26, value)]);
    }

    #[test]
    fn test_vendor_format_continuation() {
        let dictionary = format_dictionary();
        assert_eq!(wire(&dictionary, &[("Cont-Text", bytes("x"))]).unwrap(), vec![(26, vec![0, 0, 0x60, 0xb5, 3, 4, 0, b'x'])]);
        assert_eq!(read(&dictionary, vec![(26, vec![0, 0, 0x60, 0xb5, 3, 4, 0, b'x'])]), vec![named("Cont-Text", bytes("x"))]);
        let value = "b".repeat(300);
        let attributes = wire(&dictionary, &[("Cont-Text", bytes(&value))]).unwrap();
        assert_eq!(attributes.len(), 2);
        assert_eq!((attributes[0].1.len(), attributes[0].1[4], attributes[0].1[5], attributes[0].1[6]), (253, 3, 249, 0x80));
        assert_eq!(attributes[1].1[6], 0, "the last has no flag");
        assert_eq!(read(&dictionary, attributes), vec![named("Cont-Text", bytes(&value))]);
        // TLVs fragment too, and are read back whole.
        let long = "c".repeat(240);
        let attributes = wire(&dictionary, &[("Cont-Tlv-Text", bytes(&long)), ("Cont-Tlv-Text", bytes(&long))]).unwrap();
        assert_eq!(attributes.len(), 2);
        assert_eq!(read(&dictionary, attributes), vec![named("Cont-Tlv-Text", bytes(&long)), named("Cont-Tlv-Text", bytes(&long))]);
    }

    #[test]
    fn test_vendor_format_continuation_invalid() {
        let dictionary = format_dictionary();
        let flagged = vec![0, 0, 0x60, 0xb5, 3, 4, 0x80, b'x'];
        let filter = (11u8, b"f".to_vec());
        for (what, attributes) in [
            ("nothing after", vec![(26u8, flagged.clone()), filter.clone()]),
            ("another vendor", vec![(26, flagged.clone()), (26, vec![0, 0, 0, 9, 1, 3, b'y']), filter.clone()]),
            ("another type", vec![(26, flagged.clone()), (26, vec![0, 0, 0x60, 0xb5, 1, 4, 0, 1]), filter.clone()]),
        ] {
            let names: Vec<String> = read(&dictionary, attributes).into_iter().map(|(name, _)| name).collect();
            assert!(!names.contains(&"Cont-Text".to_string()), "{what}: {names:?}");
            assert!(names.contains(&"Filter-Id".to_string()), "{what}: {names:?}");
            assert_eq!(names.contains(&"Plain-Text".to_string()), what == "another vendor", "{what}: what follows is its own: {names:?}");
        }
    }

    #[test]
    fn test_bundled_dictionary_wimax_tlv() {
        let dictionary = share_test_dictionary();
        let attributes = wire(&dictionary, &[("WiMAX-Release", bytes("1.0")), ("WiMAX-Accounting-Capabilities", AttrValue::Number(1.0))]).unwrap();
        assert_eq!(attributes, vec![(26, vec![0, 0, 0x60, 0xb5, 1, 11, 0, 1, 5, b'1', b'.', b'0', 2, 3, 1])]);
        assert_eq!(read(&dictionary, attributes), vec![named("WiMAX-Release", bytes("1.0")), named("WiMAX-Accounting-Capabilities", AttrValue::Number(1.0))]);
    }

    #[test]
    fn test_plain_attributes_are_as_before() {
        let dictionary = share_test_dictionary();
        let attributes = wire(&dictionary, &[("Filter-Id", bytes("f")), ("Tunnel-Type:1", bytes("VLAN"))]).unwrap();
        assert_eq!(attributes, vec![(11, b"f".to_vec()), (64, vec![1, 0, 0, 13])]);
        assert_eq!(wire(&dictionary, &[("Cisco-AVPair", bytes("a=b"))]).unwrap(), vec![(26, vec![0, 0, 0, 9, 1, 5, b'a', b'=', b'b'])]);
    }

    #[tokio::test]
    async fn test_scripts_read_and_set_extended_attributes() {
        let script = "if get_attr('Frag-Status') ~= 1 then return 'reject' end set_attr('Response-Length', 4096) set_attr('IPv6-6rd-IPv4MaskLen', 8) set_attr('IPv6-6rd-BR-IPv4-Address', '192.0.2.1') return 'accept'";
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap());
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients(std::str::from_utf8(SECRET).unwrap()), engine, share_dictionary());
        let request = RadiusPacket::create_access_request(9, [9; 16], vec![(1, b"a".to_vec()), (241, vec![1, 0, 0, 0, 1])], SECRET);
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attribute(241), Some(&[3, 0, 0, 0x10, 0][..]));
        assert_eq!(response.attribute(173), Some(&[1, 6, 0, 0, 0, 8, 3, 6, 192, 0, 2, 1][..]), "one attribute");
    }

    #[tokio::test]
    async fn test_eap_tls_scripts_can_set_long_values_and_tlvs() {
        let (dir, settings, engine) = eap_rig("set_attr('FreeRADIUS-802.1X-EAPoL-Key-Msg', string.rep('a', 300)) set_attr('IPv6-6rd-IPv4MaskLen', 8) set_attr('IPv6-6rd-BR-IPv4-Address', '192.0.2.1') return 'accept'");
        let server = eap_server(&settings, &engine);
        let (response, _, _) = eap_conversation(&server, dir.path(), b"alice.example.com", &rustls::version::TLS13).await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attributes.iter().filter(|(kind, _)| *kind == 245).count(), 2, "fragments");
        assert_eq!(response.attributes.iter().filter(|(kind, _)| *kind == 173).count(), 1, "together");
    }

    #[tokio::test]
    async fn test_ttls_scripts_can_set_long_values_and_tlvs() {
        let script = "set_attr('FreeRADIUS-802.1X-EAPoL-Key-Msg', string.rep('a', 300)) set_attr('IPv6-6rd-IPv4MaskLen', 8) set_attr('IPv6-6rd-BR-IPv4-Address', '192.0.2.1') return 'accept'";
        let lua = crate::auth_chain::Handler::LuaVerifying(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap());
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![lua]);
        let (dir, server) = ttls_server("ttls-long", "return 'reject'", Some(chain));
        let (response, _, _) = ttls_conversation(&server, ttls_pap_peer(dir.path(), Some("alice"), "x"), b"anonymous").await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attributes.iter().filter(|(kind, _)| *kind == 245).count(), 2, "fragments");
        assert_eq!(response.attributes.iter().filter(|(kind, _)| *kind == 173).count(), 1, "together");
    }

    #[tokio::test]
    async fn test_scripts_can_set_a_long_value() {
        // The check while the script runs has to take what takes two attributes.
        let script = "set_attr('FreeRADIUS-802.1X-EAPoL-Key-Msg', string.rep('a', 300)) return 'accept'";
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap());
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients(std::str::from_utf8(SECRET).unwrap()), engine, share_dictionary());
        let request = RadiusPacket::create_access_request(9, [9; 16], vec![(1, b"a".to_vec())], SECRET);
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attributes.iter().filter(|(kind, _)| *kind == 245).count(), 2);
    }

    /// A dictionary with one attribute of each address type.
    fn address_dictionary() -> Dictionary {
        let mut dictionary = Dictionary::new();
        for (number, kind) in [(201, "ipv6addr"), (202, "ipv6prefix"), (203, "ipv4prefix"), (204, "ifid"), (205, "ether"), (206, "combo-ip")] {
            dictionary.add_attribute(&format!("Test-{kind}"), kind, 0, vec![number], None);
        }
        dictionary
    }

    fn text(value: &str) -> AttrValue {
        AttrValue::Bytes(value.as_bytes().to_vec())
    }

    fn zeros(start: &[u8], len: usize) -> Vec<u8> {
        let mut bytes = start.to_vec();
        bytes.resize(len, 0);
        bytes
    }

    #[test]
    fn test_encode_address_types() {
        let dictionary = share_test_dictionary();
        for (name, value, number, octets) in [
            ("Framed-IPv6-Prefix", "2001:db8::/64", 97, zeros(&[0, 0x40, 0x20, 0x01, 0x0D, 0xB8], 18)),
            ("Delegated-IPv6-Prefix", "2001:db8:1::/48", 123, zeros(&[0, 0x30, 0x20, 0x01, 0x0D, 0xB8, 0, 1], 18)),
            ("Framed-IPv6-Prefix", "::/0", 97, vec![0; 18]),
            ("PMIP6-Home-IPv4-HoA", "192.0.2.0/24", 155, vec![0, 0x18, 0xC0, 0, 2, 0]),
            ("Framed-Interface-Id", "0011:2233:4455:66AA", 96, vec![0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0xAA]),
        ] {
            assert_eq!(encode_attribute(&dictionary, name, &text(value)), Ok((number, octets)), "{name} {value}");
        }
        let dictionary = address_dictionary();
        assert_eq!(encode_attribute(&dictionary, "Test-ether", &text("00:11:22:aa:bb:CC")), Ok((205, vec![0, 0x11, 0x22, 0xAA, 0xBB, 0xCC])));
        assert_eq!(encode_attribute(&dictionary, "Test-combo-ip", &text("192.0.2.1")), Ok((206, vec![0xC0, 0, 2, 1])));
        let six = "2001:db8::1".parse::<Ipv6Addr>().unwrap().octets().to_vec();
        assert_eq!(encode_attribute(&dictionary, "Test-combo-ip", &text("2001:db8::1")), Ok((206, six)));
    }

    #[test]
    fn test_address_values_that_cannot_be_encoded() {
        let dictionary = address_dictionary();
        for (kind, value) in [
            ("ipv6prefix", "2001:db8::1/64"),
            ("ipv6prefix", "2001:db8::/129"),
            ("ipv6prefix", "2001:db8::"),
            ("ipv6prefix", "2001:db8::/+64"),
            ("ipv6prefix", "192.0.2.0/24"),
            ("ipv4prefix", "192.0.2.1/24"),
            ("ipv4prefix", "192.0.2.0/33"),
            ("ipv4prefix", "192.0.2.0"),
            ("ifid", "11:22:33:44"),
            ("ifid", "0011:2233:4455"),
            ("ifid", "0011:2233:4455:+677"),
            ("ifid", "0011:2233:4455:6677:8899"),
            ("ether", "00-11-22-33-44-55"),
            ("ether", "00:11:22:33:44"),
            ("ether", "00:11:22:33:44:+5"),
            ("ether", "0:11:22:33:44:55"),
            ("combo-ip", "example"),
        ] {
            let error = encode_attribute(&dictionary, &format!("Test-{kind}"), &text(value)).expect_err(value);
            assert!(error.contains(&format!("Test-{kind}")), "{value}: {error}");
        }
        for kind in ["ipv6prefix", "ipv4prefix", "ifid", "ether", "combo-ip"] {
            let error = encode_attribute(&dictionary, &format!("Test-{kind}"), &AttrValue::Number(1.0)).expect_err(kind);
            assert!(error.contains(&format!("Test-{kind}")), "{error}");
        }
    }

    fn decoded(kind: &str, value: &[u8]) -> AttrValue {
        let number = match kind {
            "ipv6addr" => 201,
            "ipv6prefix" => 202,
            "ipv4prefix" => 203,
            "ifid" => 204,
            "ether" => 205,
            _ => 206,
        };
        let packet = RadiusPacket { code: ACCESS_REQUEST, identifier: 1, authenticator: [0; 16], attributes: vec![(number, value.to_vec())] };
        request_attributes(&packet, &address_dictionary(), SECRET).remove(0).1
    }

    #[test]
    fn test_decode_address_types() {
        let six = "2001:db8::1".parse::<Ipv6Addr>().unwrap().octets();
        for (kind, octets, expected) in [
            ("ipv6addr", six.to_vec(), "2001:db8::1"),
            ("ipv6prefix", zeros(&[0, 0x40, 0x20, 0x01, 0x0D, 0xB8], 18), "2001:db8::/64"),
            ("ipv6prefix", vec![0, 0x40, 0x20, 0x01, 0x0D, 0xB8], "2001:db8::/64"),
            ("ipv4prefix", vec![0, 0x18, 0xC0, 0, 2, 0], "192.0.2.0/24"),
            ("ifid", vec![0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0xAA], "0011:2233:4455:66aa"),
            ("ether", vec![0, 0x11, 0x22, 0xAA, 0xBB, 0xCC], "00:11:22:aa:bb:cc"),
            ("combo-ip", vec![0xC0, 0, 2, 1], "192.0.2.1"),
            ("combo-ip", six.to_vec(), "2001:db8::1"),
        ] {
            assert_eq!(decoded(kind, &octets), text(expected), "{kind}");
        }
    }

    #[test]
    fn test_received_length_does_not_fit() {
        for (kind, octets) in [
            ("ipv6addr", vec![1, 2, 3]),
            ("ipv6prefix", vec![1]),
            ("ipv6prefix", vec![0; 19]),
            ("ipv6prefix", zeros(&[0, 129], 18)),
            ("ipv4prefix", vec![0, 33, 1, 2, 3, 4]),
            ("ipv4prefix", vec![0, 24, 1, 2, 3]),
            ("ifid", vec![1; 7]),
            ("ether", vec![1; 5]),
            ("combo-ip", vec![1; 5]),
        ] {
            assert_eq!(decoded(kind, &octets), AttrValue::Bytes(octets.clone()), "{kind} {octets:?}");
        }
    }

    #[test]
    fn test_address_types_round_trip() {
        let dictionary = address_dictionary();
        for (kind, value) in [
            ("ipv6addr", "2001:db8::1"),
            ("ipv6prefix", "2001:db8::/64"),
            ("ipv6prefix", "::/0"),
            ("ipv4prefix", "192.0.2.0/24"),
            ("ifid", "0011:2233:4455:66aa"),
            ("ether", "00:11:22:aa:bb:cc"),
            ("combo-ip", "192.0.2.1"),
            ("combo-ip", "2001:db8::1"),
        ] {
            let (_, octets) = encode_attribute(&dictionary, &format!("Test-{kind}"), &text(value)).unwrap();
            assert_eq!(decoded(kind, &octets), text(value), "{kind}");
        }
    }

    #[tokio::test]
    async fn test_script_reads_and_sets_address_types() {
        let script = "if get_attr('NAS-IPv6-Address') ~= '2001:db8::1' then return 'reject' end set_attr('Framed-IPv6-Prefix', '2001:db8::/64') return 'accept'";
        let server = script_server("xyzzy5461", script).await;
        let address = "2001:db8::1".parse::<Ipv6Addr>().unwrap().octets().to_vec();
        let request = RadiusPacket::create_access_request(61, [61; 16], vec![(1, b"v6".to_vec()), (95, address)], SECRET);
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attribute(97), Some(&zeros(&[0, 0x40, 0x20, 0x01, 0x0D, 0xB8], 18)[..]));
    }

    /// The script sees the error, with the attribute's name, and can carry on or let it end
    /// the request.
    #[tokio::test]
    async fn test_script_error_names_the_attribute() {
        let script = "local ok, message = pcall(set_attr, 'Framed-IPv6-Prefix', '2001:db8::1/64') set_attr('Filter-Id', tostring(message)) return ok and 'reject' or 'accept'";
        let server = script_server("xyzzy5461", script).await;
        let request = RadiusPacket::create_access_request(63, [63; 16], vec![(1, b"v6-script".to_vec())], SECRET);
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_ACCEPT, "the call should have raised an error");
        let message = String::from_utf8_lossy(response.attribute(11).unwrap()).into_owned();
        assert!(message.contains("Framed-IPv6-Prefix"), "{message}");
        assert_eq!(response.attribute(97), None);
    }

    /// A value in a reply table that cannot be sent costs the user the login.
    #[tokio::test]
    async fn test_bad_address_in_a_reply_table_rejects() {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE v6_bad_users (username TEXT, password TEXT)").await.unwrap();
        db.execute("INSERT INTO v6_bad_users VALUES ('v6-bad', 'secret')").await.unwrap();
        db.execute("CREATE TABLE v6_bad_reply (username TEXT, attribute TEXT, value TEXT)").await.unwrap();
        db.execute("INSERT INTO v6_bad_reply VALUES ('v6-bad', 'Framed-IPv6-Prefix', '2001:db8::1/64')").await.unwrap();
        let handler = crate::auth_chain::Handler::Sql {
            db,
            user_query: "SELECT password FROM v6_bad_users WHERE username = ?".to_string(),
            reply_query: Some("SELECT attribute, value FROM v6_bad_reply WHERE username = ?".to_string()),
            certificate_query: None, default_user: None,
            timeout: Duration::from_secs(5),
        };
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![handler]);
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").unwrap());
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients("xyzzy5461"), engine, share_dictionary())
            .with_auth_chains(Arc::new(AuthChains::new(Some(chain), Vec::new())));
        let ra = [64u8; 16];
        let request = RadiusPacket::create_access_request(64, ra, vec![(1, b"v6-bad".to_vec()), (USER_PASSWORD, encrypt_password("secret", &ra))], SECRET);
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_reply_attribute_from_sql_has_an_address_type() {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE v6_users (username TEXT, password TEXT)").await.unwrap();
        db.execute("INSERT INTO v6_users VALUES ('v6-alice', 'secret')").await.unwrap();
        db.execute("CREATE TABLE v6_reply (username TEXT, attribute TEXT, value TEXT)").await.unwrap();
        db.execute("INSERT INTO v6_reply VALUES ('v6-alice', 'Framed-IPv6-Prefix', '2001:db8::/64')").await.unwrap();
        let handler = crate::auth_chain::Handler::Sql {
            db,
            user_query: "SELECT password FROM v6_users WHERE username = ?".to_string(),
            reply_query: Some("SELECT attribute, value FROM v6_reply WHERE username = ?".to_string()),
            certificate_query: None, default_user: None,
            timeout: Duration::from_secs(5),
        };
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![handler]);
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'reject'").unwrap());
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients("xyzzy5461"), engine, share_dictionary())
            .with_auth_chains(Arc::new(AuthChains::new(Some(chain), Vec::new())));
        let ra = [62u8; 16];
        let request = RadiusPacket::create_access_request(62, ra, vec![(1, b"v6-alice".to_vec()), (USER_PASSWORD, encrypt_password("secret", &ra))], SECRET);
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_ACCEPT);
        assert_eq!(response.attribute(97), Some(&zeros(&[0, 0x40, 0x20, 0x01, 0x0D, 0xB8], 18)[..]));
    }

    #[test]
    fn test_encode_vendor_attribute() {
        let dictionary = share_dictionary();
        let (attr_type, value) = encode_attribute(&dictionary, "Cisco-AVPair", &AttrValue::Bytes(b"a=b".to_vec())).unwrap();
        assert_eq!(attr_type, VENDOR_SPECIFIC);
        assert_eq!(value, vec![0, 0, 0, 9, 1, 5, b'a', b'=', b'b']);
        assert!(encode_attribute(&dictionary, "Service-Type", &AttrValue::Bytes(b"Not-A-Value".to_vec())).is_err());
        assert!(encode_attribute(&dictionary, "NAS-IP-Address", &AttrValue::Bytes(b"999.1.1.1".to_vec())).is_err());
    }

    #[test]
    fn test_encode_signed_octets_and_integer64() {
        let dictionary = share_dictionary();
        // signed: 3GPP2 (vendor 5535) attribute 143, two's complement.
        let (attr_type, value) = encode_attribute(&dictionary, "3GPP2-GMT-Time-Zone-Offset", &AttrValue::Number(-3600.0)).unwrap();
        assert_eq!(attr_type, VENDOR_SPECIFIC);
        assert_eq!(value, [&5535u32.to_be_bytes()[..], &[143, 6], &(-3600i32).to_be_bytes()[..]].concat());
        assert!(encode_attribute(&dictionary, "3GPP2-GMT-Time-Zone-Offset", &AttrValue::Number(3_000_000_000.0)).is_err());

        // octets: raw bytes pass through unchanged.
        assert_eq!(encode_attribute(&dictionary, "State", &AttrValue::Bytes(vec![0, 0xff, 7])).unwrap(), (24, vec![0, 0xff, 7]));

        // integer64: 8 octets (an Ericsson AB vendor attribute, so wrapped in Vendor-Specific).
        let (attr_type, value) = encode_attribute(&dictionary, "Acct-Input-Octets-64", &AttrValue::Number(4_294_967_296.0)).unwrap();
        assert_eq!(attr_type, VENDOR_SPECIFIC);
        assert_eq!(value[5], 10); // vendor-length: 2 + 8
        assert_eq!(&value[6..], &4_294_967_296u64.to_be_bytes());

        // Decoding a signed value gives back the negative number.
        let attribute = dictionary.get_by_name("3GPP2-GMT-Time-Zone-Offset").unwrap();
        assert_eq!(decode_value(attribute, &(-3600i32).to_be_bytes()), AttrValue::Number(-3600.0));
    }

    #[tokio::test]
    async fn test_unknown_response_attribute_rejects() {
        let (response, _) = rfc_request_response("set_attr('No-Such-Attr', 'x') return 'accept'").await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert_eq!(response.attributes.len(), 1, "only the Message-Authenticator");
    }

    #[tokio::test]
    async fn test_script_reject_gives_access_reject() {
        let (response, bytes) = rfc_request_response("return 'reject'").await;
        assert_eq!(response.code, ACCESS_REJECT);
        assert_eq!(response.identifier, 0);
        let request = RadiusPacket::parse(&hex(RFC_REQUEST)).unwrap();
        let mut for_auth = bytes.clone();
        for_auth[4..20].copy_from_slice(&request.authenticator);
        assert_eq!(&bytes[4..20], &md5_with_secret(&for_auth, SECRET));
    }

    async fn accounting_server(script: &str) -> (RadiusServer, crate::lua_engine::Db) {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE IF NOT EXISTS radius_acct_sessions (id TEXT)").await.unwrap();
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), db.clone(), script).unwrap());
        (RadiusServer::new(any_addr(), any_addr(), local_clients("xyzzy5461"), engine, share_dictionary()), db)
    }

    #[tokio::test]
    async fn test_accounting_script_records_session() {
        let (server, db) = accounting_server("
            if phase == 'accounting' then
                sql_execute(\"INSERT INTO radius_acct_sessions VALUES ('\" .. get_attr('Acct-Session-Id') .. \"')\")
            end
            return 'accept'").await;
        let request = RadiusPacket::create_accounting_request(5, vec![(40, vec![0, 0, 0, 1]), (44, b"session-42".to_vec())], SECRET);
        let response = RadiusPacket::parse(&server.handle_acct(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCOUNTING_RESPONSE);
        let id = db.query_text("SELECT id FROM radius_acct_sessions WHERE id = 'session-42'").await;
        assert_eq!(id, "session-42");
    }

    #[tokio::test]
    async fn test_accounting_not_acknowledged_unless_accepted() {
        let request = RadiusPacket::create_accounting_request(5, vec![(40, vec![0, 0, 0, 1])], SECRET);
        for script in ["error('boom')", "return 'reject'", "return 'ignore'", "local x = 1"] {
            let (server, _db) = accounting_server(script).await;
            assert!(server.handle_acct(&request, LOCALHOST).await.is_none(), "script: {script}");
        }
        let (server, _db) = accounting_server("return 'accept'").await;
        let response = RadiusPacket::parse(&server.handle_acct(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCOUNTING_RESPONSE);
    }

    #[tokio::test]
    async fn test_accounting_not_acknowledged_past_the_execution_limit() {
        let engine = LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "while true do end").unwrap().with_max_execution(Duration::from_millis(50));
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients("xyzzy5461"), Arc::new(engine), share_dictionary());
        let request = RadiusPacket::create_accounting_request(5, vec![(40, vec![0, 0, 0, 1])], SECRET);
        assert!(server.handle_acct(&request, LOCALHOST).await.is_none());
    }

    #[tokio::test]
    async fn test_accounting_retransmission_after_a_failure() {
        // Fails until the test releases it, like a database that comes back.
        let server = counting_server("retry-ack", "if #sql_query(\"SELECT tag FROM dup_release WHERE tag = 'retry-ack'\") == 0 then error('not recorded') end").await;
        let request = RadiusPacket::create_accounting_request(5, vec![(40, vec![0, 0, 0, 1])], SECRET);
        assert!(server.receive(PacketKind::Acct, &request, addr(50000)).await.is_none(), "not recorded, so no acknowledgement");
        release("retry-ack").await;
        let response = server.receive(PacketKind::Acct, &request, addr(50000)).await.expect("the retransmission runs the script again");
        assert_eq!(RadiusPacket::parse(&response).unwrap().code, ACCOUNTING_RESPONSE);
        assert_eq!(runs("retry-ack").await, 2);
        // The answer is cached now, as for any answered request.
        assert_eq!(server.receive(PacketKind::Acct, &request, addr(50000)).await.unwrap(), response);
        assert_eq!(runs("retry-ack").await, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_slow_packet_does_not_block_others() {
        let db = crate::test_support::test_db();
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), db, "
            if get_attr('User-Name') == 'slow' then SLOW_LOOP end
            return 'accept'".replace("SLOW_LOOP", "local t = now_unix() while now_unix() - t < 2 do end")).unwrap()
            .with_max_execution(std::time::Duration::from_secs(10)));
        let server = Arc::new(RadiusServer::new(([127, 0, 0, 1], 18220).into(), ([127, 0, 0, 1], 18230).into(), local_clients("xyzzy5461"), engine, share_dictionary()));
        tokio::spawn(server.run());

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut buf = [0u8; 4096];
        async fn exchange(client: &UdpSocket, id: u8) -> RadiusPacket {
            let fast = RadiusPacket::create_access_request(id, [id; 16], vec![(1, b"fast".to_vec())], SECRET);
            let mut buf = [0u8; 4096];
            loop {
                client.send_to(&fast, "127.0.0.1:18220").await.unwrap();
                if let Ok(Ok((size, _))) = tokio::time::timeout(std::time::Duration::from_millis(100), client.recv_from(&mut buf)).await {
                    return RadiusPacket::parse(&buf[..size]).unwrap();
                }
            }
        }
        // Wait until the server answers, so the slow request is not lost before it binds.
        tokio::time::timeout(std::time::Duration::from_secs(5), exchange(&client, 1)).await.expect("server not answering");

        let slow = RadiusPacket::create_access_request(9, [9u8; 16], vec![(1, b"slow".to_vec())], SECRET);
        client.send_to(&slow, "127.0.0.1:18220").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let reply = tokio::time::timeout(std::time::Duration::from_secs(1), exchange(&client, 2)).await.expect("fast request was blocked");
        assert_eq!(reply.identifier, 2, "only the fast request can be answered while the slow script runs");

        let (size, _) = tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_from(&mut buf)).await.unwrap().unwrap();
        let late = RadiusPacket::parse(&buf[..size]).unwrap();
        assert_eq!((late.identifier, late.code), (9, ACCESS_ACCEPT));
    }

    /// Clients `192.168.1.0/24` ("core", secret s1) and `192.168.1.7` ("nas7", secret s2).
    async fn two_client_server(script: &str) -> RadiusServer {
        let db = crate::test_support::test_db();
        let clients = ClientTable::new(vec![
            ("192.168.1.0/24", Some("core".to_string()), b"s1".to_vec(), None),
            ("192.168.1.7", Some("nas7".to_string()), b"s2".to_vec(), None),
        ])
        .unwrap();
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), db, script).unwrap());
        RadiusServer::new(any_addr(), any_addr(), Arc::new(clients), engine, share_dictionary())
    }

    #[tokio::test]
    async fn test_per_client_secret() {
        let server = two_client_server("return 'accept'").await;
        for (source, secret) in [("192.168.1.7", b"s2"), ("192.168.1.20", b"s1")] {
            let request = RadiusPacket::create_access_request(4, [6u8; 16], vec![(1, b"nemo".to_vec())], secret);
            let bytes = server.handle_auth(&request, source.parse().unwrap()).await.expect("request was discarded");
            let response = RadiusPacket::parse(&bytes).unwrap();
            assert_eq!(response.code, ACCESS_ACCEPT);
            let mut for_auth = bytes.clone();
            for_auth[4..20].copy_from_slice(&[6u8; 16]);
            assert_eq!(&bytes[4..20], &md5_with_secret(&for_auth, secret), "wrong secret used for {}", source);
        }
        // nas7's packet signed with core's secret is discarded.
        let wrong = RadiusPacket::create_access_request(4, [6u8; 16], vec![(1, b"nemo".to_vec())], b"s1");
        assert!(server.handle_auth(&wrong, "192.168.1.7".parse().unwrap()).await.is_none());
    }

    #[tokio::test]
    async fn test_lines_for_unknown_clients_are_limited() {
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients("secret"), Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").unwrap()), share_dictionary());
        let stranger = from("192.0.2.7");
        let log = logged_during(async {
            for identifier in 0..20 {
                assert!(server.receive(PacketKind::Auth, &pap(identifier, "alice", "x"), stranger).await.is_none());
            }
        })
        .await;
        assert_eq!(server.metrics.radius_discarded_unknown_client.get(), 20);
        assert!(log.matches("unknown client 192.0.2.7").count() <= 2, "{log}");
        assert!(log.contains("unknown client 192.0.2.7"), "{log}");
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let log = logged_during(async { assert!(server.receive(PacketKind::Auth, &pap(21, "alice", "x"), stranger).await.is_none()) }).await;
        assert!(log.contains("since the last line"), "what was held back is told: {log}");
    }

    #[test]
    fn test_discard_log_line() {
        let mut log = DiscardLog::default();
        let start = Instant::now();
        assert_eq!(log.line(start, || "one".to_string()).as_deref(), Some("one"), "nothing to add to the first");
        assert_eq!(log.line(start, || unreachable!()), None);
        assert_eq!(log.line(start, || unreachable!()), None);
        assert_eq!(log.line(start + Duration::from_secs(1), || "two".to_string()).as_deref(), Some("two (3 since the last line)"));
        // Five at once and another a second later: the first of them has its line, and the next says five.
        let mut log = DiscardLog::default();
        assert_eq!(log.line(start, || "first".to_string()).as_deref(), Some("first"));
        for _ in 0..4 {
            assert_eq!(log.line(start, || unreachable!()), None);
        }
        assert_eq!(log.line(start + Duration::from_secs(1), || "later".to_string()).as_deref(), Some("later (5 since the last line)"));
    }

    #[tokio::test]
    async fn test_unknown_client_is_discarded() {
        let server = two_client_server("return 'accept'").await;
        let source: IpAddr = "10.0.0.1".parse().unwrap();
        let auth = RadiusPacket::create_access_request(4, [6u8; 16], vec![(1, b"nemo".to_vec())], b"s1");
        assert!(server.handle_auth(&auth, source).await.is_none());
        let acct = RadiusPacket::create_accounting_request(4, vec![(40, vec![0, 0, 0, 1])], b"s1");
        assert!(server.handle_acct(&acct, source).await.is_none());
    }

    #[tokio::test]
    async fn test_script_sees_client_name() {
        let server = two_client_server("return client == 'nas7' and 'accept' or 'reject'").await;
        let request = RadiusPacket::create_access_request(4, [6u8; 16], vec![(1, b"nemo".to_vec())], b"s2");
        let response = RadiusPacket::parse(&server.handle_auth(&request, "192.168.1.7".parse().unwrap()).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_ACCEPT);
        let request = RadiusPacket::create_access_request(4, [6u8; 16], vec![(1, b"nemo".to_vec())], b"s1");
        let response = RadiusPacket::parse(&server.handle_auth(&request, "192.168.1.20".parse().unwrap()).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_top_level_ignore_is_reject() {
        let (response, _) = rfc_request_response("return 'ignore'").await;
        assert_eq!(response.code, ACCESS_REJECT);
    }

    /// User-Password for a single 16-octet block: plaintext XOR MD5(secret + Request Authenticator).
    /// RFC 2865 section 5.2 for a password of at most 16 octets.
    pub(crate) fn encrypt_with(password: &[u8], secret: &[u8], authenticator: &[u8; 16]) -> Vec<u8> {
        let mut block = [0u8; 16];
        block[..password.len()].copy_from_slice(password);
        let key = Md5::digest([secret, authenticator.as_slice()].concat());
        block.iter().zip(key).map(|(byte, key)| byte ^ key).collect()
    }

    fn encrypt_password(password: &str, authenticator: &[u8; 16]) -> Vec<u8> {
        let mut block = [0u8; 16];
        block[..password.len()].copy_from_slice(password.as_bytes());
        let mut hasher = Md5::new();
        hasher.update(SECRET);
        hasher.update(authenticator);
        let pad = hasher.finalize();
        block.iter().zip(pad).map(|(p, b)| p ^ b).collect()
    }

    fn chap_password(id: u8, password: &str, challenge: &[u8]) -> Vec<u8> {
        let mut hasher = Md5::new();
        hasher.update([id]);
        hasher.update(password.as_bytes());
        hasher.update(challenge);
        let mut value = vec![id];
        value.extend_from_slice(&hasher.finalize());
        value
    }

    /// An SQL handler over seeded tables: `alice`/`secret` with two reply
    /// attributes, and `bob` with a `{SHA}` password.
    async fn chain_sql_handler() -> crate::auth_chain::Handler {
        static SEEDED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        let db = crate::test_support::test_db();
        SEEDED
            .get_or_init(|| async {
                let db = crate::test_support::test_db();
                db.execute("CREATE TABLE radius_chain_users (username TEXT, password TEXT)").await.unwrap();
                db.execute("CREATE TABLE radius_chain_reply (username TEXT, attribute TEXT, value TEXT)").await.unwrap();
                db.execute("INSERT INTO radius_chain_users VALUES ('alice', 'secret'), ('bob', '{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g='), ('carol', 'secret'), ('User', 'clientPass'), ('BIGCO\\User', 'clientPass'), ('NTDOM\\User', '{nthash}44EBBA8D5312B8D611474411F56989AE'), ('SHA\\User', '{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g='), ('WRONG\\User', 'other')")
                    .await
                    .unwrap();
                db.execute("INSERT INTO radius_chain_reply VALUES ('alice', 'Filter-Id', 'std.ppp'), ('alice', 'Service-Type', 'Framed-User'), ('carol', 'Tunnel-Password:1', 'hunter2')")
                    .await
                    .unwrap();
            })
            .await;
        crate::auth_chain::Handler::Sql {
            db,
            user_query: "SELECT password FROM radius_chain_users WHERE username = ?".to_string(),
            reply_query: Some("SELECT attribute, value FROM radius_chain_reply WHERE username = ?".to_string()),
            certificate_query: None, default_user: None,
            timeout: std::time::Duration::from_secs(5),
        }
    }

    /// A RADIUS server whose authentication is a single SQL handler over seeded tables.
    async fn chain_server() -> RadiusServer {
        let chain = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![chain_sql_handler().await]);
        // The top-level script rejects everything, so an accept proves the chain decided.
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'reject'").unwrap());
        RadiusServer::new(any_addr(), any_addr(), local_clients("xyzzy5461"), engine, share_dictionary())
            .with_auth_chains(Arc::new(AuthChains::new(Some(chain), Vec::new())))
    }

    /// A PAP Access-Request for `alice` with password `secret`.
    fn alice_request() -> Vec<u8> {
        let ra = [6u8; 16];
        RadiusPacket::create_access_request(10, ra, vec![(1, b"alice".to_vec()), (USER_PASSWORD, encrypt_password("secret", &ra))], SECRET)
    }

    async fn response_code(server: &RadiusServer, source: &str) -> u8 {
        RadiusPacket::parse(&server.handle_auth(&alice_request(), source.parse().unwrap()).await.unwrap()).unwrap().code
    }

    /// `10.0.0.1` has no `auth`; `10.0.0.2` selects the chain `vpn`.
    fn two_clients() -> Arc<ClientTable> {
        Arc::new(
            ClientTable::new(vec![
                ("10.0.0.1", Some("switch".to_string()), SECRET.to_vec(), None),
                ("10.0.0.2", Some("vpn-gw".to_string()), SECRET.to_vec(), Some("vpn".to_string())),
            ])
            .unwrap(),
        )
    }

    fn reject_chain() -> AuthChain {
        let lua = crate::auth_chain::Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'reject'").unwrap());
        AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![lua])
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(LOCALHOST, port)
    }

    fn begin<'a>(cache: &'a DuplicateCache, port: u16, identifier: u8, authenticator: u8) -> DuplicateCheck<'a> {
        cache.begin(PacketKind::Auth, None, addr(port), identifier, [authenticator; 16])
    }

    fn assert_replays(check: DuplicateCheck<'_>, expected: &[u8]) {
        match check {
            DuplicateCheck::Replay(response) => assert_eq!(response, expected),
            _ => panic!("expected a replay"),
        }
    }

    #[test]
    fn test_duplicate_cache_tells_the_receiving_sockets_apart() {
        let cache = DuplicateCache::new(Duration::from_secs(10), 100);
        let (one, two) = (Some(addr(1812)), Some("127.0.0.2:1812".parse().unwrap()));
        let begin = |receiving| cache.begin(PacketKind::Auth, receiving, addr(50000), 9, [1; 16]);
        let DuplicateCheck::Process(first) = begin(one) else { panic!("a new request is processed") };
        assert!(matches!(begin(two), DuplicateCheck::Process(_)), "the same packet to another socket is another request");
        assert!(matches!(begin(one), DuplicateCheck::Discard), "in progress");
        first.finish(b"answer");
        assert_replays(begin(one), b"answer");
    }

    #[tokio::test]
    async fn test_same_packet_to_two_sockets_is_two_requests() {
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").unwrap());
        let server = Arc::new(RadiusServer::new(any_addr(), any_addr(), local_clients("secret"), engine, share_dictionary()));
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let packet = RadiusPacket::create_access_request(1, [1; 16], vec![(1, b"alice".to_vec()), (MESSAGE_AUTHENTICATOR, vec![0; 16])], b"secret");
        for _ in 0..2 {
            let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            client.send_to(&packet, socket.local_addr().unwrap()).await.unwrap();
            tokio::spawn(server.clone().receive_loop(PacketKind::Auth, socket));
            let mut buffer = [0u8; MAX_PACKET_LEN];
            tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut buffer)).await.expect("no answer").unwrap();
        }
        assert_eq!(server.metrics.radius_duplicates.get(), 0, "neither was taken for a retransmission");
    }

    #[tokio::test]
    async fn test_overload_line_is_shared_by_the_sockets_of_a_port() {
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").unwrap());
        let server = Arc::new(RadiusServer::new(any_addr(), any_addr(), local_clients("secret"), engine, share_dictionary()).with_limits(0, 0));
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let log = logged_during(async {
            for _ in 0..2 {
                let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
                client.send_to(b"x", socket.local_addr().unwrap()).await.unwrap();
                tokio::spawn(server.clone().receive_loop(PacketKind::Auth, socket));
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while server.metrics.radius_discarded_overload.get() < 2 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert_eq!(server.metrics.radius_discarded_overload.get(), 2);
        assert_eq!(log.matches("overloaded").count(), 1, "{log}");
    }

    #[tokio::test]
    async fn test_overload_line_is_one_per_port() {
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients("secret"), Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").unwrap()), share_dictionary());
        let now = Instant::now();
        assert_eq!(server.overload_line(PacketKind::Auth, now), Some(1));
        assert_eq!(server.overload_line(PacketKind::Auth, now), None, "whichever socket it came to");
        assert_eq!(server.overload_line(PacketKind::Acct, now), Some(1), "another port");
        assert_eq!(server.overload_line(PacketKind::Auth, now + Duration::from_secs(1)), Some(2));
    }

    #[test]
    fn test_duplicate_cache_replays_and_discards() {
        let cache = DuplicateCache::new(Duration::from_secs(10), 100);
        let DuplicateCheck::Process(ticket) = begin(&cache, 50000, 9, 1) else { panic!("a new request is processed") };
        assert!(matches!(begin(&cache, 50000, 9, 1), DuplicateCheck::Discard), "in progress");
        ticket.finish(b"answer");
        assert_replays(begin(&cache, 50000, 9, 1), b"answer");
    }

    #[test]
    fn test_duplicate_cache_key_parts() {
        let cache = DuplicateCache::new(Duration::from_secs(10), 100);
        let DuplicateCheck::Process(ticket) = begin(&cache, 50000, 9, 1) else { panic!("new") };
        ticket.finish(b"answer");
        assert!(matches!(begin(&cache, 50001, 9, 1), DuplicateCheck::Process(_)), "another source port");
        assert!(matches!(begin(&cache, 50000, 10, 1), DuplicateCheck::Process(_)), "another identifier");
        assert!(matches!(cache.begin(PacketKind::Acct, None, addr(50000), 9, [1; 16]), DuplicateCheck::Process(_)), "the other port");
    }

    #[test]
    fn test_duplicate_cache_new_authenticator_takes_over() {
        let cache = DuplicateCache::new(Duration::from_secs(10), 100);
        let DuplicateCheck::Process(a) = begin(&cache, 50000, 9, 1) else { panic!("new") };
        let DuplicateCheck::Process(b) = begin(&cache, 50000, 9, 2) else { panic!("another authenticator is a new request") };
        b.finish(b"b");
        a.finish(b"a");
        assert_replays(begin(&cache, 50000, 9, 2), b"b");

        // The same when the older request ends without an answer.
        let DuplicateCheck::Process(c) = begin(&cache, 50000, 9, 3) else { panic!("new") };
        let DuplicateCheck::Process(d) = begin(&cache, 50000, 9, 4) else { panic!("new") };
        drop(c);
        assert!(matches!(begin(&cache, 50000, 9, 4), DuplicateCheck::Discard), "d is still in progress");
        d.finish(b"d");
        assert_replays(begin(&cache, 50000, 9, 4), b"d");
    }

    #[test]
    fn test_duplicate_cache_expiry() {
        let cache = DuplicateCache::new(Duration::from_millis(30), 100);
        let DuplicateCheck::Process(ticket) = begin(&cache, 50000, 9, 1) else { panic!("new") };
        ticket.finish(b"answer");
        assert_replays(begin(&cache, 50000, 9, 1), b"answer");
        std::thread::sleep(Duration::from_millis(50));
        assert!(matches!(begin(&cache, 50000, 9, 1), DuplicateCheck::Process(_)));
    }

    #[test]
    fn test_duplicate_cache_expiry_keeps_a_newer_request() {
        let cache = DuplicateCache::new(Duration::from_millis(30), 100);
        let DuplicateCheck::Process(old) = begin(&cache, 50000, 9, 1) else { panic!("new") };
        old.finish(b"old");
        let DuplicateCheck::Process(new) = begin(&cache, 50000, 9, 2) else { panic!("new") };
        std::thread::sleep(Duration::from_millis(50));
        // The old request's expiry passes while the newer one is in progress.
        assert!(matches!(begin(&cache, 50000, 9, 2), DuplicateCheck::Discard));
        new.finish(b"new");
        assert_replays(begin(&cache, 50000, 9, 2), b"new");
    }

    #[test]
    fn test_duplicate_cache_forgets_unanswered_requests() {
        let cache = DuplicateCache::new(Duration::from_secs(10), 100);
        let DuplicateCheck::Process(ticket) = begin(&cache, 50000, 9, 1) else { panic!("new") };
        drop(ticket);
        assert!(matches!(begin(&cache, 50000, 9, 1), DuplicateCheck::Process(_)));
    }

    #[test]
    fn test_duplicate_cache_full() {
        let cache = DuplicateCache::new(Duration::from_secs(10), 1);
        let DuplicateCheck::Process(ticket) = begin(&cache, 50000, 1, 1) else { panic!("new") };
        ticket.finish(b"one");
        assert!(matches!(begin(&cache, 50000, 2, 1), DuplicateCheck::ProcessUncached));
        assert!(matches!(begin(&cache, 50000, 2, 1), DuplicateCheck::ProcessUncached), "an uncached request is processed again");
        // Taking over a key does not need room.
        assert!(matches!(begin(&cache, 50000, 1, 2), DuplicateCheck::Process(_)));
    }

    /// Lua that records a run under `tag`, for `runs` and `wait_for_runs`.
    fn record_run(tag: &str) -> String {
        format!("sql_execute(\"INSERT INTO dup_runs VALUES ('{tag}')\")")
    }

    /// Lua that waits until the test calls `release(tag)`.
    fn wait_for_release(tag: &str) -> String {
        format!("while #sql_query(\"SELECT tag FROM dup_release WHERE tag = '{tag}'\") == 0 do end")
    }

    async fn create_signal_tables() {
        let db = crate::test_support::test_db();
        db.execute("CREATE TABLE IF NOT EXISTS dup_runs (tag TEXT)").await.unwrap();
        db.execute("CREATE TABLE IF NOT EXISTS dup_release (tag TEXT)").await.unwrap();
    }

    /// A server whose script records each run under `tag`, then runs the given
    /// Lua and accepts.
    async fn counting_server(tag: &str, then: &str) -> RadiusServer {
        create_signal_tables().await;
        let script = format!("{} {then} return 'accept'", record_run(tag));
        let engine = LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap().with_max_execution(Duration::from_secs(30));
        RadiusServer::new(any_addr(), any_addr(), local_clients("xyzzy5461"), Arc::new(engine), share_dictionary())
    }

    async fn runs(tag: &str) -> usize {
        crate::test_support::test_db().query_with_params("SELECT tag FROM dup_runs WHERE tag = ?", &[tag]).await.unwrap().len()
    }

    async fn wait_for_runs(tag: &str, count: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while runs(tag).await < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the script did not start");
    }

    async fn release(tag: &str) {
        crate::test_support::test_db().execute(&format!("INSERT INTO dup_release VALUES ('{tag}')")).await.unwrap();
    }

    fn nemo_request(identifier: u8, authenticator: u8) -> Vec<u8> {
        RadiusPacket::create_access_request(identifier, [authenticator; 16], vec![(1, b"nemo".to_vec())], SECRET)
    }

    #[tokio::test]
    async fn test_retransmission_gets_the_same_response() {
        let server = counting_server("answered", "").await;
        let request = nemo_request(9, 1);
        let first = server.receive(PacketKind::Auth, &request, addr(50000)).await.unwrap();
        let second = server.receive(PacketKind::Auth, &request, addr(50000)).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(runs("answered").await, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_retransmission_while_processing_is_discarded() {
        let server = Arc::new(counting_server("in-progress", &wait_for_release("in-progress")).await);
        let request = nemo_request(9, 1);
        let (running, copy) = (server.clone(), request.clone());
        let first = tokio::spawn(async move { running.receive(PacketKind::Auth, &copy, addr(50000)).await });
        wait_for_runs("in-progress", 1).await;
        assert!(server.receive(PacketKind::Auth, &request, addr(50000)).await.is_none());
        release("in-progress").await;
        assert!(first.await.unwrap().is_some());
        assert_eq!(runs("in-progress").await, 1);
    }

    #[tokio::test]
    async fn test_accounting_retransmission() {
        let server = counting_server("accounting", "").await;
        let request = RadiusPacket::create_accounting_request(3, vec![(40, vec![0, 0, 0, 1])], SECRET);
        let first = server.receive(PacketKind::Acct, &request, addr(50000)).await.unwrap();
        let second = server.receive(PacketKind::Acct, &request, addr(50000)).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(RadiusPacket::parse(&first).unwrap().code, ACCOUNTING_RESPONSE);
        assert_eq!(runs("accounting").await, 1);
    }

    #[tokio::test]
    async fn test_each_source_port_is_processed() {
        let server = counting_server("ports", "").await;
        let request = nemo_request(9, 1);
        server.receive(PacketKind::Auth, &request, addr(50000)).await.unwrap();
        server.receive(PacketKind::Auth, &request, addr(50001)).await.unwrap();
        assert_eq!(runs("ports").await, 2);
    }

    #[tokio::test]
    async fn test_identifier_reused_for_a_new_request() {
        let server = counting_server("reused", "").await;
        let (old, new) = (nemo_request(9, 1), nemo_request(9, 2));
        let old_response = server.receive(PacketKind::Auth, &old, addr(50000)).await.unwrap();
        let new_response = server.receive(PacketKind::Auth, &new, addr(50000)).await.unwrap();
        assert_ne!(old_response, new_response);
        assert_eq!(server.receive(PacketKind::Auth, &new, addr(50000)).await.unwrap(), new_response);
        assert_eq!(runs("reused").await, 2);
    }

    #[tokio::test]
    async fn test_invalid_packet_does_not_touch_the_cache() {
        let server = counting_server("invalid", "").await;
        let request = nemo_request(9, 1);
        let answer = server.receive(PacketKind::Auth, &request, addr(50000)).await.unwrap();
        let forged = RadiusPacket::create_access_request(9, [2u8; 16], vec![(1, b"nemo".to_vec())], b"other-secret");
        assert!(server.receive(PacketKind::Auth, &forged, addr(50000)).await.is_none());
        assert_eq!(server.receive(PacketKind::Auth, &request, addr(50000)).await.unwrap(), answer);
        assert_eq!(runs("invalid").await, 1);
    }

    #[tokio::test]
    async fn test_abandoned_request_is_processed_again() {
        let server = counting_server("abandoned", "while true do end").await;
        let request = nemo_request(9, 1);
        // Dropped in the middle of the script, as when its task is cancelled; the
        // retransmission only reaches the second count if it runs rather than being discarded.
        for count in [1, 2] {
            tokio::select! {
                _ = server.receive(PacketKind::Auth, &request, addr(50000)) => panic!("the script never ends"),
                _ = wait_for_runs("abandoned", count) => {}
            }
        }
    }

    #[tokio::test]
    async fn test_cache_full_still_answers() {
        let server = counting_server("cache-full", "").await.with_duplicate_cache(DUPLICATE_RETENTION, 1);
        server.receive(PacketKind::Auth, &nemo_request(1, 1), addr(50000)).await.unwrap();
        // The one entry is taken, so this request is answered but not remembered.
        let request = nemo_request(2, 2);
        let first = server.receive(PacketKind::Auth, &request, addr(50000)).await.unwrap();
        assert_eq!(RadiusPacket::parse(&first).unwrap().code, ACCESS_ACCEPT);
        server.receive(PacketKind::Auth, &request, addr(50000)).await.unwrap();
        assert_eq!(runs("cache-full").await, 3, "the retransmission runs the script again");
    }

    #[test]
    fn test_duplicate_retention_is_within_rfc_5080() {
        assert_eq!(DUPLICATE_RETENTION, Duration::from_secs(10));
    }

    #[test]
    fn test_discard_log_is_rate_limited() {
        let mut log = DiscardLog::default();
        let start = Instant::now();
        assert_eq!(log.record(start), Some(1), "the first discard is logged at once");
        for i in 0..100 {
            assert_eq!(log.record(start + Duration::from_millis(i * 5)), None);
        }
        assert_eq!(log.record(start + Duration::from_secs(1)), Some(101), "the next line counts what was held back");
        assert_eq!(log.record(start + Duration::from_millis(1100)), None);
    }

    /// Running on free ports with the given limits. Requests for user `slow`, and
    /// accounting requests when `slow_acct`, record a run and wait for `release(tag)`.
    async fn limited_server(tag: &str, max_auth: usize, max_acct: usize, slow_acct: bool) -> (SocketAddr, SocketAddr) {
        create_signal_tables().await;
        let port = || std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let (auth, acct) = (port(), port());
        let script = format!(
            "if get_attr('User-Name') == 'slow' or (phase == 'accounting' and {slow_acct}) then {} {} end return 'accept'",
            record_run(tag),
            wait_for_release(tag)
        );
        let engine = LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), script).unwrap().with_max_execution(Duration::from_secs(30));
        let server = RadiusServer::new(auth, acct, local_clients("xyzzy5461"), Arc::new(engine), share_dictionary()).with_limits(max_auth, max_acct);
        tokio::spawn(Arc::new(server).run());
        (auth, acct)
    }

    /// Sends `packet` until its script has started: the server may not be listening
    /// yet, and copies that arrive once it runs are discarded as retransmissions.
    async fn start_slow_request(socket: &UdpSocket, target: SocketAddr, packet: &[u8], tag: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while runs(tag).await == 0 {
                socket.send_to(packet, target).await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the slow request did not start");
    }

    /// Sends `packet` and waits up to `wait` for a reply.
    async fn exchange_with(socket: &UdpSocket, target: SocketAddr, packet: &[u8], wait: Duration) -> Option<RadiusPacket> {
        socket.send_to(packet, target).await.unwrap();
        let mut buf = [0u8; 4096];
        match tokio::time::timeout(wait, socket.recv_from(&mut buf)).await {
            Ok(Ok((size, _))) => Some(RadiusPacket::parse(&buf[..size]).unwrap()),
            _ => None,
        }
    }

    /// Retransmits `packet` like a NAS until a reply arrives.
    async fn exchange_until_answered(socket: &UdpSocket, target: SocketAddr, packet: &[u8]) -> RadiusPacket {
        for _ in 0..50 {
            if let Some(reply) = exchange_with(socket, target, packet, Duration::from_millis(100)).await {
                return reply;
            }
        }
        panic!("no reply after 5 s of retransmissions");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_authentication_limit() {
        let (auth, _) = limited_server("auth-limit", 1, 10, false).await;
        let (slow_client, client) = (UdpSocket::bind("127.0.0.1:0").await.unwrap(), UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let slow = RadiusPacket::create_access_request(1, [1u8; 16], vec![(1, b"slow".to_vec())], SECRET);
        start_slow_request(&slow_client, auth, &slow, "auth-limit").await;

        let request = nemo_request(2, 2);
        assert!(exchange_with(&client, auth, &request, Duration::from_millis(200)).await.is_none(), "no reply, not even a reject, at the limit");

        release("auth-limit").await;
        let mut buf = [0u8; 4096];
        let (size, _) = tokio::time::timeout(Duration::from_secs(5), slow_client.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!(RadiusPacket::parse(&buf[..size]).unwrap().code, ACCESS_ACCEPT);

        // The discarded packet left no trace, so its retransmission is served.
        assert_eq!(exchange_until_answered(&client, auth, &request).await.code, ACCESS_ACCEPT);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_no_record_for_a_packet_discarded_by_overload() {
        let (auth, _acct) = limited_server("rec-overload", 1, 10, false).await;
        let (holder, client) = (UdpSocket::bind("127.0.0.1:0").await.unwrap(), UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let slow = RadiusPacket::create_access_request(1, [71; 16], vec![(1, b"slow".to_vec())], SECRET);
        start_slow_request(&holder, auth, &slow, "rec-overload").await;
        let discarded = RadiusPacket::create_access_request(2, [72; 16], vec![(1, b"rec-discarded".to_vec())], SECRET);
        assert!(exchange_with(&client, auth, &discarded, Duration::from_millis(200)).await.is_none(), "authentication is at its limit");
        assert!(crate::logging::records_for("rec-discarded").is_empty());
        release("rec-overload").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_accounting_limit_leaves_authentication_its_own() {
        let (auth, acct) = limited_server("acct-limit", 10, 1, true).await;
        let (acct_client, client) = (UdpSocket::bind("127.0.0.1:0").await.unwrap(), UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let start = |id: u8| RadiusPacket::create_accounting_request(id, vec![(40, vec![0, 0, 0, 1])], SECRET);
        start_slow_request(&acct_client, acct, &start(1), "acct-limit").await;

        assert!(exchange_with(&client, acct, &start(2), Duration::from_millis(200)).await.is_none(), "accounting is at its limit");
        let reply = exchange_until_answered(&client, auth, &nemo_request(3, 3)).await;
        assert_eq!(reply.code, ACCESS_ACCEPT);
        release("acct-limit").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_accounting_requests_share_the_script_limit() {
        let vm = crate::lua_engine::tests::GatedVm::new();
        let holder = vm.hold_place().await;
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients("xyzzy5461"), vm.engine("return 'accept'"), share_dictionary());
        let request = RadiusPacket::create_accounting_request(3, vec![(40, vec![0, 0, 0, 1])], SECRET);
        let reply = tokio::spawn(async move { server.handle_acct(&request, LOCALHOST).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!reply.is_finished(), "RADIUS accounting ran past the limit");
        vm.open_gate();
        assert!(reply.await.unwrap().is_some());
        holder.await.unwrap();
    }

    #[tokio::test]
    async fn test_chain_per_client() {
        // The default chain's SQL handler accepts alice/secret, `vpn` rejects, and so does the script.
        let default = AuthChain::new(crate::config::AuthPolicy::FirstMatch, vec![chain_sql_handler().await]);
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'reject'").unwrap());
        let server = RadiusServer::new(any_addr(), any_addr(), two_clients(), engine, share_dictionary())
            .with_auth_chains(Arc::new(AuthChains::new(Some(default), [("vpn".to_string(), reject_chain())])));
        assert_eq!(response_code(&server, "10.0.0.1").await, ACCESS_ACCEPT);
        assert_eq!(response_code(&server, "10.0.0.2").await, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_named_chains_only() {
        // Without a default chain, the client without `auth` is decided by the top-level script.
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'accept'").unwrap());
        let server = RadiusServer::new(any_addr(), any_addr(), two_clients(), engine, share_dictionary())
            .with_auth_chains(Arc::new(AuthChains::new(None, [("vpn".to_string(), reject_chain())])));
        assert_eq!(response_code(&server, "10.0.0.1").await, ACCESS_ACCEPT);
        assert_eq!(response_code(&server, "10.0.0.2").await, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_shared_chain_between_protocols() {
        use crate::config::{AuthHandlerSettings, ChainSettings};
        use crate::ldap_handler::tests::{mock_server, settings};
        use crate::ldap_handler::PasswordCheck;
        use crate::tacacs::tests::{authen_status, script_engine, test_client};
        let (url, log) = mock_server().await;
        let vpn = ChainSettings {
            policy: crate::config::AuthPolicy::FirstMatch,
            handlers: vec![AuthHandlerSettings::Ldap(settings(vec![url], PasswordCheck::Bind))],
            username: None,
        };
        let vpn = AuthChain::from_settings(&vpn, &crate::test_support::test_db(), &crate::test_support::test_vms(), &crate::auth_chain::LuaLimits::default()).unwrap();
        // One set of chains handed to both protocols, as main.rs does.
        let chains = Arc::new(AuthChains::new(None, [("vpn".to_string(), vpn)]));

        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), crate::test_support::test_db(), "return 'reject'").unwrap());
        let radius = RadiusServer::new(any_addr(), any_addr(), two_clients(), engine, share_dictionary()).with_auth_chains(chains.clone());
        assert_eq!(response_code(&radius, "10.0.0.2").await, ACCESS_ACCEPT);

        let router = Client { auth: Some("vpn".to_string()), ..test_client() };
        let status = authen_status(router, chains, script_engine("return 'reject'").await, b"alice", b"secret").await;
        assert_eq!(status, crate::tacacs::AUTHEN_STATUS_PASS);
        // One LDAP handler for `vpn`: the TACACS+ request reused the RADIUS request's connection.
        assert_eq!(log.lock().unwrap().connections, 1);
    }

    async fn chain_response(attributes: Vec<(u8, Vec<u8>)>, authenticator: [u8; 16]) -> RadiusPacket {
        let server = chain_server().await;
        let request = RadiusPacket::create_access_request(9, authenticator, attributes, SECRET);
        RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn test_sql_reply_encrypts_tunnel_password() {
        let ra = [4u8; 16];
        let response = chain_response(vec![(1, b"carol".to_vec()), (USER_PASSWORD, encrypt_password("secret", &ra))], ra).await;
        assert_eq!(response.code, ACCESS_ACCEPT);
        let (tag, _, password) = decrypt_tunnel_password(response.attribute(69).unwrap(), SECRET, &ra);
        assert_eq!((tag, password.as_slice()), (1, &b"hunter2"[..]));
    }

    #[tokio::test]
    async fn test_chain_pap() {
        let ra = [4u8; 16];
        let ok = chain_response(vec![(1, b"alice".to_vec()), (USER_PASSWORD, encrypt_password("secret", &ra))], ra).await;
        assert_eq!(ok.code, ACCESS_ACCEPT);
        assert_eq!(ok.attribute(11), Some(&b"std.ppp"[..]));
        assert_eq!(ok.attribute(6), Some(&[0, 0, 0, 2][..]));
        let bad = chain_response(vec![(1, b"alice".to_vec()), (USER_PASSWORD, encrypt_password("wrong", &ra))], ra).await;
        assert_eq!(bad.code, ACCESS_REJECT);
    }

    #[tokio::test]
    async fn test_chain_chap() {
        let ra = [5u8; 16];
        let challenge = [0x33u8; 16];
        let with_challenge = chain_response(
            vec![(1, b"alice".to_vec()), (CHAP_PASSWORD, chap_password(7, "secret", &challenge)), (CHAP_CHALLENGE, challenge.to_vec())],
            ra,
        )
        .await;
        assert_eq!(with_challenge.code, ACCESS_ACCEPT);
        let authenticator_challenge = chain_response(vec![(1, b"alice".to_vec()), (CHAP_PASSWORD, chap_password(7, "secret", &ra))], ra).await;
        assert_eq!(authenticator_challenge.code, ACCESS_ACCEPT, "the Request Authenticator is the default challenge");
        let hashed = chain_response(vec![(1, b"bob".to_vec()), (CHAP_PASSWORD, chap_password(7, "password", &ra))], ra).await;
        assert_eq!(hashed.code, ACCESS_REJECT, "CHAP cannot be checked against a hashed password");
    }

    #[tokio::test]
    async fn test_chain_reply_attributes_order_in_packet() {
        let db = chain_server().await.state().engine.db.clone(); // seeds radius_chain_users / radius_chain_reply
        let handlers = vec![
            crate::auth_chain::Handler::Sql {
                db: db.clone(),
                user_query: "SELECT password FROM radius_chain_users WHERE username = ?".to_string(),
                reply_query: Some("SELECT attribute, value FROM radius_chain_reply WHERE username = ? AND attribute = 'Filter-Id'".to_string()),
                certificate_query: None, default_user: None,
                timeout: std::time::Duration::from_secs(5),
            },
            crate::auth_chain::Handler::Lua(LuaAuthEngine::new(crate::test_support::test_vms(), db.clone(), "set_attr('Session-Timeout', 3600) return 'accept'").unwrap()),
        ];
        let chain = AuthChain::new(crate::config::AuthPolicy::AllAccept, handlers);
        let engine = Arc::new(LuaAuthEngine::new(crate::test_support::test_vms(), db, "return 'reject'").unwrap());
        let server = RadiusServer::new(any_addr(), any_addr(), local_clients("xyzzy5461"), engine, share_dictionary())
            .with_auth_chains(Arc::new(AuthChains::new(Some(chain), Vec::new())));
        let ra = [8u8; 16];
        let request = RadiusPacket::create_access_request(9, ra, vec![(1, b"alice".to_vec()), (USER_PASSWORD, encrypt_password("secret", &ra))], SECRET);
        let response = RadiusPacket::parse(&server.handle_auth(&request, LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_ACCEPT);
        let order: Vec<u8> = response.attributes.iter().map(|(t, _)| *t).filter(|t| *t == 11 || *t == 27).collect();
        assert_eq!(order, vec![11, 27], "Filter-Id must come before Session-Timeout");
        assert_eq!(response.attribute(27), Some(&3600u32.to_be_bytes()[..]));
    }

    /// A forwarder that always says the same, made again for each request.
    struct SaysAlways(fn() -> crate::forwarding::Route);

    impl crate::forwarding::Forwarder for SaysAlways {
        fn route<'a>(&'a self, _request: &'a crate::forwarding::ForwardRequest<'a>) -> BoxFuture<'a, crate::forwarding::Route> {
            let route = (self.0)();
            Box::pin(async move { route })
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// A server whose requests go first to a forwarder that always says `route`.
    async fn forwarding_server(route: fn() -> crate::forwarding::Route) -> RadiusServer {
        let server = script_server("xyzzy5461", "set_attr('Reply-Message', 'here') return 'accept'").await;
        let state = server.state();
        server.replace_state(RadiusState::new(state.clients.clone(), state.engine.clone(), None).with_forwarder(Some(Arc::new(SaysAlways(route)))));
        server
    }

    /// Each answer of a forwarder: run here, dropped with a line in the log, another server's
    /// answer with its record, an answer made without one, or nothing.
    #[tokio::test]
    async fn test_what_a_forwarder_says() -> Result<(), String> {
        use crate::forwarding::Route;
        let local = forwarding_server(|| Route::Local).await;
        let answer = local.receive(PacketKind::Auth, &pap(1, "fwd-local", "pw"), from("127.0.0.1")).await.ok_or("no answer")?;
        let answer = RadiusPacket::parse(&answer).map_err(|e| e.to_string())?;
        assert_eq!((answer.code, answer.attribute(18)), (ACCESS_ACCEPT, Some(&b"here"[..])), "the script ran");

        let discarding = forwarding_server(|| Route::Discard("its realm cannot be told now".to_string())).await;
        let log = logged_during(async {
            assert!(discarding.receive(PacketKind::Auth, &pap(2, "fwd-discard", "pw"), from("127.0.0.1")).await.is_none());
        })
        .await;
        assert!(log.contains("RADIUS request from client local discarded: its realm cannot be told now"), "{log}");

        let answered = forwarding_server(|| Route::Answered { response: vec![ACCESS_REJECT, 9, 0, 20], account: "fwd-account".to_string(), realm: "hq.test".to_string() }).await;
        assert_eq!(answered.receive(PacketKind::Auth, &pap(3, "fwd-answered@hq.test", "pw"), from("127.0.0.1")).await, Some(vec![ACCESS_REJECT, 9, 0, 20]));
        let records = crate::logging::records_for("fwd-answered@hq.test");
        assert_eq!(records.iter().map(|record| (record.account.as_str(), record.chain.as_str(), record.handler.as_str(), record.decision.as_str())).collect::<Vec<_>>(), vec![("fwd-account", "proxy", "hq.test", "reject")]);

        let replied = forwarding_server(|| Route::Replied(vec![ACCOUNTING_RESPONSE, 4, 0, 20])).await;
        assert_eq!(replied.receive(PacketKind::Auth, &pap(4, "fwd-replied", "pw"), from("127.0.0.1")).await, Some(vec![ACCOUNTING_RESPONSE, 4, 0, 20]));
        assert!(crate::logging::records_for("fwd-replied").is_empty(), "no decision to record");

        let silent = forwarding_server(|| Route::Unanswered).await;
        assert!(silent.receive(PacketKind::Auth, &pap(5, "fwd-silent", "pw"), from("127.0.0.1")).await.is_none());
        Ok(())
    }

    /// An EAP method of an extension, for the tests of the hook: its first Response asks for a
    /// lookup of what follows a 1, its second proves the peer is `fake-user` with a 2.
    struct FakeMethod {
        /// Lookups that find the database unavailable before one answers.
        unavailable: std::sync::atomic::AtomicUsize,
        lookups: std::sync::atomic::AtomicUsize,
        /// What `before_authorize` says: 0 goes on, 1 the database is unavailable, 2 refused.
        before: std::sync::atomic::AtomicU8,
    }

    const FAKE_TYPE: u8 = 254;

    impl FakeMethod {
        fn new() -> Self {
            Self { unavailable: 0.into(), lookups: 0.into(), before: 0.into() }
        }
    }

    struct FakeConversation {
        started: bool,
        refused: bool,
        account: String,
        proved: bool,
    }

    impl crate::eap_method::EapMethod for FakeMethod {
        fn names(&self) -> crate::eap_method::EapMethodName {
            crate::eap_method::EapMethodName { kind: FAKE_TYPE, name: "fake-eap", handler: "eap-fake", title: "FAKE-EAP" }
        }
        fn start(&self) -> Box<dyn crate::eap_method::MethodConversation> {
            Box::new(FakeConversation { started: false, refused: false, account: String::new(), proved: false })
        }
        fn lookup<'a>(&'a self, _context: &'a crate::eap_method::MethodContext<'a>, request: &'a crate::eap_method::MethodData) -> futures::future::BoxFuture<'a, Result<crate::eap_method::MethodData, crate::eap_method::LookupError>> {
            use std::sync::atomic::Ordering::SeqCst;
            Box::pin(async move {
                self.lookups.fetch_add(1, SeqCst);
                if self.unavailable.try_update(SeqCst, SeqCst, |left| left.checked_sub(1)).is_ok() {
                    return Err(crate::eap_method::LookupError::Unavailable("the fake table timed out".to_string()));
                }
                let asked = request.downcast_ref::<Vec<u8>>().ok_or("not what was asked")?;
                match asked.first() {
                    Some(0) => Err(crate::eap_method::LookupError::OverBudget("too many fakes")),
                    Some(_) => Ok(Box::new(asked.clone()) as crate::eap_method::MethodData),
                    None => Err("nothing to look up".into()),
                }
            })
        }
        fn before_authorize<'a>(&'a self, _context: &'a crate::eap_method::MethodContext<'a>, name: &'a str, proof: &'a crate::eap_method::MethodData) -> futures::future::BoxFuture<'a, Result<(), crate::eap_method::NotReady>> {
            Box::pin(async move {
                assert_eq!((name, proof.downcast_ref::<u32>()), ("fake-user", Some(&7)));
                match self.before.load(std::sync::atomic::Ordering::SeqCst) {
                    0 => Ok(()),
                    waits => Err(crate::eap_method::NotReady { log: "the fake proof could not be stored".to_string(), refusal: "the fake proof was not stored", unavailable: waits == 1 }),
                }
            })
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    impl crate::eap_method::MethodConversation for FakeConversation {
        fn first_message(&mut self) -> Vec<u8> {
            vec![1]
        }
        fn started(&self) -> bool {
            self.started
        }
        fn refused(&self) -> bool {
            self.refused
        }
        fn account(&self) -> String {
            self.account.clone()
        }
        fn chain_refused(&mut self, account: String) -> String {
            self.refused = true;
            self.account = account;
            "the fake peer was refused".to_string()
        }
        fn receive(&mut self, data: &[u8], _packet: &[u8]) -> crate::eap_method::MethodStep {
            self.started = true;
            match data.split_first() {
                Some((1, rest)) => crate::eap_method::MethodStep::Lookup(Box::new(rest.to_vec())),
                Some((2, _)) => {
                    self.proved = true;
                    self.account = "fake-user".to_string();
                    crate::eap_method::MethodStep::Authorize { name: "fake-user".to_string(), proof: Some(Box::new(7u32)) }
                }
                _ => crate::eap_method::MethodStep::Failed("a fake message out of turn".to_string()),
            }
        }
        fn answer(&mut self, sent: u8, found: crate::eap_method::MethodData) -> Result<Vec<u8>, String> {
            let found = found.downcast::<Vec<u8>>().map_err(|_| "not what was found".to_string())?;
            Ok([&[sent], found.as_slice()].concat())
        }
        fn msk(&self) -> Option<zeroize::Zeroizing<[u8; 64]>> {
            self.proved.then(|| zeroize::Zeroizing::new([7; 64]))
        }
    }

    fn fake_request(identifier: u8, user: &str, eap: &[u8], state: Option<&[u8]>) -> Vec<u8> {
        let mut attributes = vec![(1, user.as_bytes().to_vec())];
        attributes.extend(split_message(eap));
        attributes.extend(state.map(|state| (STATE, state.to_vec())));
        RadiusPacket::create_access_request(identifier, [identifier; 16], attributes, SECRET)
    }

    fn fake_response(id: u8, data: &[u8]) -> Vec<u8> {
        EapPacket { code: 2, id, body: [&[FAKE_TYPE], data].concat() }.encode()
    }

    /// A server offering only the fake method, and the method.
    fn fake_server(script: &str) -> (tempfile::TempDir, RadiusServer, Arc<FakeMethod>) {
        let (dir, settings, engine) = eap_rig(script);
        let method = Arc::new(FakeMethod::new());
        let mut settings = Arc::try_unwrap(settings).unwrap();
        settings.methods = vec![FAKE_TYPE];
        settings.extra = vec![method.clone()];
        let server = eap_server(&Arc::new(settings), &engine);
        (dir, server, method)
    }

    /// Up to the fake method's first Request: its EAP Identifier and the State.
    async fn fake_opened(server: &RadiusServer, user: &str) -> (u8, Vec<u8>) {
        let response = RadiusPacket::parse(&server.handle_auth(&fake_request(1, user, &crate::eap::tests::identity_response(1, user.as_bytes()), None), LOCALHOST).await.unwrap()).unwrap();
        assert_eq!(response.code, ACCESS_CHALLENGE);
        let eap = EapPacket::parse(&joined(&response)).unwrap();
        assert_eq!(eap.body, vec![FAKE_TYPE, 1], "the method's first message");
        (eap.id, response.attribute(STATE).unwrap().to_vec())
    }

    #[tokio::test]
    async fn test_eap_method_of_an_extension() -> Result<(), String> {
        let script = "if eap_method == 'fake-eap' and get_attr('User-Name') == 'fake-user' then set_attr('Filter-Id', 'faked') return 'accept' end return 'reject'";
        let (_dir, server, method) = fake_server(script);
        let user = "fake-hook-accept";
        let (id, state) = fake_opened(&server, user).await;

        // The lookup finds the database unavailable: no answer, counted, and logged.
        method.unavailable.store(1, std::sync::atomic::Ordering::SeqCst);
        let discarded = server.metrics.radius_discarded_database.get();
        let lookup = fake_request(2, user, &fake_response(id, &[1, 9, 8]), Some(&state));
        let log = logged_during(async { assert!(server.handle_auth(&lookup, LOCALHOST).await.is_none()) }).await;
        assert_eq!(server.metrics.radius_discarded_database.get(), discarded + 1);
        assert!(log.contains("not answered: the fake table timed out"), "{log}");
        // The same Response again does the lookup again, and the conversation goes on.
        let response = RadiusPacket::parse(&server.handle_auth(&fake_request(3, user, &fake_response(id, &[1, 9, 8]), Some(&state)), LOCALHOST).await.ok_or("no answer to the repeat")?).map_err(|e| format!("{e:?}"))?;
        assert_eq!(method.lookups.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(response.code, ACCESS_CHALLENGE);
        let eap = EapPacket::parse(&joined(&response)).ok_or("no EAP")?;
        assert_eq!((eap.id, eap.body), (id.wrapping_add(1), vec![FAKE_TYPE, id.wrapping_add(1), 9, 8]), "the answer from what was found");

        // Proved: `before_authorize` cannot use the database, and the same Response comes again.
        method.before.store(1, std::sync::atomic::Ordering::SeqCst);
        let proof = fake_request(4, user, &fake_response(eap.id, &[2]), Some(&state));
        let log = logged_during(async { assert!(server.handle_auth(&proof, LOCALHOST).await.is_none()) }).await;
        assert!(log.contains("FAKE-EAP for client"), "{log}");
        assert!(log.contains("the fake proof could not be stored"), "{log}");
        method.before.store(0, std::sync::atomic::Ordering::SeqCst);
        let accept = RadiusPacket::parse(&server.handle_auth(&fake_request(5, user, &fake_response(eap.id, &[2]), Some(&state)), LOCALHOST).await.ok_or("no answer to the proof")?).map_err(|e| format!("{e:?}"))?;
        assert_eq!(accept.code, ACCESS_ACCEPT);
        assert!(accept.attributes.iter().any(|(kind, value)| *kind == 11 && value == b"faked"), "the chain's reply");
        assert_eq!(accept.attributes.iter().filter(|(kind, value)| *kind == 26 && value.get(..4) == Some(&311u32.to_be_bytes()[..])).count(), 2, "the keys");
        let records = crate::logging::records_for(user);
        assert_eq!((records.len(), records[0].handler.as_str(), records[0].account.as_str(), records[0].decision.as_str()), (1, "eap-fake", "fake-user", "accept"));
        Ok(())
    }

    #[tokio::test]
    async fn test_eap_method_of_an_extension_refused() -> Result<(), String> {
        let (_dir, server, method) = fake_server("return 'reject'");
        // `before_authorize` refuses: rejected without the chain.
        let user = "fake-hook-not-stored";
        let (id, state) = fake_opened(&server, user).await;
        method.before.store(2, std::sync::atomic::Ordering::SeqCst);
        let log = logged_during(async {
            let reject = server.handle_auth(&fake_request(2, user, &fake_response(id, &[2]), Some(&state)), LOCALHOST).await;
            assert_eq!(reject.map(|reject| reject[0]), Some(ACCESS_REJECT));
        })
        .await;
        assert!(log.contains("FAKE-EAP for client") && log.contains("the fake proof was not stored"), "{log}");
        assert_eq!(crate::logging::records_for(user)[0].handler, "eap-fake");

        // The chain refuses: the conversation's reason, and the account it gave.
        method.before.store(0, std::sync::atomic::Ordering::SeqCst);
        let user = "fake-hook-chain-refused";
        let (id, state) = fake_opened(&server, user).await;
        let log = logged_during(async {
            let reject = server.handle_auth(&fake_request(2, user, &fake_response(id, &[2]), Some(&state)), LOCALHOST).await;
            assert_eq!(reject.map(|reject| reject[0]), Some(ACCESS_REJECT));
        })
        .await;
        assert!(log.contains("the chain did not accept the subscriber"), "{log}");
        let records = crate::logging::records_for(user);
        assert_eq!((records[0].handler.as_str(), records[0].account.as_str(), records[0].decision.as_str()), ("eap-fake", "fake-user", "reject"));

        // Over a budget: an overload, logged on the budget's own line.
        let user = "fake-hook-budget";
        let (id, state) = fake_opened(&server, user).await;
        let log = logged_during(async {
            let reject = server.handle_auth(&fake_request(2, user, &fake_response(id, &[1, 0]), Some(&state)), LOCALHOST).await;
            assert_eq!(reject.map(|reject| reject[0]), Some(ACCESS_REJECT));
        })
        .await;
        assert!(log.contains("too many fakes"), "{log}");
        assert_eq!(crate::logging::records_for(user)[0].handler, "eap-fake");

        // What the method calls out of turn fails the conversation with its reason.
        let user = "fake-hook-out-of-turn";
        let (id, state) = fake_opened(&server, user).await;
        let log = logged_during(async {
            let reject = server.handle_auth(&fake_request(2, user, &fake_response(id, &[5]), Some(&state)), LOCALHOST).await;
            assert_eq!(reject.map(|reject| reject[0]), Some(ACCESS_REJECT));
        })
        .await;
        assert!(log.contains("a fake message out of turn"), "{log}");
        Ok(())
    }
}
