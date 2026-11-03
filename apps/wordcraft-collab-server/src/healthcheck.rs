//! The container healthcheck: `wordcraft-collab-server healthcheck` asks the running server's
//! `/api/v1/health/live` over plain HTTP/1.1, so the image needs no curl.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(3);

/// `Ok` if the server at `listen_addr` answers 200 on the liveness endpoint.
pub fn probe(listen_addr: SocketAddr) -> std::io::Result<()> {
    match status(listen_addr, "/api/v1/health/live")? {
        200 => Ok(()),
        code => Err(std::io::Error::other(format!("liveness answered HTTP {code}"))),
    }
}

/// The HTTP status of `GET path`. A wildcard address (`0.0.0.0`) is probed on loopback.
pub fn status(listen_addr: SocketAddr, path: &str) -> std::io::Result<u16> {
    let mut addr = listen_addr;
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes())?;
    let mut head = [0u8; 32];
    let n = stream.read(&mut head)?;
    let line = String::from_utf8_lossy(head.get(..n).unwrap_or_default()).into_owned();
    // "HTTP/1.1 200 OK"
    line.strip_prefix("HTTP/1.")
        .and_then(|rest| rest.get(2..5))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("not an HTTP response: {:?}", line.lines().next().unwrap_or_default())))
}
