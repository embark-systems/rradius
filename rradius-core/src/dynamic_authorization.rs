//! What a script's `send_disconnect` and `send_coa` send through: dynamic authorization (RFC 5176)
//! that a program's extension can offer the Lua engine.

use crate::common::AttrValue;
use crate::config::ServerConfig;
use crate::dictionary::Dictionary;
use crate::lua_engine::log_line;
use crate::radius::{md5_with_secret, RadiusPacket, MAX_PACKET_LEN};
use futures::future::BoxFuture;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::Instant;

/// The codes of RFC 5176.
pub const DISCONNECT_REQUEST: u8 = 40;
pub const DISCONNECT_ACK: u8 = 41;
pub const DISCONNECT_NAK: u8 = 42;
pub const COA_REQUEST: u8 = 43;
pub const COA_ACK: u8 = 44;
pub const COA_NAK: u8 = 45;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RequestType {
    Disconnect,
    Coa,
}

/// What the NAS said, if anything.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    Ack,
    /// With the Error-Cause, when there was one.
    Nak(Option<u32>),
    Timeout,
}

/// What `send_disconnect` and `send_coa` send through.
pub trait DynamicAuthorization: Send + Sync {
    /// Checks the target and the attributes and makes the request, without sending it; the
    /// errors are mistakes in the call.
    fn prepare(&self, kind: RequestType, target: &str, attributes: &[(String, AttrValue)]) -> Result<Box<dyn PreparedRequest>, String>;
}

/// A request ready to send.
pub trait PreparedRequest: Send {
    /// Sends it and waits for the answer.
    fn send(self: Box<Self>) -> BoxFuture<'static, Outcome>;
}

/// What a sender is made from, at the start and at each reload.
pub struct DynamicAuthorizationContext<'a> {
    pub config: &'a ServerConfig,
    pub dictionary: &'a Arc<Dictionary>,
}

pub async fn connected_socket(target: SocketAddr) -> std::io::Result<UdpSocket> {
    let local: IpAddr = match target {
        SocketAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        SocketAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let socket = UdpSocket::bind((local, 0)).await?;
    socket.connect(target).await?;
    Ok(socket)
}

pub fn random_identifier() -> Result<u8, getrandom::Error> {
    let mut byte = [0u8; 1];
    getrandom::fill(&mut byte)?;
    Ok(byte[0])
}

/// The octets a [`send_and_wait_counted`] call wrote to the socket and read from it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WaitedBytes {
    /// Every send that succeeded, each retransmission counted again.
    pub sent: u64,
    /// Every datagram received, whether or not it answered the request.
    pub received: u64,
}

/// Sends a Disconnect-Request or CoA-Request signed with `secret` up to `attempts` times, and
/// returns the first answer that proves to be from the NAS. Network errors are logged and count
/// as an attempt without an answer.
pub async fn send_and_wait(target: SocketAddr, packet: &[u8], secret: &[u8], timeout: Duration, attempts: u32) -> Option<RadiusPacket> {
    send_and_wait_counted(target, packet, secret, timeout, attempts).await.0
}

/// [`send_and_wait`], and how many octets it sent and received.
pub async fn send_and_wait_counted(target: SocketAddr, packet: &[u8], secret: &[u8], timeout: Duration, attempts: u32) -> (Option<RadiusPacket>, WaitedBytes) {
    let mut bytes = WaitedBytes::default();
    let socket = match connected_socket(target).await {
        Ok(socket) => socket,
        Err(e) => {
            log_line(tracing::Level::WARN, format!("dynamic authorization to {target}: {e}"));
            return (None, bytes);
        }
    };
    let mut buf = [0u8; MAX_PACKET_LEN];
    for _ in 0..attempts {
        if let Err(e) = socket.send(packet).await {
            log_line(tracing::Level::WARN, format!("dynamic authorization to {target}: {e}"));
            continue;
        }
        bytes.sent += packet.len() as u64;
        let deadline = Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, socket.recv(&mut buf)).await {
                Err(_) => break,
                Ok(Err(e)) => {
                    log_line(tracing::Level::WARN, format!("dynamic authorization to {target}: {e}"));
                    break;
                }
                Ok(Ok(size)) => {
                    bytes.received += size as u64;
                    if let Some(answer) = checked_answer(packet, secret, &buf[..size]) {
                        return (Some(answer), bytes);
                    }
                }
            }
        }
    }
    (None, bytes)
}

/// The ACK or NAK to `request` in `data`, if its Identifier fits and it is signed with `secret`;
/// `None` when the datagram does not answer this request: wrong kind, wrong Identifier, or not
/// signed with the secret.
pub fn checked_answer(request: &[u8], secret: &[u8], data: &[u8]) -> Option<RadiusPacket> {
    let response = RadiusPacket::parse(data).ok()?;
    // An ACK is the request's code plus one, a NAK plus two (RFC 5176 section 3).
    if response.identifier != request[1] || (response.code != request[0] + 1 && response.code != request[0] + 2) {
        return None;
    }
    let request_authenticator: [u8; 16] = request[4..20].try_into().ok()?;
    let mut signed = data.to_vec();
    signed[4..20].copy_from_slice(&request_authenticator);
    if !crate::passwords::constant_time_eq(&md5_with_secret(&signed, secret), &data[4..20]) {
        return None;
    }
    // Message-Authenticator is computed with the Request Authenticator in place.
    let as_signed = RadiusPacket { authenticator: request_authenticator, ..response };
    if as_signed.verify_message_authenticator(secret) == Some(false) {
        return None;
    }
    Some(RadiusPacket { authenticator: response_authenticator(data), ..as_signed })
}

fn response_authenticator(data: &[u8]) -> [u8; 16] {
    let mut authenticator = [0u8; 16];
    authenticator.copy_from_slice(&data[4..20]);
    authenticator
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"coa-test-secret";

    /// A NAS on loopback: `behave` gets each request it reads, in order, and returns the
    /// datagrams to write back.
    async fn nas(behave: impl Fn(usize, &[u8]) -> Vec<Vec<u8>> + Send + 'static) -> Result<SocketAddr, String> {
        let socket = UdpSocket::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        let address = socket.local_addr().map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            let mut buffer = [0u8; MAX_PACKET_LEN];
            let mut count = 0;
            while let Ok((size, from)) = socket.recv_from(&mut buffer).await {
                for datagram in behave(count, &buffer[..size]) {
                    let _ = socket.send_to(&datagram, from).await;
                }
                count += 1;
            }
        });
        Ok(address)
    }

    fn request() -> Vec<u8> {
        RadiusPacket::create_signed_request(DISCONNECT_REQUEST, 7, vec![(1, b"alice".to_vec())], SECRET)
    }

    fn ack_to(request: &[u8]) -> Result<Vec<u8>, String> {
        let parsed = RadiusPacket::parse(request).map_err(|e| format!("{e:?}"))?;
        Ok(RadiusPacket::create_response(DISCONNECT_ACK, &parsed, vec![], SECRET))
    }

    /// The first request goes unanswered and the second is answered: both sends and the answer
    /// are counted.
    #[tokio::test]
    async fn test_a_retransmission_is_counted() -> Result<(), String> {
        let packet = request();
        let address = nas(|count, request| if count == 1 { ack_to(request).into_iter().collect() } else { Vec::new() }).await?;
        let (answer, bytes) = send_and_wait_counted(address, &packet, SECRET, Duration::from_millis(150), 2).await;
        assert_eq!(answer.map(|answer| answer.code), Some(DISCONNECT_ACK));
        assert_eq!(bytes, WaitedBytes { sent: 2 * packet.len() as u64, received: 20 });
        Ok(())
    }

    /// A datagram that is not the answer is read and counted before the real one.
    #[tokio::test]
    async fn test_a_datagram_that_is_not_the_answer_is_counted() -> Result<(), String> {
        let packet = request();
        let address = nas(|_, request| {
            let mut replies = vec![vec![0u8; 30]];
            replies.extend(ack_to(request));
            replies
        })
        .await?;
        let (answer, bytes) = send_and_wait_counted(address, &packet, SECRET, Duration::from_millis(500), 1).await;
        assert!(answer.is_some());
        assert_eq!(bytes, WaitedBytes { sent: packet.len() as u64, received: 50 });
        Ok(())
    }

    /// Nothing answers: the sends are counted and there is no answer.
    #[tokio::test]
    async fn test_no_answer_counts_only_the_sends() -> Result<(), String> {
        let packet = request();
        let address = nas(|_, _| Vec::new()).await?;
        let (answer, bytes) = send_and_wait_counted(address, &packet, SECRET, Duration::from_millis(50), 3).await;
        assert!(answer.is_none());
        assert_eq!(bytes, WaitedBytes { sent: 3 * packet.len() as u64, received: 0 });
        Ok(())
    }

    /// `send_and_wait` answers as before.
    #[tokio::test]
    async fn test_send_and_wait_answers() -> Result<(), String> {
        let packet = request();
        let address = nas(|_, request| ack_to(request).into_iter().collect()).await?;
        let answer = send_and_wait(address, &packet, SECRET, Duration::from_millis(500), 1).await;
        assert_eq!(answer.map(|answer| answer.code), Some(DISCONNECT_ACK));
        Ok(())
    }
}
