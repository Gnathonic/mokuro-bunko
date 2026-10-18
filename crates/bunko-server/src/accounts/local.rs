//! A server reachable from this computer only, and the passwordless admin it may have.
//!
//! The setup wizard lets the owner of such a server skip the admin password. That
//! account has no password hash, so no password (Basic auth included) ever matches
//! it; it signs in with an empty password, and only when every check here holds:
//! the server listens on a loopback address, the connection comes from this machine,
//! the request names this machine as its host (a DNS-rebinding page names its own)
//! and no proxy forwarded it.

use http::{HeaderMap, header};

use super::util::Client;
use crate::core::Core;

/// `server.host` is a loopback address: nothing off this machine can connect.
pub fn loopback_host(host: &str) -> bool {
    let host = host.trim();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// This server listens on this computer only.
pub fn local_only(core: &Core) -> bool {
    loopback_host(&core.config.read().server.host)
}

/// 0.5.2 `_is_local_request`: loopback peer, and a loopback client address if proxy
/// headers name a different one. An unknown peer (no `ConnectInfo`) is not local.
pub fn is_local(client: &Client) -> bool {
    if !client.peer_known || !client.peer.is_loopback() {
        return false;
    }
    client.ip == client.peer.to_string() || crate::http::client_ip::is_loopback(&client.ip)
}

/// The `Host` header names this machine (`localhost`, `127.0.0.1`, `[::1]`, any port).
fn host_header_is_local(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let name = match host.rsplit_once(':') {
        // `[::1]:8080` or `host:8080`; a bare IPv6 address has more than one colon.
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) && !name.ends_with(':') => {
            name
        }
        _ => host,
    };
    loopback_host(name)
}

/// A passwordless account may sign in on this request (module docs).
pub fn passwordless_sign_in(core: &Core, client: &Client, headers: &HeaderMap) -> bool {
    const FORWARDED: [&str; 4] = [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-real-ip",
    ];
    local_only(core)
        && is_local(client)
        && host_header_is_local(headers)
        && !FORWARDED.iter().any(|h| headers.contains_key(*h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        for h in [
            "127.0.0.1",
            "::1",
            "[::1]",
            "localhost",
            "LOCALHOST",
            "127.0.0.2",
        ] {
            assert!(loopback_host(h), "{h}");
        }
        for h in [
            "0.0.0.0",
            "::",
            "192.168.1.2",
            "example.com",
            "",
            "localhost.example",
        ] {
            assert!(!loopback_host(h), "{h}");
        }
    }

    #[test]
    fn host_headers() {
        let of = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::HOST, v.parse().unwrap());
            host_header_is_local(&h)
        };
        assert!(of("127.0.0.1:18100"));
        assert!(of("localhost:18100"));
        assert!(of("[::1]:18100"));
        assert!(of("localhost"));
        assert!(!of("evil.example:18100"));
        assert!(!of("127.0.0.1.nip.io:18100"));
        assert!(!host_header_is_local(&HeaderMap::new()));
    }
}
