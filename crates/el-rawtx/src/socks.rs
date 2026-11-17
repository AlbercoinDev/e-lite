//! Minimal SOCKS5 client with a timeout on every step, shared by the P2P and
//! Electrum sources (both reach peers through Tor).

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub(crate) fn split_host_port(peer: &str) -> io::Result<(&str, u16)> {
    if let Some(rest) = peer.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad IPv6 peer"))?;
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad peer port"))?;
        return Ok((host, port));
    }
    let (host, port) = peer
        .rsplit_once(':')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad peer address"))?;
    let port = port
        .parse::<u16>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad peer port"))?;
    Ok((host, port))
}

/// Why a SOCKS5 CONNECT failed. Only `Proxy` means the local Tor is unusable;
/// a reply code or a CONNECT that never completes is about the peer or the
/// circuit, and must not stop P2P for every other peer (AUD-153).
#[derive(Debug)]
pub(crate) enum SocksError {
    /// TCP to the proxy, its greeting or its authentication failed.
    Proxy(io::Error),
    /// The proxy answered the CONNECT with a non-zero reply code.
    Reply(u8),
    /// The CONNECT was sent but its reply did not complete (slow circuit).
    Stream(io::Error),
}

impl fmt::Display for SocksError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SocksError::Proxy(e) => write!(f, "socks proxy: {e}"),
            SocksError::Reply(code) => {
                write!(f, "socks reply {code} ({})", socks_reply_name(*code))
            }
            SocksError::Stream(e) => write!(f, "socks connect: {e}"),
        }
    }
}

fn socks_reply_name(code: u8) -> &'static str {
    match code {
        1 => "general failure",
        2 => "not allowed by ruleset",
        3 => "network unreachable",
        4 => "host unreachable",
        5 => "connection refused",
        6 => "TTL expired",
        7 => "command not supported",
        8 => "address type not supported",
        _ => "unknown",
    }
}

/// Minimal SOCKS5 CONNECT with bounded connect/read/write timeouts.
///
/// The `socks` crate does not expose timeouts, so a stalled Tor SOCKS proxy
/// could block a P2P fetch indefinitely. This performs the same handshake with
/// the P2P timeout applied to every step.
pub(crate) fn socks_connect(
    proxy: &str,
    peer: &str,
    timeout: Duration,
) -> std::result::Result<TcpStream, SocksError> {
    let mut stream = socks_greet(proxy, timeout).map_err(SocksError::Proxy)?;
    socks_request(&mut stream, peer)?;
    Ok(stream)
}

/// Connect to the proxy and complete the method negotiation (with a random
/// username: Tor isolates streams with different credentials).
fn socks_greet(proxy: &str, timeout: Duration) -> io::Result<TcpStream> {
    let proxy_addrs: Vec<_> = proxy.to_socket_addrs()?.collect();
    let mut stream = None;
    for addr in proxy_addrs {
        if let Ok(sock) = TcpStream::connect_timeout(&addr, timeout) {
            stream = Some(sock);
            break;
        }
    }
    let mut stream = stream.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("socks proxy {proxy} did not accept a connection"),
        )
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    // Offer no-auth and username/password (Tor uses the username for circuit
    // isolation).
    stream.write_all(&[5, 2, 0, 2])?;
    let mut resp = [0u8; 2];
    stream.read_exact(&mut resp)?;
    if resp[0] != 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad SOCKS version",
        ));
    }
    match resp[1] {
        0 => {}
        2 => {
            let user = format!("e-lite-{}", rand::random::<u64>());
            let user = user.as_bytes();
            let pass = b"e-lite";
            let mut auth = Vec::with_capacity(3 + user.len() + pass.len());
            auth.push(1);
            auth.push(user.len() as u8);
            auth.extend_from_slice(user);
            auth.push(pass.len() as u8);
            auth.extend_from_slice(pass);
            stream.write_all(&auth)?;
            let mut status = [0u8; 2];
            stream.read_exact(&mut status)?;
            if status[0] != 1 || status[1] != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "SOCKS authentication failed",
                ));
            }
        }
        0xff => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SOCKS proxy rejected all auth methods",
            ))
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported SOCKS auth method",
            ))
        }
    }
    Ok(stream)
}

/// Send the CONNECT for `peer` and read the reply.
fn socks_request(stream: &mut TcpStream, peer: &str) -> std::result::Result<(), SocksError> {
    let (host, port) = split_host_port(peer).map_err(SocksError::Stream)?;
    let mut request = vec![5, 1, 0];
    if let Ok(ip) = host.parse::<IpAddr>() {
        match ip {
            IpAddr::V4(v4) => {
                request.push(1);
                request.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                request.push(4);
                request.extend_from_slice(&v6.octets());
            }
        }
    } else {
        let host = host.as_bytes();
        if host.len() > 255 {
            return Err(SocksError::Stream(io::Error::new(
                io::ErrorKind::InvalidInput,
                "peer host too long",
            )));
        }
        request.push(3);
        request.push(host.len() as u8);
        request.extend_from_slice(host);
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).map_err(SocksError::Proxy)?;

    // From here on Tor is building a circuit to the peer: a timeout is the
    // circuit's (or the peer's), not the proxy's.
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).map_err(SocksError::Stream)?;
    if head[0] != 5 {
        return Err(SocksError::Proxy(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad SOCKS reply version",
        )));
    }
    if head[1] != 0 {
        return Err(SocksError::Reply(head[1]));
    }
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).map_err(SocksError::Stream)?;
            len[0] as usize
        }
        _ => {
            return Err(SocksError::Proxy(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad SOCKS reply address type",
            )))
        }
    };
    // Bound address and port.
    let mut rest = vec![0u8; skip + 2];
    stream.read_exact(&mut rest).map_err(SocksError::Stream)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Instant;

    #[test]
    fn socks_connect_is_bounded_by_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 16];
                let _ = sock.read(&mut buf);
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        let started = Instant::now();
        let result = socks_connect(&addr, "127.0.0.1:8333", Duration::from_millis(200));
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "SOCKS handshake must not block past the timeout"
        );
    }

    #[test]
    fn socks_connect_completes_password_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut greeting = [0u8; 4];
            sock.read_exact(&mut greeting).unwrap();
            sock.write_all(&[5, 2]).unwrap();
            let mut auth_head = [0u8; 2];
            sock.read_exact(&mut auth_head).unwrap();
            let mut user = vec![0u8; auth_head[1] as usize];
            sock.read_exact(&mut user).unwrap();
            let mut plen = [0u8; 1];
            sock.read_exact(&mut plen).unwrap();
            let mut pass = vec![0u8; plen[0] as usize];
            sock.read_exact(&mut pass).unwrap();
            sock.write_all(&[1, 0]).unwrap();
            // CONNECT 127.0.0.1:8333 is 4 + 4 + 2 bytes.
            let mut request = [0u8; 10];
            sock.read_exact(&mut request).unwrap();
            sock.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).unwrap();
            std::thread::sleep(Duration::from_secs(1));
        });
        let stream = socks_connect(&addr, "127.0.0.1:8333", Duration::from_secs(2)).unwrap();
        assert_eq!(
            stream.peer_addr().unwrap().ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
    }
}
