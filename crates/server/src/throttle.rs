//! Per-account login backoff.
//!
//! The IP rate limiter (`ratelimit`) stops one client hammering the server,
//! but an attacker with many addresses can still aim every guess at the same
//! account. This tracker is keyed by *username* instead: after
//! [`FREE_FAILURES`] consecutive wrong passwords, each further attempt must
//! wait an exponentially growing delay (1 s, 2 s, 4 s, … capped at
//! [`MAX_LOCKOUT`]) before the password is even checked.
//!
//! Check and record are **one atomic step**: [`LoginThrottle::begin`] either
//! refuses the attempt or *reserves* it (counts it as in flight) under the
//! same lock, and the returned [`Attempt`] is later resolved as a success or a
//! failure. Without the reservation, N requests fired at once would all pass
//! the check before any of them recorded its failure, and every one would get
//! its password verified. With it, at most "free attempts remaining" (and,
//! once in backoff, exactly one) verifications are in flight per username.
//!
//! It deliberately tracks usernames that don't exist too. Otherwise "gets
//! locked out after 5 tries" vs "never gets locked out" would reveal which
//! usernames are real.
//!
//! The flip side, accepted on purpose: anyone who knows a username can keep
//! that account in backoff. Existing sessions keep working; see SECURITY.md.
//!
//! State is in memory only: a restart forgets it, which is fine — the point
//! is to make online guessing slow, not to keep a permanent record.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Wrong passwords allowed before any delay kicks in.
pub const FREE_FAILURES: u32 = 5;
/// Upper bound for the delay between attempts once locked.
pub const MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// Most usernames tracked at once. Past this the least recently seen
/// *evictable* entry is dropped (see [`LoginThrottle::begin`]), so a flood of
/// random usernames can't exhaust memory.
pub const MAX_TRACKED: usize = 10_000;
/// Usernames are at most 32 chars; anything longer can't be a real account,
/// so truncating the key costs nothing and bounds per-entry memory.
const MAX_KEY_CHARS: usize = 64;
/// `Retry-After` for an attempt refused because others for the same account
/// are still being checked (their outcome decides the real delay).
const BUSY_RETRY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
struct Entry {
    /// Consecutive failures since the last success.
    failures: u32,
    /// Attempts reserved by `begin` whose password check hasn't finished.
    in_flight: u32,
    /// No attempt is checked before this instant.
    locked_until: Option<Instant>,
    /// For LRU eviction.
    last_seen: Instant,
}

impl Entry {
    fn fresh(now: Instant) -> Self {
        Self {
            failures: 0,
            in_flight: 0,
            locked_until: None,
            last_seen: now,
        }
    }

    fn is_locked(&self, now: Instant) -> bool {
        self.locked_until.is_some_and(|until| until > now)
    }

    /// How many attempts may be in flight at once: the free failures still
    /// left, or — once in backoff — exactly one per unlock.
    fn slots(&self) -> u32 {
        FREE_FAILURES.saturating_sub(self.failures).max(1)
    }

    /// Nothing worth remembering: dropping it changes no behaviour.
    fn is_idle(&self, now: Instant) -> bool {
        self.failures == 0 && self.in_flight == 0 && !self.is_locked(now)
    }
}

#[derive(Debug)]
pub struct LoginThrottle {
    // A plain `std::sync::Mutex` (not tokio's) is right here: the lock is held
    // for a few map operations and never across an `.await`.
    entries: Mutex<HashMap<String, Entry>>,
    capacity: usize,
}

impl Default for LoginThrottle {
    fn default() -> Self {
        Self::with_capacity(MAX_TRACKED)
    }
}

fn key(username: &str) -> String {
    username
        .chars()
        .take(MAX_KEY_CHARS)
        .flat_map(char::to_lowercase)
        .collect()
}

/// Delay imposed after the `failures`-th consecutive failure: 0 up to
/// `FREE_FAILURES - 1`, then 2^(failures − FREE_FAILURES) seconds, capped.
pub fn lockout_for(failures: u32) -> Duration {
    if failures < FREE_FAILURES {
        return Duration::ZERO;
    }
    // Cap the exponent before shifting: 2^10 s already exceeds 15 min, and
    // `1u64 << 64` would overflow.
    let exp = (failures - FREE_FAILURES).min(20);
    Duration::from_secs(1u64 << exp).min(MAX_LOCKOUT)
}

/// A reserved login attempt. Resolve it with [`Attempt::failed`] or
/// [`Attempt::succeeded`]; if it is simply dropped (e.g. a database error
/// bailed out with `?` before the password was judged), `Drop` releases the
/// reservation without counting a failure.
///
/// The `'a` lifetime ties the guard to the throttle it came from, so the
/// compiler guarantees the throttle outlives every open attempt.
#[must_use = "an Attempt must be resolved as failed or succeeded"]
#[derive(Debug)]
pub struct Attempt<'a> {
    throttle: &'a LoginThrottle,
    key: String,
    resolved: bool,
}

impl Attempt<'_> {
    /// Wrong password: finalize the reservation as a failure and return the
    /// new consecutive-failure count.
    pub fn failed(mut self, now: Instant) -> u32 {
        self.resolved = true;
        let mut map = self.throttle.lock();
        // Normally the entry exists (we hold a reservation, so it can't have
        // been evicted). `or_insert_with` covers the case where a concurrent
        // success removed it.
        let e = map
            .entry(self.key.clone())
            .or_insert_with(|| Entry::fresh(now));
        e.in_flight = e.in_flight.saturating_sub(1);
        e.failures = e.failures.saturating_add(1);
        e.last_seen = now;
        let delay = lockout_for(e.failures);
        e.locked_until = (!delay.is_zero()).then(|| now + delay);
        e.failures
    }

    /// Correct password: the account's failure history is cleared.
    pub fn succeeded(mut self) {
        self.resolved = true;
        let mut map = self.throttle.lock();
        if let Some(e) = map.get_mut(&self.key) {
            e.in_flight = e.in_flight.saturating_sub(1);
            e.failures = 0;
            e.locked_until = None;
            // Keep the entry only while other attempts still hold slots in it.
            if e.in_flight == 0 {
                map.remove(&self.key);
            }
        }
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        let mut map = self.throttle.lock();
        if let Some(e) = map.get_mut(&self.key) {
            e.in_flight = e.in_flight.saturating_sub(1);
            if e.is_idle(Instant::now()) {
                map.remove(&self.key);
            }
        }
    }
}

impl LoginThrottle {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // A poisoned mutex only means another thread panicked while holding
        // it; the map itself is still usable, so recover instead of panicking.
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Check-and-reserve, atomically. `Err(wait)` if `username` is locked out
    /// at `now`, or if all of its current attempt slots are already in flight.
    /// Otherwise the attempt is counted as in flight until the returned
    /// [`Attempt`] is resolved or dropped.
    ///
    /// **Eviction.** A new username past `capacity` evicts the least recently
    /// seen entry that is neither locked nor in flight — never a locked one,
    /// since forgetting it would reset its lockout. If *every* tracked entry
    /// is locked or in flight, the new username is refused (fails closed)
    /// until the soonest lock expires. Reaching that state takes thousands of
    /// distinct usernames driven into lockout, all behind the IP rate limits;
    /// an account that already has an entry is unaffected.
    pub fn begin(&self, username: &str, now: Instant) -> Result<Attempt<'_>, Duration> {
        let k = key(username);
        let mut map = self.lock();
        if !map.contains_key(&k) && map.len() >= self.capacity {
            // O(n), but only when full.
            let victim = map
                .iter()
                .filter(|(_, e)| !e.is_locked(now) && e.in_flight == 0)
                .min_by_key(|(_, e)| e.last_seen)
                .map(|(k, _)| k.clone());
            match victim {
                Some(v) => {
                    map.remove(&v);
                }
                None => {
                    let soonest = map
                        .values()
                        .filter_map(|e| e.locked_until)
                        .filter(|until| *until > now)
                        .min()
                        .map_or(BUSY_RETRY, |until| until - now);
                    return Err(soonest.max(BUSY_RETRY));
                }
            }
        }
        let e = map.entry(k.clone()).or_insert_with(|| Entry::fresh(now));
        if let Some(until) = e.locked_until.filter(|u| *u > now) {
            return Err(until - now);
        }
        if e.in_flight >= e.slots() {
            // Everything this account may try right now is already being
            // checked. If they all fail, this is how long the lock will be.
            return Err(lockout_for(e.failures + e.in_flight).max(BUSY_RETRY));
        }
        e.in_flight += 1;
        e.last_seen = now;
        Ok(Attempt {
            throttle: self,
            key: k,
            resolved: false,
        })
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One complete wrong-password attempt.
    fn fail(t: &LoginThrottle, name: &str, now: Instant) -> u32 {
        t.begin(name, now).expect("not locked").failed(now)
    }

    /// Is a new attempt allowed right now? (Reserves and releases.)
    fn check(t: &LoginThrottle, name: &str, now: Instant) -> Result<(), Duration> {
        t.begin(name, now).map(drop)
    }

    #[test]
    fn delay_schedule() {
        assert_eq!(lockout_for(4), Duration::ZERO);
        assert_eq!(lockout_for(5), Duration::from_secs(1));
        assert_eq!(lockout_for(6), Duration::from_secs(2));
        assert_eq!(lockout_for(8), Duration::from_secs(8));
        assert_eq!(lockout_for(100), MAX_LOCKOUT);
        assert_eq!(lockout_for(u32::MAX), MAX_LOCKOUT);
    }

    #[test]
    fn locks_after_free_failures_and_unlocks_after_the_delay() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for _ in 0..FREE_FAILURES - 1 {
            fail(&t, "mikko", now);
            assert!(check(&t, "mikko", now).is_ok());
        }
        fail(&t, "mikko", now); // 5th
        assert_eq!(check(&t, "mikko", now), Err(Duration::from_secs(1)));
        // Case-insensitive key.
        assert!(check(&t, "MIKKO", now).is_err());
        // Other accounts unaffected.
        assert!(check(&t, "liisa", now).is_ok());
        // After the delay, one more attempt is allowed.
        let later = now + Duration::from_secs(1);
        assert!(check(&t, "mikko", later).is_ok());
        // A 6th failure doubles the wait.
        fail(&t, "mikko", later);
        assert_eq!(check(&t, "mikko", later), Err(Duration::from_secs(2)));
    }

    #[test]
    fn success_resets() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for _ in 0..FREE_FAILURES - 1 {
            fail(&t, "mikko", now);
        }
        t.begin("mikko", now).unwrap().succeeded();
        assert!(check(&t, "mikko", now).is_ok());
        assert_eq!(fail(&t, "mikko", now), 1);
        t.begin("mikko", now).unwrap().succeeded();
        assert_eq!(t.len(), 0, "a success leaves nothing behind");
    }

    #[test]
    fn in_flight_attempts_are_capped_at_the_free_failures_left() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        fail(&t, "mikko", now); // 4 free attempts left
        let held: Vec<_> = (0..FREE_FAILURES - 1)
            .map(|_| t.begin("mikko", now).expect("slot free"))
            .collect();
        // All four slots are in flight: a fifth concurrent attempt is refused.
        assert!(t.begin("mikko", now).is_err());
        // If they all fail, the account is locked…
        for a in held {
            a.failed(now);
        }
        assert_eq!(check(&t, "mikko", now), Err(Duration::from_secs(1)));
        // …and once in backoff, only ONE attempt per unlock may be in flight.
        let later = now + Duration::from_secs(1);
        let one = t.begin("mikko", later).expect("unlocked");
        assert!(t.begin("mikko", later).is_err());
        one.failed(later);
        assert_eq!(check(&t, "mikko", later), Err(Duration::from_secs(2)));
    }

    #[test]
    fn dropping_an_attempt_releases_its_slot_without_a_failure() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for _ in 0..FREE_FAILURES {
            drop(t.begin("mikko", now).unwrap());
        }
        assert_eq!(t.len(), 0);
        assert_eq!(fail(&t, "mikko", now), 1);
    }

    #[test]
    fn map_size_is_bounded_by_evicting_the_oldest_unlocked() {
        let t = LoginThrottle::with_capacity(3);
        let t0 = Instant::now();
        for (i, name) in ["a", "b", "c", "d"].iter().enumerate() {
            fail(&t, name, t0 + Duration::from_secs(i as u64));
        }
        assert_eq!(t.len(), 3);
        // "a" was the oldest and got evicted: it starts from scratch.
        assert_eq!(fail(&t, "a", t0 + Duration::from_secs(10)), 1);
    }

    #[test]
    fn a_locked_entry_is_never_evicted() {
        let t = LoginThrottle::with_capacity(3);
        let t0 = Instant::now();
        // "locked" is the OLDEST entry, locked until t0 + 1 s.
        for _ in 0..FREE_FAILURES {
            fail(&t, "locked", t0);
        }
        let ms = |n| t0 + Duration::from_millis(n);
        fail(&t, "b", ms(1));
        fail(&t, "c", ms(2));
        // Full. A new name must evict "b" (oldest UNLOCKED), not "locked".
        fail(&t, "d", ms(3));
        assert_eq!(t.len(), 3);
        assert!(check(&t, "locked", ms(4)).is_err(), "lockout survived");
        assert_eq!(fail(&t, "b", ms(5)), 1, "b was the one evicted");
    }

    #[test]
    fn when_every_entry_is_locked_new_names_are_refused() {
        let t = LoginThrottle::with_capacity(2);
        let now = Instant::now();
        for name in ["x", "y"] {
            for _ in 0..FREE_FAILURES {
                fail(&t, name, now);
            }
        }
        // Both tracked names are locked (1 s): a third name fails closed…
        assert_eq!(check(&t, "z", now), Err(Duration::from_secs(1)));
        assert_eq!(t.len(), 2);
        // …the locked ones stay locked…
        assert!(check(&t, "x", now).is_err());
        // …and once a lock expires, that entry is evictable again.
        assert!(check(&t, "z", now + Duration::from_secs(1)).is_ok());
    }
}
