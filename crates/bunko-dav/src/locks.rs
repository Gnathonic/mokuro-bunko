//! In-memory WebDAV lock table (class 2, WsgiDAV `LockManager` semantics, spec §8.3.9).
//!
//! Enough for Finder / Windows Explorer / office clients, nothing persisted. Fixes over
//! 0.5.2 (spec 14.7): the per-user progress files are locked per user (their key carries
//! the username, so one reader's lock never blocks another's save), the principal is the
//! authenticated username (so only the lock's owner may UNLOCK it), and LOCK on an
//! unmapped URL is a lock-null resource answered `201` instead of a 500 with a dangling
//! lock.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rand::RngCore;

pub const DEFAULT_TIMEOUT_SECS: i64 = 604_800;
/// Above ~10 years a timeout counts as infinite (WsgiDAV `MAX_FINITE_TIMEOUT_LIMIT`).
const MAX_FINITE_TIMEOUT: i64 = 10 * 365 * 24 * 3600;
/// Bound on live locks (memory); a LOCK beyond it answers 507.
pub const MAX_LOCKS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Exclusive,
    Shared,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Exclusive => "exclusive",
            Scope::Shared => "shared",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Lock {
    pub token: String,
    /// Lock key (normalised path; per-user files carry `\0<username>`).
    pub key: String,
    /// The href reported as `lockroot`.
    pub root_href: String,
    pub scope: Scope,
    pub infinite_depth: bool,
    /// The `<owner>` element, serialised (empty when none was given).
    pub owner_xml: String,
    /// Seconds, or negative for infinite.
    pub timeout: i64,
    pub expires: Option<Instant>,
    pub principal: String,
}

impl Lock {
    /// `Second-<remaining>` or `Infinite`.
    pub fn timeout_text(&self) -> String {
        match self.expires {
            None => "Infinite".to_string(),
            Some(at) => format!(
                "Second-{}",
                at.saturating_duration_since(Instant::now()).as_secs()
            ),
        }
    }

    fn expired(&self, now: Instant) -> bool {
        self.expires.is_some_and(|at| at <= now)
    }
}

/// `Timeout:` header -> seconds (negative = infinite); default one week.
pub fn parse_timeout(value: Option<&str>) -> i64 {
    let Some(value) = value else {
        return DEFAULT_TIMEOUT_SECS;
    };
    for part in value.split(',') {
        let part = part.trim();
        if part.eq_ignore_ascii_case("infinite") {
            return -1;
        }
        if let Some(n) = part
            .strip_prefix("Second-")
            .or_else(|| part.strip_prefix("second-"))
            && let Ok(n) = n.trim().parse::<i64>()
        {
            return if n > MAX_FINITE_TIMEOUT { -1 } else { n };
        }
    }
    DEFAULT_TIMEOUT_SECS
}

#[cfg(test)]
fn parent_key(key: &str) -> Option<&str> {
    if key == "/" {
        return None;
    }
    let base = key.split('\0').next().unwrap_or(key);
    let cut = base.rfind('/')?;
    Some(if cut == 0 { "/" } else { &base[..cut] })
}

fn is_child(parent: &str, child: &str) -> bool {
    if parent == "/" {
        return child != "/";
    }
    child.starts_with(parent) && child[parent.len()..].starts_with(['/', '\0'])
}

/// The account a per-user key (`<path>\0<account>`) belongs to, `None` for a shared key.
fn private_owner(key: &str) -> Option<&str> {
    key.split_once('\0').map(|(_, owner)| owner)
}

/// A lock on another account's own copy of a per-user path (`<path>\0<other>`). It is
/// private to that account: an operation on the shared tree above it never conflicts
/// with it (nor names it), it only goes with the tree (`remove_tree`).
fn private_to_other(lock: &Lock, principal: &str) -> bool {
    private_owner(&lock.key).is_some_and(|owner| owner != principal)
}

/// The other direction: a depth-infinity lock held by anyone but the owner of the
/// per-user `key` below it does not cover that key. The owner's own copy is outside
/// every other principal's lock scope, so a folder lock never blocks the owner's LOCK or
/// write of it, and the owner's own exclusive lock on it keeps guaranteeing its writes.
fn foreign_to_key(lock: &Lock, key: &str) -> bool {
    private_owner(key).is_some_and(|owner| owner != lock.principal)
}

#[derive(Debug, Default)]
pub struct LockManager {
    locks: Mutex<HashMap<String, Lock>>,
}

pub enum AcquireError {
    Conflict(Vec<String>),
    TooMany,
}

impl LockManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn purge(map: &mut HashMap<String, Lock>) {
        let now = Instant::now();
        map.retain(|_, l| !l.expired(now));
    }

    /// Locks set directly on `key`.
    pub fn locks_on(&self, key: &str) -> Vec<Lock> {
        let mut map = self.locks.lock();
        Self::purge(&mut map);
        let mut out: Vec<Lock> = map.values().filter(|l| l.key == key).cloned().collect();
        out.sort_by(|a, b| a.token.cmp(&b.token));
        out
    }

    /// Tokens of the locks protecting `key` (direct, or a depth-infinity ancestor) held by
    /// `principal` (WsgiDAV `get_indirect_url_lock_list`).
    pub fn indirect_tokens(&self, key: &str, principal: &str) -> Vec<String> {
        let mut map = self.locks.lock();
        Self::purge(&mut map);
        map.values()
            .filter(|l| {
                l.principal == principal
                    && (l.key == key || (l.infinite_depth && is_child(&l.key, key)))
            })
            .map(|l| l.token.clone())
            .collect()
    }

    /// May `principal` modify `key` (and, for `infinite`, everything below it)?
    /// `Err(hrefs)` lists the conflicting lock roots (WsgiDAV `check_write_permission`).
    pub fn check_write(
        &self,
        key: &str,
        infinite: bool,
        tokens: &[String],
        principal: &str,
    ) -> Result<(), Vec<String>> {
        let mut map = self.locks.lock();
        Self::purge(&mut map);
        let mut conflicts = Vec::new();
        for l in map.values() {
            let direct = l.key == key;
            let via_parent = l.infinite_depth && is_child(&l.key, key) && !foreign_to_key(l, key);
            let below = infinite && is_child(key, &l.key) && !private_to_other(l, principal);
            if !(direct || via_parent || below) {
                continue;
            }
            if (direct || via_parent) && l.principal == principal && tokens.contains(&l.token) {
                continue;
            }
            conflicts.push(l.root_href.clone());
        }
        if conflicts.is_empty() {
            Ok(())
        } else {
            conflicts.sort();
            conflicts.dedup();
            Err(conflicts)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn acquire(
        &self,
        key: &str,
        root_href: &str,
        scope: Scope,
        infinite_depth: bool,
        owner_xml: String,
        timeout: i64,
        principal: &str,
    ) -> Result<Lock, AcquireError> {
        let mut map = self.locks.lock();
        Self::purge(&mut map);
        if map.len() >= MAX_LOCKS {
            return Err(AcquireError::TooMany);
        }
        let mut conflicts = Vec::new();
        for l in map.values() {
            let direct = l.key == key;
            let via_parent = l.infinite_depth && is_child(&l.key, key) && !foreign_to_key(l, key);
            let below = infinite_depth && is_child(key, &l.key) && !private_to_other(l, principal);
            if direct || via_parent {
                if l.scope == Scope::Shared && scope == Scope::Shared {
                    continue;
                }
                conflicts.push(l.root_href.clone());
            } else if below {
                conflicts.push(l.root_href.clone());
            }
        }
        if !conflicts.is_empty() {
            conflicts.sort();
            conflicts.dedup();
            return Err(AcquireError::Conflict(conflicts));
        }
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);
        let token = format!(
            "opaquelocktoken:{}",
            raw.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let lock = Lock {
            token: token.clone(),
            key: key.to_string(),
            root_href: root_href.to_string(),
            scope,
            infinite_depth,
            owner_xml,
            timeout,
            expires: (timeout >= 0).then(|| Instant::now() + Duration::from_secs(timeout as u64)),
            principal: principal.to_string(),
        };
        map.insert(token, lock.clone());
        Ok(lock)
    }

    pub fn refresh(&self, token: &str, timeout: i64) -> Option<Lock> {
        let mut map = self.locks.lock();
        Self::purge(&mut map);
        let lock = map.get_mut(token)?;
        lock.timeout = timeout;
        lock.expires = (timeout >= 0).then(|| Instant::now() + Duration::from_secs(timeout as u64));
        Some(lock.clone())
    }

    pub fn get(&self, token: &str) -> Option<Lock> {
        let mut map = self.locks.lock();
        Self::purge(&mut map);
        map.get(token).cloned()
    }

    /// Is `key` locked (directly or through an ancestor) by `token`?
    pub fn is_locked_by_token(&self, key: &str, token: &str) -> bool {
        self.get(token)
            .is_some_and(|l| l.key == key || is_child(&l.key, key))
    }

    pub fn release(&self, token: &str) {
        self.locks.lock().remove(token);
    }

    /// Forget every lock on `key` and below (the resource was deleted or moved away).
    pub fn remove_tree(&self, key: &str) {
        self.locks
            .lock()
            .retain(|_, l| !(l.key == key || is_child(key, &l.key)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(parent_key("/a/b"), Some("/a"));
        assert_eq!(parent_key("/a"), Some("/"));
        assert_eq!(
            parent_key("/mokuro-reader/goals.json\0alice"),
            Some("/mokuro-reader")
        );
        assert!(is_child("/a", "/a/b"));
        assert!(!is_child("/a", "/ab"));
        assert!(is_child("/mokuro-reader", "/mokuro-reader/goals.json\0al"));
        assert!(is_child("/", "/x"));
    }

    #[test]
    fn exclusive_conflicts_and_tokens() {
        let m = LockManager::new();
        let Ok(l) = m.acquire(
            "/a/b",
            "/a/b",
            Scope::Exclusive,
            true,
            String::new(),
            60,
            "alice",
        ) else {
            panic!()
        };
        assert!(matches!(
            m.acquire(
                "/a/b",
                "/a/b",
                Scope::Shared,
                false,
                String::new(),
                60,
                "bob"
            ),
            Err(AcquireError::Conflict(_))
        ));
        assert!(matches!(
            m.acquire(
                "/a",
                "/a/",
                Scope::Exclusive,
                true,
                String::new(),
                60,
                "bob"
            ),
            Err(AcquireError::Conflict(_))
        ));
        assert!(
            m.acquire(
                "/a",
                "/a/",
                Scope::Exclusive,
                false,
                String::new(),
                60,
                "bob"
            )
            .is_ok()
        );
        assert!(m.check_write("/a/b/c", false, &[], "alice").is_err());
        assert!(
            m.check_write("/a/b/c", false, std::slice::from_ref(&l.token), "alice")
                .is_ok()
        );
        assert!(
            m.check_write("/a/b/c", false, std::slice::from_ref(&l.token), "bob")
                .is_err()
        );
        assert!(m.is_locked_by_token("/a/b/c", &l.token));
        m.release(&l.token);
        assert!(m.check_write("/a/b/c", false, &[], "alice").is_ok());
        assert_eq!(parse_timeout(Some("Second-30")), 30);
        assert_eq!(parse_timeout(Some("Infinite, Second-4100000000")), -1);
        assert_eq!(parse_timeout(None), DEFAULT_TIMEOUT_SECS);
    }

    fn lock(m: &LockManager, key: &str, infinite: bool, who: &str) -> Result<Lock, AcquireError> {
        m.acquire(key, key, Scope::Exclusive, infinite, String::new(), 60, who)
    }

    /// A per-user key `<path>\0<account>` is outside every OTHER principal's lock scope,
    /// whichever lock came first: bob's private lock never blocks the editor's folder
    /// LOCK, and the editor's folder lock never blocks bob's LOCK or write of his own
    /// copy. Bob's own folder lock still covers it.
    #[test]
    fn a_private_key_is_outside_other_principals_locks_both_ways() {
        let private = "/s/v.edit.mokuro\0bob";

        // bob first, then the editor's folder lock.
        let m = LockManager::new();
        let Ok(mine) = lock(&m, private, false, "bob") else {
            panic!("bob's lock")
        };
        assert!(lock(&m, "/s", true, "editor").is_ok());
        assert!(
            m.check_write(private, false, std::slice::from_ref(&mine.token), "bob")
                .is_ok(),
            "bob's token still guarantees his own write"
        );
        assert!(m.check_write(private, false, &[], "bob").is_err());
        assert!(m.check_write("/s", true, &[], "editor").is_err());

        // The editor's folder lock first, then bob.
        let m = LockManager::new();
        let Ok(folder) = lock(&m, "/s", true, "editor") else {
            panic!("folder lock")
        };
        assert!(m.check_write(private, false, &[], "bob").is_ok());
        assert!(
            m.check_write("/s/v.edit.mokuro\0editor", false, &[], "editor")
                .is_err()
        );
        assert!(
            m.check_write(
                "/s/v.edit.mokuro\0editor",
                false,
                std::slice::from_ref(&folder.token),
                "editor"
            )
            .is_ok()
        );
        assert!(m.check_write("/s/v.mokuro", false, &[], "bob").is_err());
        let Ok(mine) = lock(&m, private, false, "bob") else {
            panic!("bob's lock under the editor's folder lock")
        };
        assert!(
            m.check_write(private, false, std::slice::from_ref(&mine.token), "bob")
                .is_ok()
        );

        // bob's own folder lock covers his own copy.
        let m = LockManager::new();
        assert!(lock(&m, "/s", true, "bob").is_ok());
        assert!(m.check_write(private, false, &[], "bob").is_err());
        assert!(matches!(
            lock(&m, private, false, "bob"),
            Err(AcquireError::Conflict(_))
        ));
    }
}
