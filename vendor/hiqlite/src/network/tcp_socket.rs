use crate::Error;
use axum_server::accept::Accept;
use socket2::{Socket, TcpKeepalive};
use std::future::Ready;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
pub use tokio::net::TcpListener;
use tokio::task;
use tracing::debug;

fn standard_keepalive() -> TcpKeepalive {
    TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(10))
        .with_retries(3)
}

/// How long sent data may stay unacknowledged before the kernel closes the connection (Linux
/// `TCP_USER_TIMEOUT`). Without it a connection that lost its peer to a partition keeps
/// retransmitting with exponential backoff for `tcp_retries2` rounds (15 minutes by default
/// on a quiet link, far longer than a Raft election), so a healed follower only recovers when
/// the next retransmission happens to get through. Measured on a three-node cluster with a
/// follower cut off for 12 to 60 seconds: the first write through it succeeded 15 to 47
/// seconds after the heal without this, 5 to 7 seconds with it. 10 seconds is above the
/// round-trip times and loss that cross-site links see, and did not break connections at
/// 100 ms RTT with 5% packet loss.
#[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
const USER_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn create_listening_socket(addr: SocketAddr) -> Result<TcpListener, Error> {
    task::spawn_blocking(move || {
        let socket = if addr.is_ipv4() {
            Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )?
        } else {
            Socket::new(
                socket2::Domain::IPV6,
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )?
        };
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.set_tcp_nodelay(true)?;

        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        {
            socket.set_tcp_cork(false)?;
            socket.set_priority(6)?;
            socket.set_tcp_user_timeout(Some(USER_TIMEOUT))?;
        }

        let keepalive = standard_keepalive();
        socket.set_tcp_keepalive(&keepalive)?;
        socket.set_keepalive(true)?;

        socket.bind(&addr.into())?;
        socket.listen(1024)?;
        Ok(TcpListener::from_std(socket.into())?)
    })
    .await?
}

pub fn configure_tcp_stream(stream: &mut tokio::net::TcpStream) {
    let socket_ref = socket2::SockRef::from(&*stream);

    if let Err(err) = stream.set_nodelay(true) {
        debug!("Failed to set TCP_NODELAY on TCP stream: {err}");
    }
    if let Err(err) = socket_ref.set_tcp_keepalive(&standard_keepalive()) {
        debug!("Failed to configure TCP keepalive on TCP stream: {err}");
    }
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    if let Err(err) = socket_ref.set_priority(6) {
        debug!("Failed to set socket priority on TCP stream: {err}");
    }
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    if let Err(err) = socket_ref.set_tcp_user_timeout(Some(USER_TIMEOUT)) {
        debug!("Failed to set TCP_USER_TIMEOUT on TCP stream: {err}");
    }
}

#[derive(Clone, Copy)]
pub struct ConfiguredStreamAcceptor;

impl<S> Accept<tokio::net::TcpStream, S> for ConfiguredStreamAcceptor {
    type Stream = tokio::net::TcpStream;
    type Service = S;
    type Future = Ready<io::Result<(tokio::net::TcpStream, S)>>;

    fn accept(&self, stream: tokio::net::TcpStream, service: S) -> Self::Future {
        let mut stream = stream;
        configure_tcp_stream(&mut stream);
        std::future::ready(Ok((stream, service)))
    }
}

#[cfg(all(test, any(target_os = "android", target_os = "fuchsia", target_os = "linux")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn streams_get_the_user_timeout() -> Result<(), Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (connected, accepted) = tokio::join!(tokio::net::TcpStream::connect(addr), listener.accept());
        let mut client = connected?;
        let (mut server, _) = accepted?;

        configure_tcp_stream(&mut client);
        configure_tcp_stream(&mut server);

        assert_eq!(socket2::SockRef::from(&client).tcp_user_timeout()?, Some(USER_TIMEOUT));
        assert_eq!(socket2::SockRef::from(&server).tcp_user_timeout()?, Some(USER_TIMEOUT));
        Ok(())
    }

    #[tokio::test]
    async fn listening_socket_has_the_user_timeout() -> Result<(), Box<dyn std::error::Error>> {
        let listener = create_listening_socket("127.0.0.1:0".parse()?).await?;
        assert_eq!(socket2::SockRef::from(&listener).tcp_user_timeout()?, Some(USER_TIMEOUT));
        Ok(())
    }
}
