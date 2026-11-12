//! RADIUS over TLS (RFC 6614) for the load: one connection per socket, opened and through its
//! handshake before the load starts, then read and written without blocking.
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection};
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

/// The shortest and longest a RADIUS packet can say it is.
const MIN_PACKET: usize = 20;
const MAX_PACKET: usize = 4096;

/// Where RadSec connections go and how they are made, also to make them again.
pub struct TlsTarget {
    pub address: SocketAddr,
    /// `--bind`: the source address.
    pub bind: Option<IpAddr>,
    pub config: Arc<ClientConfig>,
    pub name: ServerName<'static>,
    pub timeout: Duration,
}

/// One connection.
pub struct TlsLink {
    stream: mio::net::TcpStream,
    tls: ClientConnection,
    /// Plaintext read so far that is not yet a whole packet.
    received: Vec<u8>,
}

/// What reading a connection gave.
pub struct Received {
    pub packets: Vec<Vec<u8>>,
    /// Why the connection is over, if it is: what was read before still counts.
    pub closed: Option<String>,
}

impl TlsLink {
    /// Connects and finishes the handshake, each step within the target's timeout.
    pub fn connect(target: &TlsTarget) -> Result<TlsLink, String> {
        let mut stream = open(target).map_err(|e| format!("connecting to {}: {e}", target.address))?;
        let _ = stream.set_nodelay(true);
        stream.set_read_timeout(Some(target.timeout)).map_err(|e| format!("connecting to {}: {e}", target.address))?;
        stream.set_write_timeout(Some(target.timeout)).map_err(|e| format!("connecting to {}: {e}", target.address))?;
        let mut tls = ClientConnection::new(target.config.clone(), target.name.clone()).map_err(|e| format!("TLS: {e}"))?;
        // What waits to be written is bounded anyway: at most 256 requests of at most 4096
        // octets on a connection. A full buffer would otherwise look like a broken connection.
        tls.set_buffer_limit(None);
        while tls.is_handshaking() {
            tls.complete_io(&mut stream).map_err(|e| format!("TLS handshake with {}: {e}", target.address))?;
        }
        // The server may still be checking our certificate: a refusal shows as the first read failing.
        stream.set_nonblocking(true).map_err(|e| format!("connecting to {}: {e}", target.address))?;
        Ok(TlsLink { stream: mio::net::TcpStream::from_std(stream), tls, received: Vec::new() })
    }

    pub fn source(&mut self) -> &mut mio::net::TcpStream {
        &mut self.stream
    }

    /// Hands `packet` to TLS and writes what it can. True when everything went out; false
    /// when the rest waits for the socket to be writable.
    pub fn send(&mut self, packet: &[u8]) -> io::Result<bool> {
        self.tls.writer().write_all(packet)?;
        self.flush()
    }

    /// Writes what TLS holds until it is all out or the socket is full.
    pub fn flush(&mut self) -> io::Result<bool> {
        while self.tls.wants_write() {
            match self.tls.write_tls(&mut self.stream) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "the connection takes no more")),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    /// Reads everything there is, and the whole packets in it.
    pub fn receive(&mut self) -> Received {
        let mut received = Received { packets: Vec::new(), closed: None };
        loop {
            match self.tls.read_tls(&mut self.stream) {
                Ok(0) => {
                    received.closed = Some("the server closed the connection".to_string());
                    break;
                }
                Ok(_) => {
                    if let Err(error) = self.tls.process_new_packets() {
                        received.closed = Some(format!("TLS: {error}"));
                        break;
                    }
                    // rustls holds only so much plaintext: take it out before reading more.
                    if let Err(error) = self.take_plaintext() {
                        received.closed = Some(error);
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    received.closed = Some(format!("reading: {error}"));
                    break;
                }
            }
        }
        while let [_, _, high, low, ..] = self.received[..] {
            let length = usize::from(u16::from_be_bytes([high, low]));
            if !(MIN_PACKET..=MAX_PACKET).contains(&length) {
                received.closed = Some(format!("a packet that says it is {length} octets"));
                break;
            }
            if self.received.len() < length {
                break;
            }
            received.packets.push(self.received.drain(..length).collect());
        }
        // TLS may have something to say back, such as an alert.
        if received.closed.is_none() {
            if let Err(error) = self.flush() {
                received.closed = Some(format!("writing: {error}"));
            }
        }
        received
    }

    /// Moves what TLS has decrypted into `received`.
    fn take_plaintext(&mut self) -> Result<(), String> {
        let mut buffer = [0u8; MAX_PACKET];
        loop {
            match self.tls.reader().read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(read) => self.received.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(format!("TLS: {error}")),
            }
        }
    }
}

/// A TCP connection to the target, from `--bind` when given.
fn open(target: &TlsTarget) -> io::Result<std::net::TcpStream> {
    let Some(bind) = target.bind else { return std::net::TcpStream::connect_timeout(&target.address, target.timeout) };
    let socket = socket2::Socket::new(socket2::Domain::for_address(target.address), socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    socket.bind(&SocketAddr::new(bind, 0).into())?;
    socket.connect_timeout(&target.address.into(), target.timeout)?;
    Ok(socket.into())
}
