//! A TACACS+ packet off a connection, and its body as it would be after decryption.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rradius_core::tacacs::{parse_acct_request, parse_authen_start, parse_author_request, parse_continue, read_packet, start_credentials, TacacsPacket};
use std::sync::LazyLock;

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| tokio::runtime::Builder::new_current_thread().build().expect("a runtime"));

fuzz_target!(|data: &[u8]| {
    let _ = TacacsPacket::parse(data);
    let mut reader = data;
    let _ = RUNTIME.block_on(read_packet(&mut reader, 65536));
    if let Some(attributes) = parse_authen_start(data) {
        let _ = start_credentials(&attributes);
    }
    let _ = parse_author_request(data);
    let _ = parse_acct_request(data);
    let _ = parse_continue(data);
});
