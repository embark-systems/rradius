//! Address attribute values from text: no text may crash the encoder, and what it
//! accepts has to read back as text that encodes to the same octets.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rradius_core::dictionary::Dictionary;
use rradius_core::common::AttrValue;
use rradius_core::radius::{encode_response_attribute, request_attributes, RadiusPacket};
use std::sync::LazyLock;

const TYPES: [&str; 6] = ["ipv6addr", "ipv6prefix", "ipv4prefix", "ifid", "ether", "combo-ip"];

static DICTIONARY: LazyLock<Dictionary> = LazyLock::new(|| {
    let mut dictionary = Dictionary::new();
    for (index, kind) in TYPES.iter().enumerate() {
        dictionary.add_attribute(kind, kind, 0, vec![200 + index as u32], None);
    }
    dictionary
});

fn encode(kind: &str, value: &AttrValue) -> Result<(u8, Vec<u8>), String> {
    encode_response_attribute(&DICTIONARY, kind, value, b"", &[0; 16], &mut Vec::new())
}

fuzz_target!(|data: &[u8]| {
    let Some((selector, text)) = data.split_first() else { return };
    let kind = TYPES[usize::from(*selector) % TYPES.len()];
    let Ok((number, octets)) = encode(kind, &AttrValue::Bytes(text.to_vec())) else { return };
    let packet = RadiusPacket { code: 1, identifier: 0, authenticator: [0; 16], attributes: vec![(number, octets.clone())] };
    let (_, decoded) = request_attributes(&packet, &DICTIONARY, b"").remove(0);
    assert!(matches!(decoded, AttrValue::Bytes(ref bytes) if bytes != &octets || octets.is_empty()), "{kind} stayed raw: {octets:?}");
    let (_, again) = encode(kind, &decoded).expect("what was decoded encodes");
    assert_eq!(again, octets, "{kind}");
});
