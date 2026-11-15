//! wire module (split from lib.rs in alpha.29.17.0).

use super::*;

/// Transport for one Electrum client. Plain TCP and TLS share one read/write
/// path so the notifier can write to a TLS connection without splitting
/// rustls' connection state across threads.
pub enum Wire {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ServerConnection, TcpStream>>),
}

impl Wire {
    pub(crate) fn plain(stream: TcpStream) -> std::io::Result<Self> {
        configure_socket(&stream)?;
        Ok(Wire::Plain(stream))
    }

    pub(crate) fn tls(config: Arc<ServerConfig>, stream: TcpStream) -> std::io::Result<Self> {
        configure_socket(&stream)?;
        let conn = ServerConnection::new(config)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        Ok(Wire::Tls(Box::new(StreamOwned::new(conn, stream))))
    }

    pub fn is_tls(&self) -> bool {
        matches!(self, Wire::Tls(_))
    }

    pub(crate) fn socket(&self) -> &TcpStream {
        match self {
            Wire::Plain(s) => s,
            Wire::Tls(s) => &s.sock,
        }
    }

    /// Complete the TLS handshake with a bounded deadline. Plain connections
    /// return immediately.
    pub(crate) fn handshake(&mut self) -> std::io::Result<()> {
        let Wire::Tls(stream) = self else {
            return Ok(());
        };
        let deadline = Instant::now() + TLS_HANDSHAKE_TIMEOUT;
        while stream.conn.is_handshaking() {
            match stream.conn.complete_io(&mut stream.sock) {
                Ok((0, 0)) => std::thread::sleep(Duration::from_millis(5)),
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "TLS handshake timeout",
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn configure_socket(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(CLIENT_READ_POLL))?;
    stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))?;
    let _ = stream.set_nodelay(true);
    Ok(())
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(s) => s.read(buf),
            Wire::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(s) => s.write(buf),
            Wire::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Wire::Plain(s) => s.flush(),
            Wire::Tls(s) => s.flush(),
        }
    }
}
