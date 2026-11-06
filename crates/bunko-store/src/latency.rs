//! Latency classes (spec §3.1): a caller that must answer quickly (the bridge serving
//! a reader request) runs its store calls inside [`interactive`]. A remote store then
//! fails a content call with [`unavailable_for`](crate::unavailable_for) instead of
//! waiting out a provider pause longer than 2 s, and gives the call priority over
//! background work. Local stores ignore the class.

use std::cell::Cell;

thread_local! {
    static INTERACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Restores the previous flag, also when `f` panics.
struct Reset(bool);

impl Drop for Reset {
    fn drop(&mut self) {
        INTERACTIVE.with(|c| c.set(self.0));
    }
}

/// Run `f` with this thread's store calls in the interactive latency class. Nests;
/// the class covers only calls made on this thread (work `f` hands to another thread
/// runs in that thread's class).
pub fn interactive<T>(f: impl FnOnce() -> T) -> T {
    let _reset = Reset(INTERACTIVE.with(|c| c.replace(true)));
    f()
}

/// Whether the current thread runs inside [`interactive`].
pub fn is_interactive() -> bool {
    INTERACTIVE.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_is_scoped_and_nests() {
        assert!(!is_interactive());
        let inner = interactive(|| {
            let nested = interactive(is_interactive);
            (is_interactive(), nested)
        });
        assert_eq!(inner, (true, true));
        assert!(!is_interactive());
        let r = std::panic::catch_unwind(|| interactive(|| panic!("boom")));
        assert!(r.is_err());
        assert!(!is_interactive());
        // Another thread has its own class.
        interactive(|| {
            assert!(!std::thread::spawn(is_interactive).join().unwrap());
        });
    }
}
