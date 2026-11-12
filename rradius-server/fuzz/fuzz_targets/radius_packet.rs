//! Everything done with a datagram from anyone, before its shared secret has been checked.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rradius_core::dictionary::Dictionary;
use rradius_core::radius::{encode_response_attributes, request_attributes, request_credentials, RadiusPacket};
use std::sync::LazyLock;

static DICTIONARY: LazyLock<Dictionary> = LazyLock::new(|| {
    let mut dictionary = Dictionary::new();
    dictionary.load_from_file(concat!(env!("CARGO_MANIFEST_DIR"), "/../../share/dictionary")).expect("the bundled dictionary loads");
    dictionary
});

fuzz_target!(|data: &[u8]| {
    let Ok(packet) = RadiusPacket::parse(data) else { return };
    let _ = packet.verify_message_authenticator(b"secret");
    let attributes = request_attributes(&packet, &DICTIONARY, b"secret");
    // What was read can be written, or says why not, and no attribute is more than its header can
    // say. One at a time too: one that cannot be written, as User-Password, must not hide the rest.
    let encode = |attributes: &[(String, _)]| {
        let names = attributes.iter().map(|(name, value)| (name.as_str(), value));
        if let Ok(encoded) = encode_response_attributes(&DICTIONARY, names, b"secret", &[0; 16], &mut Vec::new()) {
            assert!(encoded.iter().all(|(_, value)| value.len() <= 253));
        }
    };
    for attribute in attributes.chunks(1) {
        encode(attribute);
    }
    encode(&attributes);
    let _ = request_credentials(&packet, b"secret");
});
