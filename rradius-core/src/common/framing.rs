//! RADIUS over a byte stream (RFC 6614 section 2.3, RFC 6613): each packet framed by its own
//! Length field, as RadSec and the RadSec connections to upstream servers carry them.

use crate::radius::MAX_PACKET_LEN;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt};

/// The shortest RADIUS packet: its header.
const MIN_PACKET_LEN: usize = 20;

/// Reads one packet, framed by its Length field; `None` when the client closed between
/// packets. A bad Length is an error: the start of the next packet cannot be found.
pub async fn read_packet<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut packet = vec![0u8; 4];
    match reader.read_exact(&mut packet).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let length = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    if !(MIN_PACKET_LEN..=MAX_PACKET_LEN).contains(&length) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad packet length {length}")));
    }
    packet.resize(length, 0);
    reader.read_exact(&mut packet[4..]).await?;
    Ok(Some(packet))
}
