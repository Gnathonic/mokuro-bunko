//! Who a request is attributed to (design §3): `u:<username>`, `p:<username>` for a
//! processor account, or `a:<12 hex>` for an anonymous request, the start of
//! `sha256(salt || canonical client IP)`, so anonymous visitors can be told apart
//! without long-lived rows keeping raw IPs.

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

/// `a:` + the first 12 hex digits of `sha256(salt || ip)` (`ip` canonical).
pub fn anon_id(salt: &[u8; 32], ip: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(ip.as_bytes());
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
    }
}
