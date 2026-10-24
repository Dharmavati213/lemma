//! The MCP socket's transport ([`crate::mcp`]): a unix socket, or on Windows, whose standard
//! library has none, a loopback TCP port.
//!
//! On Windows the socket path names a small file instead, holding the port's address
//! (`127.0.0.1:<port>`) and nothing else; it lives where the unix socket would (the user's own
//! runtime directory) and is written when the port is bound. `lemma-mcp` reads it the same way.
//! The token check on the first line is unchanged, so a local program that finds the port still
//! cannot use it.

use std::io;
use std::path::Path;

#[cfg(unix)]
pub(crate) use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};

#[cfg(windows)]
pub(crate) use std::net::{TcpListener as Listener, TcpStream as Stream};

/// Connects to the server at `socket`.
pub(crate) fn connect(socket: &Path) -> io::Result<Stream> {
    #[cfg(unix)]
    {
        Stream::connect(socket)
    }
    #[cfg(windows)]
    {
        Stream::connect_timeout(&windows::read_address(socket)?, windows::CONNECT_TIMEOUT)
    }
}

#[cfg(windows)]
pub(crate) use windows::{bind_new, read_address};

#[cfg(windows)]
mod windows {
    use std::fs;
    use std::io::{self, Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};
    use std::path::Path;
    use std::time::Duration;

    /// How long a connection to the loopback port may take (it is refused at once when nothing
    /// listens).
    pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
    /// Longest port file read (`127.0.0.1:65535` is 15 bytes).
    const MAX_ADDRESS_BYTES: u64 = 64;

    /// Binds a loopback port and writes its address to `socket`, which must not exist yet
    /// (`AlreadyExists` otherwise).
    pub(crate) fn bind_new(socket: &Path) -> io::Result<TcpListener> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let address = listener.local_addr()?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(socket)?;
        if let Err(e) = file.write_all(address.to_string().as_bytes()) {
            drop(file);
            let _ = fs::remove_file(socket);
            return Err(e);
        }
        Ok(listener)
    }

    /// The loopback address in the port file `socket` (`InvalidData` when it holds anything
    /// else).
    pub(crate) fn read_address(socket: &Path) -> io::Result<SocketAddr> {
        let mut text = String::new();
        fs::File::open(socket)?
            .take(MAX_ADDRESS_BYTES)
            .read_to_string(&mut text)
            .map_err(|_| not_a_port_file(socket))?;
        match text.trim().parse::<SocketAddr>() {
            Ok(address) if address.ip().is_loopback() => Ok(address),
            _ => Err(not_a_port_file(socket)),
        }
    }

    fn not_a_port_file(socket: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} does not name an MCP port", socket.display()),
        )
    }
}

/// Two connected streams, for tests.
#[cfg(test)]
pub(crate) fn pair() -> io::Result<(Stream, Stream)> {
    #[cfg(unix)]
    {
        Stream::pair()
    }
    #[cfg(windows)]
    {
        let listener = Listener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let client = Stream::connect(listener.local_addr()?)?;
        let (server, _) = listener.accept()?;
        Ok((server, client))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_port_file_names_the_listener() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mcp.sock");
        let listener = bind_new(&socket).unwrap();
        assert_eq!(
            read_address(&socket).unwrap(),
            listener.local_addr().unwrap()
        );
        let _client = connect(&socket).unwrap();
        assert_eq!(
            bind_new(&socket).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        std::fs::write(&socket, "example.com:80").unwrap();
        assert_eq!(
            read_address(&socket).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
