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

/// Sends a Disconnect-Request or CoA-Request signed with `secret` up to `attempts` times, and
/// returns the first answer that proves to be from the NAS. Network errors are logged and count
/// as an attempt without an answer.
pub async fn send_and_wait(target: SocketAddr, packet: &[u8], secret: &[u8], timeout: Duration, attempts: u32) -> Option<RadiusPacket> {
    let socket = match connected_socket(target).await {
        Ok(socket) => socket,
        Err(e) => {
            log_line(tracing::Level::WARN, format!("dynamic authorization to {target}: {e}"));
            return None;
        }
    };
    let mut buf = [0u8; MAX_PACKET_LEN];
    for _ in 0..attempts {
        if let Err(e) = socket.send(packet).await {
            log_line(tracing::Level::WARN, format!("dynamic authorization to {target}: {e}"));
            continue;
        }
        let deadline = Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, socket.recv(&mut buf)).await {
                Err(_) => break,
                Ok(Err(e)) => {
                    log_line(tracing::Level::WARN, format!("dynamic authorization to {target}: {e}"));
                    break;
                }
                Ok(Ok(size)) => {
                    if let Some(answer) = checked_answer(packet, secret, &buf[..size]) {
                        return Some(answer);
                    }
                }
            }
        }
    }
    None
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
