//! A minimal HTTP/1.1 client for loopback timings: one `Connection: close` request per
//! call, timed from connect to the last byte, with nothing in between that could add
//! its own latency (no pool, no TLS, no async runtime).

use anyhow::{Context, Result, bail};
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub elapsed: Duration,
}

impl Reply {
    pub fn ms(&self) -> f64 {
        self.elapsed.as_secs_f64() * 1000.0
    }

    pub fn json(&self) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.body).context("response is not JSON")
    }
}

pub fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<Reply> {
    let start = Instant::now();
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_nodelay(true)?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let elapsed = start.elapsed();
    let (status, body) = parse(&raw)?;
    Ok(Reply {
        status,
        body,
        elapsed,
    })
}

pub fn get(addr: SocketAddr, path: &str) -> Result<Reply> {
    request(addr, "GET", path, &[], &[])
}

fn parse(raw: &[u8]) -> Result<(u16, Vec<u8>)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("no end of headers in the response")?;
    let head = std::str::from_utf8(&raw[..split]).context("headers are not UTF-8")?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .context("bad status line")?;
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = &raw[split + 4..];
    Ok((
        status,
        if chunked {
            dechunk(body)?
        } else {
            body.to_vec()
        },
    ))
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .context("truncated chunk size")?;
        let size_text = std::str::from_utf8(&body[..eol])?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .context("bad chunk size")?;
        body = &body[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            bail!("truncated chunk");
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn serve_once(response: &'static [u8]) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            stream.write_all(response).unwrap();
        });
        addr
    }

    #[test]
    fn reads_status_and_a_plain_body() {
        let addr = serve_once(b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\nno");
        let reply = get(addr, "/x").unwrap();
        assert_eq!(reply.status, 404);
        assert_eq!(reply.body, b"no");
    }

    #[test]
    fn reads_a_chunked_body() {
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n4;x=y\r\n\":1}\r\n0\r\n\r\n",
        );
        let reply = get(addr, "/").unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.json().unwrap(), serde_json::json!({"a": 1}));
    }
}
