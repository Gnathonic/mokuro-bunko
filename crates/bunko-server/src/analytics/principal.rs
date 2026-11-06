//! Who a request is attributed to (design §3): `u:<username>`, `p:<username>` for a
//! processor account, or `a:<12 hex>` for an anonymous request, the start of
//! `sha256(salt || client network)`, so anonymous visitors can be told apart without
//! long-lived rows keeping raw IPs. The network is the IPv4 address, or the /64 of an
//! IPv6 one (one subscriber's prefix: any client there can make every request a new
//! address). The salt is replaced every `retention.ip_days`
//! ([`bunko_db::analytics::AnalyticsDb::salt`]), so an id outliving the raw IPs cannot
//! be traced back to them.

use bunko_core::Role;
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrincipalKind {
    User,
    Anon,
    Processor,
}

impl PrincipalKind {
    /// The API's `kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            PrincipalKind::User => "user",
            PrincipalKind::Anon => "anon",
            PrincipalKind::Processor => "processor",
        }
    }

    /// The kind of a stored principal id.
    pub fn of(principal: &str) -> PrincipalKind {
        match principal.get(..2) {
            Some("p:") => PrincipalKind::Processor,
            Some("a:") => PrincipalKind::Anon,
            _ => PrincipalKind::User,
        }
    }
}

/// A request's principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub id: Arc<str>,
    pub kind: PrincipalKind,
    /// The account (users and processors).
    pub username: Option<Arc<str>>,
}

/// Most characters of a client address kept where one is stored (a trusted proxy may
/// send any text).
pub const IP_TEXT_MAX: usize = 64;

/// The client address as stored: an IP in canonical form (IPv4-mapped IPv6 folded),
/// any other text as-is, cut to [`IP_TEXT_MAX`] characters.
pub fn canonical_ip_text(text: &str) -> String {
    let t = text.trim();
    match t.parse::<IpAddr>() {
        Ok(ip) => crate::http::client_ip::canonical(ip).to_string(),
        Err(_) => t.chars().take(IP_TEXT_MAX).collect(),
    }
}

/// Most characters of an attempted username kept for a refused sign-in (accounts are
/// 3 to 32 bytes; a refused credential may name anything, of any length).
pub const USERNAME_TEXT_MAX: usize = 64;

/// A refused sign-in's attempted username as stored: cut to [`USERNAME_TEXT_MAX`]
/// characters.
pub fn attempted_username_text(name: &str) -> String {
    name.chars().take(USERNAME_TEXT_MAX).collect()
}

/// What an anonymous id is keyed by: an IPv4 address, the `/64` of an IPv6 one, or a
/// trusted proxy's other text as-is.
pub fn anon_network(ip: &str) -> std::borrow::Cow<'_, str> {
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) if v6.to_ipv4_mapped().is_none() => {
            let net = std::net::Ipv6Addr::from(v6.to_bits() & !u128::from(u64::MAX));
            format!("{net}/64").into()
        }
        _ => ip.into(),
    }
}

/// `a:` + the first 12 hex digits of `sha256(salt || network)` (`ip` canonical; see
/// [`anon_network`]).
pub fn anon_id(salt: &[u8; 32], ip: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(anon_network(ip).as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(14);
    out.push_str("a:");
    out.push_str(&hex::encode(&digest[..6]));
    out
}

/// The principal of a request: its account when it had one, else its address.
pub fn principal_of(
    username: Option<&str>,
    role: Role,
    ip_canonical: &str,
    salt: &[u8; 32],
) -> Principal {
    match username {
        Some(u) if role == Role::Processor => Principal {
            id: format!("p:{u}").into(),
            kind: PrincipalKind::Processor,
            username: Some(u.into()),
        },
        Some(u) => Principal {
            id: format!("u:{u}").into(),
            kind: PrincipalKind::User,
            username: Some(u.into()),
        },
        None => Principal {
            id: anon_id(salt, ip_canonical).into(),
            kind: PrincipalKind::Anon,
            username: None,
        },
    }
}

/// The API's `label` of a principal id: the account name, or `Anonymous <hash>`.
pub fn label(principal: &str) -> String {
    if principal == bunko_db::analytics::traffic::ANON_OTHER {
        return "Other anonymous visitors".to_string();
    }
    if principal == bunko_db::analytics::traffic::ANON_GROUP {
        return "Anonymous visitors".to_string();
    }
    match PrincipalKind::of(principal) {
        PrincipalKind::Anon => format!("Anonymous {}", principal.get(2..).unwrap_or("")),
        _ => principal.get(2..).unwrap_or(principal).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_labels() {
        let salt = [7u8; 32];
        let a = principal_of(None, Role::Anonymous, "203.0.113.5", &salt);
        assert_eq!(a.kind, PrincipalKind::Anon);
        assert_eq!(a.id.len(), 14);
        assert!(a.id.starts_with("a:"));
        assert!(a.id[2..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            a.id,
            principal_of(None, Role::Anonymous, "203.0.113.5", &salt).id
        );
        assert_ne!(
            a.id,
            principal_of(None, Role::Anonymous, "203.0.113.6", &salt).id
        );
        assert_ne!(
            a.id,
            principal_of(None, Role::Anonymous, "203.0.113.5", &[8u8; 32]).id
        );
        let u = principal_of(Some("alice"), Role::Registered, "203.0.113.5", &salt);
        assert_eq!(&*u.id, "u:alice");
        let p = principal_of(Some("ocr"), Role::Processor, "203.0.113.5", &salt);
        assert_eq!((&*p.id, p.kind), ("p:ocr", PrincipalKind::Processor));
        assert_eq!(label("u:alice"), "alice");
        assert_eq!(label(&a.id), format!("Anonymous {}", &a.id[2..]));
        assert_eq!(PrincipalKind::of("p:ocr"), PrincipalKind::Processor);
        assert_eq!(canonical_ip_text("::ffff:192.0.2.1"), "192.0.2.1");
        assert_eq!(canonical_ip_text(" proxy-name "), "proxy-name");
        assert_eq!(canonical_ip_text(&"x".repeat(100)).len(), IP_TEXT_MAX);
        assert_eq!(
            attempted_username_text(&"é".repeat(1000)).chars().count(),
            USERNAME_TEXT_MAX
        );
        assert_eq!(attempted_username_text("alice"), "alice");
    }

    /// Every address of one IPv6 /64 is one visitor; IPv4 addresses stay apart.
    #[test]
    fn an_ipv6_slash_64_is_one_anonymous_visitor() {
        let salt = [3u8; 32];
        let id = |ip: &str| anon_id(&salt, ip);
        assert_eq!(
            id("2001:db8:aa:1::1"),
            id("2001:db8:aa:1:ffff:1234:5678:9abc")
        );
        assert_ne!(id("2001:db8:aa:1::1"), id("2001:db8:aa:2::1"));
        assert_ne!(id("192.0.2.1"), id("192.0.2.2"));
        assert_eq!(anon_network("2001:db8:aa:1:2:3:4:5"), "2001:db8:aa:1::/64");
        assert_eq!(anon_network("192.0.2.1"), "192.0.2.1");
        assert_eq!(anon_network("proxy-name"), "proxy-name");
        assert_eq!(
            label(bunko_db::analytics::traffic::ANON_OTHER),
            "Other anonymous visitors"
        );
    }
}
