use std::sync::Mutex;

use jiff::{Span, Timestamp};

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

pub struct FixedClock {
    now: Mutex<Timestamp>,
}

impl FixedClock {
    #[must_use]
    pub const fn new(at: Timestamp) -> Self {
        Self {
            now: Mutex::new(at),
        }
    }

    /// # Panics
    /// panics if the clock's lock is poisoned, or if the sum leaves jiff's range
    pub fn advance(&self, by: Span) {
        let mut guard = self.now.lock().unwrap();
        *guard += by;
    }

    /// # Panics
    /// panics if the clock's lock is poisoned
    pub fn set(&self, at: Timestamp) {
        *self.now.lock().unwrap() = at;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        *self.now.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_returns_set_time_and_advances() {
        let t0: Timestamp = "2026-08-23T02:00:00Z".parse().unwrap();
        let clock = FixedClock::new(t0);
        assert_eq!(clock.now(), t0);

        clock.advance(Span::new().hours(3));
        assert_eq!(clock.now(), t0 + Span::new().hours(3));
    }

    #[test]
    fn system_clock_is_monotonic_across_calls() {
        let clock = SystemClock;
        let a = clock.now();
        let b = clock.now();
        assert!(b >= a);
    }
}
