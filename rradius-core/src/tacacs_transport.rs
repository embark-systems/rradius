//! TACACS+ in the clear inside a connection an extension made, as RFC 9887 has it inside TLS:
//! the extension listens, makes the connection and checks its peer; the TACACS+ server handles
//! the packets, every one with TAC_PLUS_UNENCRYPTED_FLAG, and asks the extension now and then
//! whether the peer may stay.

use crate::clients::Client;

/// What the extension that made a connection checks again while it is open.
pub trait PeerCheck: Send + Sync {
    /// Called on the first packet after `Timeouts::recheck` since the connection was made or
    /// last checked. `Err` closes the connection without an answer, logged as
    /// `TACACS+ over TLS connection from <source> closed: <reason>`.
    fn recheck(&self, network: &ipnet::IpNet, client: &Client) -> Result<(), String>;
}

/// What an extension that serves TACACS+ over TLS tells the core of it, among its settings:
/// the port it listens on, which the port without TLS cannot be, and whether the port without
/// TLS stays closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TacacsTlsServed {
    pub port: u16,
    pub tls_only: bool,
}
