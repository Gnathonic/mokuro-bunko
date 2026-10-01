//! Failed-login limiter (0.5.2 `AuthAttemptLimiter`): 10 failures in 300 s block the
//! `ip:username` key for 900 s. Unlike 0.5.2 the key table is bounded.

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
        let Some(e) = map.get_mut(key) else {
            return Ok(());
        };
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
        Ok(())
    }

    pub fn record_failure(&self, key: &str) {
        let now = Instant::now();
        let mut map = self.inner.lock();
        if map.len() >= MAX_KEYS && !map.contains_key(key) {
            // Evict entries with nothing recent and no block before growing further.
            map.retain(|_, e| {
                e.blocked_until.is_some_and(|u| u > now)
                    || e.failures
                        .back()
                        .is_some_and(|t| now.duration_since(*t) <= self.window)
            });
            if map.len() >= MAX_KEYS {
                return;
            }
        }
        map.entry(key.to_string())
            .or_default()
            .failures
            .push_back(now);
    }

    pub fn record_success(&self, key: &str) {
        self.inner.lock().remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
