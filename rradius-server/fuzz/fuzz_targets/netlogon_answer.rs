//! What a domain controller sends back: RPC PDUs, endpoint mapper answers, sealed stubs and
//! the validation information of a logon. The first octet picks the decoder; none may crash,
//! however the rest is made.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rradius_core::netlogon::{crypto, epm, logon, rpc};

fuzz_target!(|data: &[u8]| {
    let Some((selector, rest)) = data.split_first() else { return };
    match selector % 4 {
        0 => {
            if let Ok(pdu) = rpc::parse(rest) {
                let _ = rpc::check_bind_ack(&pdu, 1);
                let _ = rpc::response_stub(&pdu, 2);
            }
        }
        1 => {
            let _ = epm::parse_map_response(rest);
        }
        2 => {
            if let Ok(answer) = logon::parse_answer(rest, &[7; 16]) {
                if let Some(validation) = answer.validation {
                    let groups = validation.group_sids();
                    let _ = (validation.is_guest(), groups.iter().map(ToString::to_string).count());
                }
            }
        }
        _ => {
            if rest.len() >= crypto::SIGNATURE_LEN {
                let (signature, data) = rest.split_at(crypto::SIGNATURE_LEN);
                let mut data = data.to_vec();
                let _ = crypto::unseal(&[7; 16], 0, false, signature, &mut data);
            }
        }
    }
});
