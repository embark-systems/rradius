//! Dictionary text. It comes from the operator, not the network, but a typing mistake
//! should get an error rather than a crash.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rradius_core::dictionary::Dictionary;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let _ = Dictionary::new().load_from_str("/nonexistent/dictionary", text);
});
