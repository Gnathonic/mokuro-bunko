//! Failed-login limiter (0.5.2 `AuthAttemptLimiter`): 10 failures in 300 s block the
//! `ip:username` key for 900 s.
//!
//! 0.5.2 checked and recorded in two steps, which let any number of concurrent guesses
//! through before the first failure landed. Here at most [`MAX_PENDING`] unverified checks
//! per key are in flight: [`AuthLimiter::allow_blocking`] (the WebDAV path, already on a
//! blocking thread) waits for a slot, [`AuthLimiter::allow`] (async login endpoints)
//! answers "retry in 1 s". Only real failures count towards the block, so a client making
//! many parallel requests with the right password is never refused. The key table is
//! bounded; when full, the least recently active keys are evicted (never "stop
//! recording", which would fail open).

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub struct AuthLimiter {
    max_failures: usize,
    window: Duration,
    block: Duration,
    inner: Mutex<HashMap<String, Entry>>,
    freed: parking_lot::Condvar,
}

#[derive(Default)]
struct Entry {
    failures: VecDeque<Instant>,
    blocked_until: Option<Instant>,
    /// Checks allowed but not yet resolved (success/failure); stale ones expire.
    pending: VecDeque<Instant>,
}

/// Unverified password checks one `ip:username` key may have in flight at once.
pub const MAX_PENDING: usize = 4;
/// A pending check older than this is forgotten (its caller never reported back).
const PENDING_TTL: Duration = Duration::from_secs(60);

const MAX_KEYS: usize = 10_000;

impl Default for AuthLimiter {
    fn default() -> Self {
        Self::new(10, Duration::from_secs(300), Duration::from_secs(900))
    }
}

impl AuthLimiter {
    pub fn new(max_failures: usize, window: Duration, block: Duration) -> Self {
        Self {
            max_failures,
            window,
            block,
            inner: Mutex::new(HashMap::new()),
            freed: parking_lot::Condvar::new(),
        }
    }

    /// `Ok(())` or `Err(retry_in_seconds)`; never waits (async callers). A key with
    /// [`MAX_PENDING`] checks in flight answers `Err(1)`.
    pub fn allow(&self, key: &str) -> Result<(), u64> {
        let mut map = self.inner.lock();
        match self.check(&mut map, key, Instant::now()) {
            Ok(true) => Ok(()),
            Ok(false) => Err(1),
            Err(retry) => Err(retry),
        }
    }

    /// Like [`Self::allow`], but waits (up to 30 s) for an in-flight slot instead of
    /// refusing. Call only from a blocking thread.
    pub fn allow_blocking(&self, key: &str) -> Result<(), u64> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut map = self.inner.lock();
        loop {
            match self.check(&mut map, key, Instant::now()) {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    if self.freed.wait_until(&mut map, deadline).timed_out() {
                        return Err(1);
                    }
                }
                Err(retry) => return Err(retry),
            }
        }
    }

    /// `Ok(true)`: allowed and counted as pending; `Ok(false)`: no slot free; `Err`: blocked.
    fn check(
        &self,
        map: &mut HashMap<String, Entry>,
        key: &str,
        now: Instant,
    ) -> Result<bool, u64> {
        if !map.contains_key(key) && map.len() >= MAX_KEYS {
            evict(map, now, self.window);
        }
        let e = map.entry(key.to_string()).or_default();
        if let Some(until) = e.blocked_until {
            if until > now {
                return Err((until - now).as_secs() + 1);
            }
            e.blocked_until = None;
        }
        while e
            .failures
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.window)
        {
            e.failures.pop_front();
        }
        while e
            .pending
            .front()
            .is_some_and(|t| now.duration_since(*t) > PENDING_TTL)
        {
            e.pending.pop_front();
        }
        if e.failures.len() >= self.max_failures {
            e.failures.clear();
            e.blocked_until = Some(now + self.block);
            return Err(self.block.as_secs());
        }
        if e.pending.len() >= MAX_PENDING {
            return Ok(false);
        }
        e.pending.push_back(now);
        Ok(true)
    }

    fn resolve(&self, key: &str, failed: bool) {
        let mut map = self.inner.lock();
        if let Some(e) = map.get_mut(key) {
            e.pending.pop_front();
            if failed {
                e.failures.push_back(Instant::now());
            } else {
                e.failures.clear();
                e.blocked_until = None;
            }
            if !failed && e.pending.is_empty() {
                map.remove(key);
            }
        }
        drop(map);
        self.freed.notify_all();
    }

    pub fn record_failure(&self, key: &str) {
        self.resolve(key, true);
    }

    pub fn record_success(&self, key: &str) {
        self.resolve(key, false);
    }
}

/// Drop expired keys; if still full, drop the least recently active tenth.
fn evict(map: &mut HashMap<String, Entry>, now: Instant, window: Duration) {
    map.retain(|_, e| {
        e.blocked_until.is_some_and(|u| u > now)
            || e.failures
                .back()
                .is_some_and(|t| now.duration_since(*t) <= window)
    });
    if map.len() < MAX_KEYS {
        return;
    }
    let mut by_age: Vec<(Instant, String)> = map
        .iter()
        .map(|(k, e)| {
            (
                e.failures
                    .back()
                    .copied()
                    .or(e.blocked_until)
                    .unwrap_or(now),
                k.clone(),
            )
        })
        .collect();
    by_age.sort();
    for (_, k) in by_age.into_iter().take(MAX_KEYS / 10) {
        map.remove(&k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_checks_are_capped_but_successes_never_block() {
        let l = AuthLimiter::default();
        for _ in 0..MAX_PENDING {
            assert!(l.allow("k").is_ok());
        }
        assert_eq!(l.allow("k"), Err(1));
        // Parallel requests with the right password: each success frees its slot and
        // nothing accumulates towards a block.
        for _ in 0..50 {
            l.record_success("k");
            assert!(l.allow("k").is_ok());
        }
    }

    #[test]
    fn full_table_still_limits_new_keys() {
        let l = AuthLimiter::default();
        for i in 0..MAX_KEYS {
            let k = format!("k{i}");
            l.allow(&k).unwrap();
            l.record_failure(&k);
        }
        for _ in 0..10 {
            assert!(l.allow("new").is_ok());
            l.record_failure("new");
        }
        assert!(l.allow("new").is_err());
    }

    #[test]
    fn blocks_after_ten() {
        let l = AuthLimiter::default();
        for _ in 0..10 {
            assert!(l.allow("k").is_ok());
            l.record_failure("k");
        }
        assert_eq!(l.allow("k"), Err(900));
        let r = l.allow("k").unwrap_err();
        assert!((899..=901).contains(&r));
        l.record_success("k");
        assert!(l.allow("k").is_ok());
    }
}
