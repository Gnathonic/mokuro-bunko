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

/// Headers a proxy adds when it forwards a request.
const FORWARDED: [&str; 4] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-real-ip",
];

/// A passwordless account may sign in on this request (module docs).
pub fn passwordless_sign_in(core: &Core, client: &Client, headers: &HeaderMap) -> bool {
    local_only(core)
        && is_local(client)
        && host_header_is_local(headers)
        && !FORWARDED.iter().any(|h| headers.contains_key(*h))
}

/// A browser on this computer, as the folder browser needs it (it lists this disk):
/// a local connection ([`is_local`]) that no proxy forwarded, and whose `Host`, when it
/// names one, is this machine. A DNS-rebinding page reaches the server over loopback
/// too, but names its own host.
pub fn browser_on_this_machine(client: &Client, headers: &HeaderMap) -> bool {
    is_local(client)
        && (!headers.contains_key(header::HOST) || host_header_is_local(headers))
        && !FORWARDED.iter().any(|h| headers.contains_key(*h))
}

/// [`browser_on_this_machine`] from a request's parts: its connection's address (none
/// known: not local) and the client address the server resolved for it.
pub fn request_on_this_machine(
    peer: Option<std::net::IpAddr>,
    client_ip: &str,
    headers: &HeaderMap,
) -> bool {
    let Some(peer) = peer else {
        return false;
    };
    let client = Client {
        peer,
        peer_known: true,
        ip: client_ip.to_string(),
    };
    browser_on_this_machine(&client, headers)
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

    #[test]
    fn the_folder_browser_wants_a_browser_here() {
        let here = Client {
            peer: "127.0.0.1".parse().unwrap(),
            peer_known: true,
            ip: "127.0.0.1".into(),
        };
        let with = |pairs: &[(&'static str, &str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, v.parse().unwrap());
            }
            h
        };
        assert!(browser_on_this_machine(&here, &HeaderMap::new()));
        assert!(browser_on_this_machine(
            &here,
            &with(&[("host", "localhost:18100")])
        ));
        assert!(!browser_on_this_machine(
            &here,
            &with(&[("host", "rebind.example.com")])
        ));
        assert!(!browser_on_this_machine(
            &here,
            &with(&[("host", "localhost"), ("x-real-ip", "203.0.113.4")])
        ));
        let far = Client {
            peer: "203.0.113.4".parse().unwrap(),
            peer_known: true,
            ip: "203.0.113.4".into(),
        };
        assert!(!browser_on_this_machine(&far, &HeaderMap::new()));
    }
}
