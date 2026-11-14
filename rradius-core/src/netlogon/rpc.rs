//! Connection-oriented DCE/RPC PDUs (DCE 1.1 RPC chapter 12, with the security trailer of
//! MS-RPCE 2.2.2.11): bind, bind_ack, request, response and fault, and reading whole PDUs from
//! a stream.
use tokio::io::{AsyncRead, AsyncReadExt};

pub const PTYPE_REQUEST: u8 = 0;
pub const PTYPE_RESPONSE: u8 = 2;
pub const PTYPE_FAULT: u8 = 3;
pub const PTYPE_BIND: u8 = 11;
pub const PTYPE_BIND_ACK: u8 = 12;
pub const PTYPE_BIND_NAK: u8 = 13;
pub const PFC_FIRST_FRAG: u8 = 0x01;
pub const PFC_LAST_FRAG: u8 = 0x02;
const HEADER_LEN: usize = 16;
/// The security trailer before the auth value.
const TRAILER_LEN: usize = 8;
/// Where Windows and Samba pad the stub to before a security trailer.
const AUTH_PAD_ALIGNMENT: usize = 16;
/// The largest fragment either side sends: what this client offers in its bind.
pub const MAX_FRAGMENT: u16 = 5840;

/// An interface or transfer syntax: a UUID and a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Syntax {
    /// As it goes on the wire: the first three fields little-endian.
    pub uuid: [u8; 16],
    pub major: u16,
    pub minor: u16,
}

/// Turns the text form of a UUID into its little-endian wire form.
const fn uuid(text: &str) -> [u8; 16] {
    let digits = text.as_bytes();
    let mut raw = [0u8; 16];
    let mut i = 0;
    let mut n = 0;
    while i < digits.len() {
        if digits[i] == b'-' {
            i += 1;
            continue;
        }
        let high = hex_digit(digits[i]);
        let low = hex_digit(digits[i + 1]);
        raw[n] = high << 4 | low;
        n += 1;
        i += 2;
    }
    [raw[3], raw[2], raw[1], raw[0], raw[5], raw[4], raw[7], raw[6], raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15]]
}

const fn hex_digit(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        _ => digit - b'A' + 10,
    }
}

pub const NETLOGON: Syntax = Syntax { uuid: uuid("12345678-1234-abcd-ef00-01234567cffb"), major: 1, minor: 0 };
pub const EPM: Syntax = Syntax { uuid: uuid("e1af8308-5d1f-11c9-91a4-08002b14a0fa"), major: 3, minor: 0 };
pub const NDR: Syntax = Syntax { uuid: uuid("8a885d04-1ceb-11c9-9fe8-08002b104860"), major: 2, minor: 0 };

/// Why an exchange failed.
#[derive(Debug)]
pub enum RpcError {
    Io(std::io::Error),
    /// The peer sent something that is not a valid PDU or stub.
    Malformed(String),
    /// The peer answered with a fault PDU carrying this status.
    Fault(u32),
    /// The peer refused the bind, with its reason.
    BindRefused(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Malformed(what) => write!(f, "malformed RPC answer: {what}"),
            Self::Fault(status) => write!(f, "RPC fault 0x{status:08x}"),
            Self::BindRefused(why) => write!(f, "RPC bind refused: {why}"),
        }
    }
}

impl From<std::io::Error> for RpcError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<super::ndr::NdrError> for RpcError {
    fn from(e: super::ndr::NdrError) -> Self {
        Self::Malformed(e.0)
    }
}

/// The security trailer: who authenticates the PDU, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Auth {
    pub auth_type: u8,
    pub level: u8,
    pub context_id: u32,
}

fn header(ptype: u8, flags: u8, frag_length: usize, auth_length: usize, call_id: u32) -> Vec<u8> {
    let mut pdu = Vec::with_capacity(frag_length);
    pdu.extend_from_slice(&[5, 0, ptype, flags, 0x10, 0, 0, 0]);
    pdu.extend_from_slice(&(frag_length as u16).to_le_bytes());
    pdu.extend_from_slice(&(auth_length as u16).to_le_bytes());
    pdu.extend_from_slice(&call_id.to_le_bytes());
    pdu
}

fn trailer(auth: Auth, pad: usize) -> [u8; TRAILER_LEN] {
    let id = auth.context_id.to_le_bytes();
    [auth.auth_type, auth.level, pad as u8, 0, id[0], id[1], id[2], id[3]]
}

/// A bind for one presentation context, NDR over `syntax`, with an optional auth token.
pub fn bind(call_id: u32, syntax: Syntax, auth: Option<(Auth, &[u8])>) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&MAX_FRAGMENT.to_le_bytes());
    body.extend_from_slice(&MAX_FRAGMENT.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&[1, 0, 0, 0]);
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&[1, 0]);
    for syntax in [syntax, NDR] {
        body.extend_from_slice(&syntax.uuid);
        body.extend_from_slice(&syntax.major.to_le_bytes());
        body.extend_from_slice(&syntax.minor.to_le_bytes());
    }
    let token = auth.map_or(&[][..], |(_, token)| token);
    let trailer_len = if auth.is_some() { TRAILER_LEN } else { 0 };
    let length = HEADER_LEN + body.len() + trailer_len + token.len();
    let mut pdu = header(PTYPE_BIND, PFC_FIRST_FRAG | PFC_LAST_FRAG, length, token.len(), call_id);
    pdu.extend_from_slice(&body);
    if let Some((auth, token)) = auth {
        pdu.extend_from_slice(&trailer(auth, 0));
        pdu.extend_from_slice(token);
    }
    pdu
}

/// A request with its stub in one fragment. With `auth`, the stub is padded and `protect` is
/// given the padded stub to seal in place; it returns the auth value of length `auth_len`.
pub fn request(call_id: u32, opnum: u16, stub: &[u8], auth: Option<(Auth, usize)>, protect: impl FnOnce(&mut [u8]) -> Vec<u8>) -> Result<Vec<u8>, RpcError> {
    let (pad, auth_len) = match auth {
        Some((_, auth_len)) => ((AUTH_PAD_ALIGNMENT - stub.len() % AUTH_PAD_ALIGNMENT) % AUTH_PAD_ALIGNMENT, auth_len),
        None => (0, 0),
    };
    let trailer_len = if auth.is_some() { TRAILER_LEN } else { 0 };
    let length = HEADER_LEN + 8 + stub.len() + pad + trailer_len + auth_len;
    if length > usize::from(MAX_FRAGMENT) {
        return Err(RpcError::Malformed(format!("a request of {length} octets does not fit one fragment")));
    }
    let mut pdu = header(PTYPE_REQUEST, PFC_FIRST_FRAG | PFC_LAST_FRAG, length, auth_len, call_id);
    pdu.extend_from_slice(&(stub.len() as u32).to_le_bytes());
    pdu.extend_from_slice(&0u16.to_le_bytes());
    pdu.extend_from_slice(&opnum.to_le_bytes());
    let start = pdu.len();
    pdu.extend_from_slice(stub);
    pdu.resize(start + stub.len() + pad, 0);
    if let Some((auth, auth_len)) = auth {
        let value = protect(&mut pdu[start..]);
        if value.len() != auth_len {
            return Err(RpcError::Malformed("the auth value has the wrong length".to_string()));
        }
        pdu.extend_from_slice(&trailer(auth, pad));
        pdu.extend_from_slice(&value);
    }
    Ok(pdu)
}

/// One PDU as read from the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdu {
    pub ptype: u8,
    pub flags: u8,
    pub call_id: u32,
    /// What follows the 16-octet header, up to the security trailer.
    pub body: Vec<u8>,
    /// The security trailer's pad length and the auth value, when the PDU has one.
    pub auth: Option<(Auth, u8, Vec<u8>)>,
}

/// Splits a whole PDU into its parts, checking the header.
pub fn parse(pdu: &[u8]) -> Result<Pdu, RpcError> {
    let malformed = |what: &str| RpcError::Malformed(what.to_string());
    if pdu.len() < HEADER_LEN || pdu[0] != 5 || pdu[1] != 0 {
        return Err(malformed("not a version 5.0 PDU"));
    }
    // Only little-endian integers and ASCII characters are understood.
    if pdu[4] & 0xf0 != 0x10 {
        return Err(malformed("not little-endian"));
    }
    let frag_length = usize::from(u16::from_le_bytes([pdu[8], pdu[9]]));
    let auth_length = usize::from(u16::from_le_bytes([pdu[10], pdu[11]]));
    if frag_length != pdu.len() {
        return Err(malformed("the fragment length does not match"));
    }
    let call_id = u32::from_le_bytes([pdu[12], pdu[13], pdu[14], pdu[15]]);
    let (body, auth) = if auth_length == 0 {
        (pdu[HEADER_LEN..].to_vec(), None)
    } else {
        let Some(trailer_at) = frag_length.checked_sub(auth_length + TRAILER_LEN).filter(|at| *at >= HEADER_LEN) else {
            return Err(malformed("the auth length does not fit"));
        };
        let trailer = &pdu[trailer_at..trailer_at + TRAILER_LEN];
        let auth = Auth { auth_type: trailer[0], level: trailer[1], context_id: u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]) };
        (pdu[HEADER_LEN..trailer_at].to_vec(), Some((auth, trailer[2], pdu[trailer_at + TRAILER_LEN..].to_vec())))
    };
    Ok(Pdu { ptype: pdu[2], flags: pdu[3], call_id, body, auth })
}

/// Reads one whole PDU: its header says how long it is.
pub async fn read_pdu(stream: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>, RpcError> {
    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header).await?;
    let frag_length = usize::from(u16::from_le_bytes([header[8], header[9]]));
    if frag_length < HEADER_LEN {
        return Err(RpcError::Malformed(format!("a fragment length of {frag_length}")));
    }
    let mut pdu = vec![0u8; frag_length];
    pdu[..HEADER_LEN].copy_from_slice(&header);
    stream.read_exact(&mut pdu[HEADER_LEN..]).await?;
    Ok(pdu)
}

/// What a bind_ack accepted: the auth value, if any, for the caller to check.
pub fn check_bind_ack(pdu: &Pdu, call_id: u32) -> Result<Option<Vec<u8>>, RpcError> {
    if pdu.ptype == PTYPE_BIND_NAK {
        let reason = pdu.body.get(..2).map_or(0, |raw| u16::from_le_bytes([raw[0], raw[1]]));
        return Err(RpcError::BindRefused(format!("bind_nak, reason {reason}")));
    }
    if pdu.ptype != PTYPE_BIND_ACK || pdu.call_id != call_id {
        return Err(RpcError::Malformed(format!("expected a bind_ack, got PDU type {}", pdu.ptype)));
    }
    let mut reader = super::ndr::Reader::new(&pdu.body);
    let _max_xmit = reader.u16()?;
    let _max_recv = reader.u16()?;
    let _assoc_group = reader.u32()?;
    let address_length = reader.u16()?;
    reader.bytes(usize::from(address_length))?;
    // Results start 4-aligned from the start of the PDU, which is 16 octets before the body.
    reader.align(4)?;
    let results = reader.u8()?;
    reader.bytes(3)?;
    if results == 0 {
        return Err(RpcError::Malformed("a bind_ack without results".to_string()));
    }
    let result = reader.u16()?;
    let reason = reader.u16()?;
    if result != 0 {
        return Err(RpcError::BindRefused(format!("presentation context result {result}, reason {reason}")));
    }
    Ok(pdu.auth.as_ref().map(|(_, _, value)| value.clone()))
}

/// The stub of a response fragment, or the fault it carries.
pub fn response_stub(pdu: &Pdu, call_id: u32) -> Result<&[u8], RpcError> {
    if pdu.call_id != call_id {
        return Err(RpcError::Malformed(format!("an answer to call {} while waiting for {call_id}", pdu.call_id)));
    }
    match pdu.ptype {
        PTYPE_RESPONSE => pdu.body.get(8..).ok_or_else(|| RpcError::Malformed("a short response".to_string())),
        PTYPE_FAULT => {
            let status = pdu.body.get(8..12).map_or(0, |raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]));
            Err(RpcError::Fault(status))
        }
        other => Err(RpcError::Malformed(format!("expected a response, got PDU type {other}"))),
    }
}

/// Builds the PDUs a server sends, for the test domain controller.
#[cfg(test)]
pub mod server {
    use super::*;

    pub fn bind_ack(call_id: u32, auth: Option<(Auth, &[u8])>) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&MAX_FRAGMENT.to_le_bytes());
        body.extend_from_slice(&MAX_FRAGMENT.to_le_bytes());
        body.extend_from_slice(&0x1234u32.to_le_bytes());
        let address = b"49152\0";
        body.extend_from_slice(&(address.len() as u16).to_le_bytes());
        body.extend_from_slice(address);
        while !(HEADER_LEN + body.len()).is_multiple_of(4) {
            body.push(0);
        }
        body.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
        body.extend_from_slice(&NDR.uuid);
        body.extend_from_slice(&NDR.major.to_le_bytes());
        body.extend_from_slice(&NDR.minor.to_le_bytes());
        let token = auth.map_or(&[][..], |(_, token)| token);
        let trailer_len = if auth.is_some() { TRAILER_LEN } else { 0 };
        let length = HEADER_LEN + body.len() + trailer_len + token.len();
        let mut pdu = header(PTYPE_BIND_ACK, PFC_FIRST_FRAG | PFC_LAST_FRAG, length, token.len(), call_id);
        pdu.extend_from_slice(&body);
        if let Some((auth, token)) = auth {
            pdu.extend_from_slice(&trailer(auth, 0));
            pdu.extend_from_slice(token);
        }
        pdu
    }

    /// One response fragment carrying `stub`, sealed with `protect` when `auth` is given.
    pub fn response(call_id: u32, flags: u8, stub: &[u8], auth: Option<(Auth, usize)>, protect: impl FnOnce(&mut [u8]) -> Vec<u8>) -> Vec<u8> {
        let (pad, auth_len) = match auth {
            Some((_, auth_len)) => ((AUTH_PAD_ALIGNMENT - stub.len() % AUTH_PAD_ALIGNMENT) % AUTH_PAD_ALIGNMENT, auth_len),
            None => (0, 0),
        };
        let trailer_len = if auth.is_some() { TRAILER_LEN } else { 0 };
        let length = HEADER_LEN + 8 + stub.len() + pad + trailer_len + auth_len;
        let mut pdu = header(PTYPE_RESPONSE, flags, length, auth_len, call_id);
        pdu.extend_from_slice(&(stub.len() as u32).to_le_bytes());
        pdu.extend_from_slice(&[0, 0, 0, 0]);
        let start = pdu.len();
        pdu.extend_from_slice(stub);
        pdu.resize(start + stub.len() + pad, 0);
        if let Some((auth, _)) = auth {
            let value = protect(&mut pdu[start..]);
            pdu.extend_from_slice(&trailer(auth, pad));
            pdu.extend_from_slice(&value);
        }
        pdu
    }

    pub fn fault(call_id: u32, status: u32) -> Vec<u8> {
        let mut pdu = header(PTYPE_FAULT, PFC_FIRST_FRAG | PFC_LAST_FRAG, HEADER_LEN + 16, 0, call_id);
        pdu.extend_from_slice(&[0; 8]);
        pdu.extend_from_slice(&status.to_le_bytes());
        pdu.extend_from_slice(&[0; 4]);
        pdu
    }

    /// The request a client sent: its opnum and padded stub, and its trailer.
    pub fn parse_request(pdu: &Pdu) -> Result<(u16, Vec<u8>), RpcError> {
        if pdu.ptype != PTYPE_REQUEST || pdu.body.len() < 8 {
            return Err(RpcError::Malformed("not a request".to_string()));
        }
        Ok((u16::from_le_bytes([pdu.body[6], pdu.body[7]]), pdu.body[8..].to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uuid_wire_form() {
        // The NDR transfer syntax as it appears in every bind.
        assert_eq!(NDR.uuid, [0x04, 0x5d, 0x88, 0x8a, 0xeb, 0x1c, 0xc9, 0x11, 0x9f, 0xe8, 0x08, 0x00, 0x2b, 0x10, 0x48, 0x60]);
    }

    #[test]
    fn test_bind_layout() -> Result<(), RpcError> {
        let pdu = bind(1, NETLOGON, None);
        assert_eq!(pdu.len(), 72);
        assert_eq!(&pdu[..4], &[5, 0, PTYPE_BIND, 3]);
        assert_eq!(u16::from_le_bytes([pdu[8], pdu[9]]), 72);
        // The abstract syntax, then the NDR transfer syntax.
        assert_eq!(&pdu[32..48], &NETLOGON.uuid);
        assert_eq!(&pdu[52..68], &NDR.uuid);
        let auth = Auth { auth_type: 0x44, level: 6, context_id: 1 };
        let with_token = bind(2, NETLOGON, Some((auth, b"token")));
        let parsed = parse(&with_token)?;
        assert_eq!(parsed.auth, Some((auth, 0, b"token".to_vec())));
        Ok(())
    }

    #[test]
    fn test_bind_ack() -> Result<(), RpcError> {
        let ack = parse(&server::bind_ack(3, None))?;
        assert_eq!(check_bind_ack(&ack, 3)?, None);
        assert!(check_bind_ack(&ack, 4).is_err());
        Ok(())
    }

    #[test]
    fn test_request_and_response() -> Result<(), RpcError> {
        let auth = Auth { auth_type: 0x44, level: 6, context_id: 1 };
        let pdu = request(5, 39, &[1, 2, 3], Some((auth, 4)), |stub| {
            assert_eq!(stub.len(), 16);
            vec![9; 4]
        })?;
        let parsed = parse(&pdu)?;
        assert_eq!(parsed.auth, Some((auth, 13, vec![9; 4])));
        let (opnum, stub) = server::parse_request(&parsed)?;
        assert_eq!((opnum, &stub[..3]), (39, &[1u8, 2, 3][..]));
        let response = parse(&server::response(5, 3, &[7; 20], None, |_| Vec::new()))?;
        assert_eq!(response_stub(&response, 5)?, &[7; 20]);
        assert!(response_stub(&response, 6).is_err());
        assert!(matches!(response_stub(&parse(&server::fault(5, 0x1c010003))?, 5), Err(RpcError::Fault(0x1c010003))));
        Ok(())
    }

    #[test]
    fn test_parse_refuses() {
        for bad in [&[5u8, 0, 2][..], &[4, 0, 2, 3, 0x10, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0], &[5, 0, 2, 3, 0x00, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0]] {
            assert!(parse(bad).is_err());
        }
        // An auth length longer than the PDU.
        let mut pdu = server::response(1, 3, &[0; 8], None, |_| Vec::new());
        pdu[10] = 200;
        assert!(parse(&pdu).is_err());
        // A frag_length that does not match.
        let mut pdu = server::fault(1, 5);
        pdu[8] = 99;
        assert!(parse(&pdu).is_err());
    }

    #[tokio::test]
    async fn test_read_pdu() -> Result<(), RpcError> {
        let pdu = server::fault(1, 5);
        let mut stream = [pdu.clone(), vec![1, 2, 3]].concat();
        let mut reader = &stream[..];
        assert_eq!(read_pdu(&mut reader).await?, pdu);
        stream[8] = 4;
        let mut reader = &stream[..];
        assert!(read_pdu(&mut reader).await.is_err());
        // Ends in the middle.
        let mut reader = &pdu[..20];
        assert!(read_pdu(&mut reader).await.is_err());
        Ok(())
    }
}
