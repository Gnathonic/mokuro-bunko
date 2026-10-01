//! Failed-login limiter (0.5.2 `AuthAttemptLimiter`): 10 failures in 300 s block the
//! `ip:username` key for 900 s.
//!
//! Unlike 0.5.2, [`AuthLimiter::allow`] *reserves* the attempt: it is counted as a
//! failure the moment it is allowed and forgotten again by [`AuthLimiter::record_success`].
//! 0.5.2 checked and recorded in two steps, which let any number of concurrent guesses
//! through before the first failure landed. The key table is bounded; when full, the
//! least recently active keys are evicted (never "stop recording", which would fail open).

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub struct AuthLimiter {
    max_failures: usize,
    window: Duration,
    block: Duration,
    inner: Mutex<HashMap<String, Entry>>,
}

#[derive(Default)]
struct Entry {
    failures: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

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
        }
    }

    /// `Ok(())` or `Err(retry_in_seconds)`.
    pub fn allow(&self, key: &str) -> Result<(), u64> {
        self.allow_at(key, Instant::now())
    }

    fn allow_at(&self, key: &str, now: Instant) -> Result<(), u64> {
        let mut map = self.inner.lock();
        if !map.contains_key(key) && map.len() >= MAX_KEYS {
            evict(&mut map, now, self.window);
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
        if e.failures.len() >= self.max_failures {
            e.failures.clear();
            e.blocked_until = Some(now + self.block);
            return Err(self.block.as_secs());
        }
        // Reserved: counts as a failure until record_success says otherwise.
        e.failures.push_back(now);
        Ok(())
    }

    /// The attempt was already counted by [`Self::allow`]; kept so call sites read like
    /// 0.5.2's.
    pub fn record_failure(&self, _key: &str) {}

    pub fn record_success(&self, key: &str) {
        self.inner.lock().remove(key);
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
    fn concurrent_guesses_are_counted_up_front() {
        let l = AuthLimiter::default();
        // Ten checks in flight before any result is known: the eleventh is refused.
        for _ in 0..10 {
            assert!(l.allow("k").is_ok());
        }
        assert!(l.allow("k").is_err());
    }

    #[test]
    fn full_table_still_limits_new_keys() {
        let l = AuthLimiter::default();
        for i in 0..MAX_KEYS {
            l.allow(&format!("k{i}")).unwrap();
        }
        for _ in 0..10 {
            assert!(l.allow("new").is_ok());
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
