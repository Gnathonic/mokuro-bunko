//! Client mistakes. The simulator never panics on a request: a request it cannot
//! accept (a missing parameter, an unknown query clause, a malformed body) gets a 4xx
//! answer in the provider's error shape, and a line here naming the provider, the
//! method, the path and what was wrong. Tests assert the list is empty at the end.

use parking_lot::Mutex;

#[derive(Default)]
pub struct Violations(Mutex<Vec<String>>);

impl Violations {
    pub fn push(&self, line: String) {
        self.0.lock().push(line);
    }

    pub fn list(&self) -> Vec<String> {
        self.0.lock().clone()
    }

    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock())
    }
}

/// What a handler returns when the request itself is wrong: the reason, recorded as a
/// violation and sent back in a 400.
pub type Bad = String;

/// `Some(v)` or the reason the request is wrong.
pub fn need<T>(v: Option<T>, what: &str) -> Result<T, Bad> {
    v.ok_or_else(|| what.to_string())
}
