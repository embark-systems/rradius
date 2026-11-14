//! The endpoint mapper on port 135 (DCE 1.1 RPC appendix L, `ept_map`): which TCP port the
//! Netlogon interface listens on.
use super::ndr::{Reader, Writer};
use super::rpc::{self, RpcError, Syntax};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub const PORT: u16 = 135;
const OPNUM_EPT_MAP: u16 = 3;
const PROTOCOL_UUID: u8 = 0x0d;
const PROTOCOL_RPC_CO: u8 = 0x0b;
const PROTOCOL_TCP: u8 = 0x07;
const PROTOCOL_IP: u8 = 0x09;

fn floor(lhs: &[u8], rhs: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(lhs.len() as u16).to_le_bytes());
    out.extend_from_slice(lhs);
    out.extend_from_slice(&(rhs.len() as u16).to_le_bytes());
    out.extend_from_slice(rhs);
}

fn syntax_floor(syntax: Syntax, out: &mut Vec<u8>) {
    let mut lhs = vec![PROTOCOL_UUID];
    lhs.extend_from_slice(&syntax.uuid);
    lhs.extend_from_slice(&syntax.major.to_le_bytes());
    floor(&lhs, &syntax.minor.to_le_bytes(), out);
}

/// The tower asked about: `interface` over NDR, connection-oriented RPC on TCP over IP.
pub fn tower(interface: Syntax, port: u16, address: [u8; 4]) -> Vec<u8> {
    let mut tower = 5u16.to_le_bytes().to_vec();
    syntax_floor(interface, &mut tower);
    syntax_floor(rpc::NDR, &mut tower);
    floor(&[PROTOCOL_RPC_CO], &[0, 0], &mut tower);
    floor(&[PROTOCOL_TCP], &port.to_be_bytes(), &mut tower);
    floor(&[PROTOCOL_IP], &address, &mut tower);
    tower
}

/// The stub of an `ept_map` request for `interface`.
pub fn map_request(interface: Syntax) -> Vec<u8> {
    let tower = tower(interface, 0, [0; 4]);
    let mut writer = Writer::new();
    // object: a null UUID.
    writer.pointer();
    writer.bytes(&[0; 16]);
    // map_tower: twr_t { tower_length; [size_is(tower_length)] tower_octet_string }.
    writer.pointer();
    writer.u32(tower.len() as u32);
    writer.u32(tower.len() as u32);
    writer.bytes(&tower);
    // entry_handle: a null context handle, then max_towers.
    writer.align(4);
    writer.bytes(&[0; 20]);
    writer.u32(4);
    writer.into_bytes()
}

/// The TCP port of the first tower in an `ept_map` answer.
pub fn parse_map_response(stub: &[u8]) -> Result<u16, RpcError> {
    let mut reader = Reader::new(stub);
    reader.bytes(20)?;
    let towers = reader.u32()?;
    let count = reader.varying(Some(towers))?;
    let mut present = 0;
    for _ in 0..count {
        present += u32::from(reader.pointer()?);
    }
    if present == 0 {
        return Err(RpcError::Malformed("the endpoint mapper knows no Netlogon endpoint".to_string()));
    }
    let length = reader.u32()?;
    let conformance = reader.count()?;
    if conformance != length {
        return Err(RpcError::Malformed("inconsistent tower lengths".to_string()));
    }
    let tower = reader.bytes(length as usize)?;
    tcp_port(tower)
}

fn tcp_port(tower: &[u8]) -> Result<u16, RpcError> {
    let mut reader = Reader::new(tower);
    let floors = u16::from_le_bytes(reader.array()?);
    for _ in 0..floors {
        let lhs_len = u16::from_le_bytes(reader.array()?);
        let lhs = reader.bytes(usize::from(lhs_len))?;
        let rhs_len = u16::from_le_bytes(reader.array()?);
        let rhs = reader.bytes(usize::from(rhs_len))?;
        if lhs == [PROTOCOL_TCP] && rhs.len() == 2 {
            return Ok(u16::from_be_bytes([rhs[0], rhs[1]]));
        }
    }
    Err(RpcError::Malformed("the tower has no TCP port".to_string()))
}

/// Asks the endpoint mapper on `stream` for the port of `interface`.
pub async fn map(stream: &mut (impl AsyncRead + AsyncWrite + Unpin), interface: Syntax) -> Result<u16, RpcError> {
    stream.write_all(&rpc::bind(1, rpc::EPM, None)).await?;
    rpc::check_bind_ack(&rpc::parse(&rpc::read_pdu(stream).await?)?, 1)?;
    stream.write_all(&rpc::request(2, OPNUM_EPT_MAP, &map_request(interface), None, |_| Vec::new())?).await?;
    let response = rpc::parse(&rpc::read_pdu(stream).await?)?;
    if response.flags & rpc::PFC_LAST_FRAG == 0 {
        return Err(RpcError::Malformed("the endpoint mapper's answer is in more than one fragment".to_string()));
    }
    parse_map_response(rpc::response_stub(&response, 2)?)
}

/// An `ept_map` answer with one tower for `port`, as the test domain controller sends it.
#[cfg(test)]
pub fn map_response(port: u16) -> Vec<u8> {
    let tower = tower(rpc::NETLOGON, port, [192, 0, 2, 10]);
    let mut writer = Writer::new();
    writer.bytes(&[0; 20]);
    writer.u32(1);
    writer.u32(4);
    writer.u32(0);
    writer.u32(1);
    writer.pointer();
    writer.u32(tower.len() as u32);
    writer.u32(tower.len() as u32);
    writer.bytes(&tower);
    writer.align(4);
    writer.u32(0);
    writer.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tower_layout() {
        let tower = tower(rpc::NETLOGON, 49152, [192, 0, 2, 10]);
        // 2 + 2 syntax floors of 2+19+2+2 + three floors of 2+1+2+(2, 2, 4).
        assert_eq!(tower.len(), 2 + 2 * 25 + 7 + 7 + 9);
        assert_eq!(&tower[..5], &[5, 0, 19, 0, PROTOCOL_UUID]);
        assert_eq!(tcp_port(&tower).ok(), Some(49152));
    }

    #[test]
    fn test_map_response() -> Result<(), RpcError> {
        assert_eq!(parse_map_response(&map_response(49667))?, 49667);
        let mut none = Writer::new();
        none.bytes(&[0; 20]);
        for value in [0u32, 4, 0, 0, 0x16c9a0d6] {
            none.u32(value);
        }
        assert!(parse_map_response(&none.into_bytes()).is_err());
        assert!(parse_map_response(&map_response(1)[..40]).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_map_over_a_stream() -> Result<(), RpcError> {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let answer = tokio::spawn(async move {
            let bind = rpc::parse(&rpc::read_pdu(&mut server).await?)?;
            server.write_all(&rpc::server::bind_ack(bind.call_id, None)).await?;
            let request = rpc::parse(&rpc::read_pdu(&mut server).await?)?;
            let (opnum, stub) = rpc::server::parse_request(&request)?;
            assert_eq!((opnum, stub), (OPNUM_EPT_MAP, map_request(rpc::NETLOGON)));
            server.write_all(&rpc::server::response(request.call_id, 3, &map_response(49668), None, |_| Vec::new())).await?;
            Ok::<_, RpcError>(())
        });
        assert_eq!(map(&mut client, rpc::NETLOGON).await?, 49668);
        answer.await.map_err(|e| RpcError::Malformed(e.to_string()))??;
        Ok(())
    }
}
