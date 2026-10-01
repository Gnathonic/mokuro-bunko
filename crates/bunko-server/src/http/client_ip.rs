//! Client IP resolution behind trusted reverse proxies (0.5.2 `security.py`).
//!
//! Loopback peers are always trusted (the bundled nginx), plus `server.trusted_proxies`.
//! A trusted peer's `X-Real-IP` wins, else the rightmost `X-Forwarded-For` entry.

use http::HeaderMap;
use ipnet::IpNet;
use std::net::IpAddr;

#[derive(Debug, Clone, Default)]
pub struct TrustedProxies {
    nets: Vec<IpNet>,
}

impl TrustedProxies {
    pub fn new(list: &[String]) -> Self {
        let nets = list
            .iter()
            .filter_map(|s| {
                s.parse::<IpNet>()
                    .ok()
                    .or_else(|| s.parse::<IpAddr>().ok().map(IpNet::from))
            })
            .collect();
        Self { nets }
    }

    pub fn is_proxy_peer(&self, peer: IpAddr) -> bool {
        let peer = canonical(peer);
        peer.is_loopback() || self.nets.iter().any(|n| n.contains(&peer))
    }

    pub fn client_ip(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        if !self.is_proxy_peer(peer) {
            return canonical(peer);
        }
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        if let Some(ip) = header("x-real-ip").and_then(|s| s.parse::<IpAddr>().ok()) {
            return canonical(ip);
        }
        if let Some(xff) = header("x-forwarded-for")
            && let Some(ip) = xff
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .next_back()
                .and_then(|s| s.parse::<IpAddr>().ok())
        {
            return canonical(ip);
        }
        canonical(peer)
    }

    /// The client address as text, for rate-limit keys and logs. Falls back to the raw
    /// header text when a trusted proxy sends something that is not an IP (0.5.2 kept it).
    pub fn client_ip_text(&self, peer: IpAddr, headers: &HeaderMap) -> String {
        if self.is_proxy_peer(peer) {
            let header = |name: &str| {
                headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            };
            if let Some(real) = header("x-real-ip") {
                return real.to_string();
            }
            if let Some(xff) = header("x-forwarded-for")
                && let Some(last) = xff
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .next_back()
            {
                return last.to_string();
            }
        }
        canonical(peer).to_string()
    }
}

/// `::ffff:a.b.c.d` → `a.b.c.d` so dual-stack sockets compare like IPv4.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

pub fn is_loopback(text: &str) -> bool {
    text.trim()
        .parse::<IpAddr>()
        .map(|ip| canonical(ip).is_loopback())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        m
    }

    #[test]
    fn untrusted_peer_ignores_headers() {
        let t = TrustedProxies::new(&[]);
        let peer: IpAddr = "203.0.113.5".parse().unwrap();
        assert_eq!(t.client_ip(peer, &h(&[("x-real-ip", "1.2.3.4")])), peer);
    }

    #[test]
    fn loopback_peer_trusted() {
        let t = TrustedProxies::new(&[]);
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(
            t.client_ip(peer, &h(&[("x-real-ip", "1.2.3.4")]))
                .to_string(),
            "1.2.3.4"
        );
        assert_eq!(
            t.client_ip(peer, &h(&[("x-forwarded-for", "9.9.9.9, 5.6.7.8")]))
                .to_string(),
            "5.6.7.8"
        );
        assert_eq!(
            t.client_ip("::ffff:127.0.0.1".parse().unwrap(), &h(&[]))
                .to_string(),
            "127.0.0.1"
        );
    }

    #[test]
    fn configured_network() {
        let t = TrustedProxies::new(&["10.0.0.0/8".into()]);
        assert!(t.is_proxy_peer("10.1.2.3".parse().unwrap()));
        assert!(!t.is_proxy_peer("192.168.1.1".parse().unwrap()));
    }
}
