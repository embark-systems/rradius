#![no_main]
use libfuzzer_sys::fuzz_target;
use rradius_core::eap::{parse_avps, parse_mschapv2_response, parse_tlv_result, parse_tls_fragment, EapPacket};

// Neither parser may panic, and what parses has to come back the same.
fuzz_target!(|data: &[u8]| {
    if let Some(packet) = EapPacket::parse(data) {
        assert_eq!(packet.encode(), data);
        if let Some((_, rest)) = packet.response() {
            let _ = parse_tls_fragment(rest);
        }
    }
    let _ = parse_tls_fragment(data);
    let _ = parse_mschapv2_response(data);
    let _ = parse_avps(data);
    let _ = parse_tlv_result(data, data.first().copied().unwrap_or(0));
});
