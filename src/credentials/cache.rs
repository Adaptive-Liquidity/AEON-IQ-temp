//! Bounded credential cache with an absolute TTL (plan §2, decision A-1).
//!
//! # Why absolute, not sliding
//!
//! A sliding (idle) TTL resets on every hit. Under one, a *continuously used*
//! revoked credential never expires — each request pushes the deadline out
//! again — so revocation outlives its bound indefinitely for exactly the
//! credential an attacker is actively using. That is the one failure mode the
//! cache must not have, so the deadline is set once at insert and never
//! extended.
//!
//! # The two deadlines
//!
//! Each entry carries both:
//!
//! * a **monotonic** deadline (`expires_tick`), which governs the 30-second
//!   TTL. Monotonic so that a wall-clock jump backwards — NTP correction, VM
//!   restore, an operator setting the date — cannot extend a cached revoked
//!   credential's life.
//! * a **wall-clock** deadline (`expires_wall`), which is
//!   `min(inserted_at + TTL, the credential's own expiry)`. This is what makes
//!   a cache entry unable to outlive the credential it describes: a credential
//!   expiring in 5 seconds gets a 5-second entry, not a 30-second one.
//!
//! An entry is live only while **both** hold.
//!
//! # Failure mode
//!
//! Every operation returns `Result`. A poisoned mutex — meaning a thread
//! panicked mid-update and the map may be torn — yields `Err`, which the
//! authenticator turns into a rejection. The cache can therefore make
//! authentication fail, never succeed.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// **FROZEN** (decision A-1). A revoked credential must stop authenticating
/// within this bound, so it is a constant rather than a setting: making it
/// configurable would make the guarantee configurable.
pub const MAX_TTL: Duration = Duration::from_secs(30);

/// Entries held before eviction begins.
pub const DEFAULT_CAPACITY: usize = 1024;

/// Time, injected so tests can advance it deterministically instead of sleeping.
pub trait Clock: Send + Sync + fmt::Debug {
    /// Wall-clock time, compared against credential expiry and revocation.
    fn now(&self) -> DateTime<Utc>;
    /// A monotonic reading. Only differences between two readings are
    /// meaningful; the origin is arbitrary.
    fn tick(&self) -> Duration;
}

/// The production clock.
#[derive(Debug)]
pub struct SystemClock {
    start: std::time::Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn tick(&self) -> Duration {
        self.start.elapsed()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("credential cache is unusable (poisoned lock); failing closed")]
pub struct CacheError;

#[derive(Debug, Clone)]
struct Entry<V> {
    value: V,
    /// Monotonic deadline. Written once at insert; never extended on access.
    expires_tick: Duration,
    /// `min(inserted_at + ttl, the credential's own bound)`.
    expires_wall: DateTime<Utc>,
}

/// A capacity-bounded map whose entries expire on an absolute deadline.
pub struct BoundedTtlCache<V> {
    entries: Mutex<HashMap<Uuid, Entry<V>>>,
    capacity: usize,
    ttl: Duration,
}

impl<V> fmt::Debug for BoundedTtlCache<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately does not render the values: they are credential records.
        f.debug_struct("BoundedTtlCache")
            .field("capacity", &self.capacity)
            .field("ttl", &self.ttl)
            .field(
                "len",
                &self.entries.lock().map(|e| e.len()).unwrap_or(usize::MAX),
            )
            .finish()
    }
}

impl<V: Clone> BoundedTtlCache<V> {
    /// `ttl` is clamped to [`MAX_TTL`]: configuration may shorten the window,
    /// never lengthen it, so no deployment can widen the revocation bound.
    ///
    /// A capacity of zero is raised to one — a cache that cannot hold anything
    /// would silently turn every request into a database round-trip.
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
            ttl: ttl.min(MAX_TTL),
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Fetch a live entry.
    ///
    /// Does **not** touch the deadline. An expired entry is dropped here rather
    /// than returned, which is eviction, not refresh.
    pub fn get(&self, key: Uuid, clock: &dyn Clock) -> Result<Option<V>, CacheError> {
        let mut entries = self.entries.lock().map_err(|_| CacheError)?;
        let Some(entry) = entries.get(&key) else {
            return Ok(None);
        };
        if is_live(entry, clock) {
            Ok(Some(entry.value.clone()))
        } else {
            entries.remove(&key);
            Ok(None)
        }
    }

    /// Insert or replace an entry.
    ///
    /// `hard_bound` is the credential's own expiry, if it has one. The entry's
    /// wall-clock deadline is the earlier of that and the TTL, so a cache entry
    /// can never outlive its credential.
    pub fn insert(
        &self,
        key: Uuid,
        value: V,
        hard_bound: Option<DateTime<Utc>>,
        clock: &dyn Clock,
    ) -> Result<(), CacheError> {
        let now_tick = clock.tick();
        let now_wall = clock.now();

        let mut entries = self.entries.lock().map_err(|_| CacheError)?;

        if entries.len() >= self.capacity && !entries.contains_key(&key) {
            // Expired entries first — they are free to drop and may be all that
            // is needed.
            entries.retain(|_, e| is_live_at(e, now_tick, now_wall));

            if entries.len() >= self.capacity {
                // Then the entry closest to its own deadline, which is the one
                // whose eviction costs the least future work. O(n) over a
                // bounded n, on the miss path only.
                let victim = entries
                    .iter()
                    .min_by_key(|(_, e)| e.expires_tick)
                    .map(|(k, _)| *k);
                if let Some(victim) = victim {
                    entries.remove(&victim);
                }
            }
        }

        let ttl_wall =
            chrono::Duration::from_std(self.ttl).expect("TTL is at most 30 s, always in range");
        let mut expires_wall = now_wall + ttl_wall;
        if let Some(bound) = hard_bound {
            expires_wall = expires_wall.min(bound);
        }

        entries.insert(
            key,
            Entry {
                value,
                expires_tick: now_tick + self.ttl,
                expires_wall,
            },
        );
        Ok(())
    }

    /// Drop one entry, so a revocation performed through this process takes
    /// effect immediately rather than at the TTL bound.
    ///
    /// This is an optimisation, not the guarantee: a revocation performed by
    /// another replica or by direct SQL is still bound by the TTL, which is why
    /// the TTL exists.
    pub fn invalidate(&self, key: Uuid) -> Result<(), CacheError> {
        self.entries.lock().map_err(|_| CacheError)?.remove(&key);
        Ok(())
    }

    pub fn len(&self) -> Result<usize, CacheError> {
        Ok(self.entries.lock().map_err(|_| CacheError)?.len())
    }
}

fn is_live<V>(entry: &Entry<V>, clock: &dyn Clock) -> bool {
    is_live_at(entry, clock.tick(), clock.now())
}

fn is_live_at<V>(entry: &Entry<V>, now_tick: Duration, now_wall: DateTime<Utc>) -> bool {
    now_tick < entry.expires_tick && now_wall < entry.expires_wall
}

// ── Test clock ───────────────────────────────────────────────────────────────

/// A manually advanced clock.
///
/// Both readings move together via [`TestClock::advance`], so a test that
/// advances 31 seconds moves the monotonic and wall-clock deadlines alike.
/// [`TestClock::advance_wall_only`] exists to prove the monotonic deadline is
/// the one holding the TTL.
#[cfg(test)]
#[derive(Debug)]
pub struct TestClock {
    inner: Mutex<(DateTime<Utc>, Duration)>,
}

#[cfg(test)]
impl TestClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            inner: Mutex::new((start, Duration::ZERO)),
        }
    }

    pub fn advance(&self, by: Duration) {
        let mut inner = self.inner.lock().unwrap();
        inner.0 += chrono::Duration::from_std(by).expect("test durations are in range");
        inner.1 += by;
    }

    /// Move wall-clock time without moving the monotonic reading.
    pub fn advance_wall_only(&self, by: chrono::Duration) {
        self.inner.lock().unwrap().0 += by;
    }
}

#[cfg(test)]
impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        self.inner.lock().unwrap().0
    }

    fn tick(&self) -> Duration {
        self.inner.lock().unwrap().1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn epoch() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("fixed valid timestamp")
    }

    fn cache() -> BoundedTtlCache<u32> {
        BoundedTtlCache::new(DEFAULT_CAPACITY, MAX_TTL)
    }

    #[test]
    fn the_frozen_ttl_is_thirty_seconds() {
        // Decision A-1. If this changes, the revocation guarantee changes with
        // it and the plan needs amending first.
        assert_eq!(MAX_TTL, Duration::from_secs(30));
    }

    #[test]
    fn a_configured_ttl_may_shorten_but_never_lengthen() {
        assert_eq!(
            BoundedTtlCache::<u32>::new(4, Duration::from_secs(5)).ttl(),
            Duration::from_secs(5)
        );
        assert_eq!(
            BoundedTtlCache::<u32>::new(4, Duration::from_secs(3600)).ttl(),
            MAX_TTL,
            "a longer TTL must clamp, not widen the revocation bound"
        );
    }

    #[test]
    fn an_entry_is_returned_before_its_deadline() {
        let clock = TestClock::new(epoch());
        let cache = cache();
        cache.insert(Uuid::nil(), 7, None, &clock).unwrap();
        clock.advance(Duration::from_secs(29));
        assert_eq!(cache.get(Uuid::nil(), &clock).unwrap(), Some(7));
    }

    #[test]
    fn an_entry_is_gone_at_the_deadline() {
        let clock = TestClock::new(epoch());
        let cache = cache();
        cache.insert(Uuid::nil(), 7, None, &clock).unwrap();
        clock.advance(Duration::from_secs(30));
        assert_eq!(
            cache.get(Uuid::nil(), &clock).unwrap(),
            None,
            "the bound is 30 s inclusive: at exactly 30 s the entry is dead"
        );
    }

    #[test]
    fn the_ttl_is_absolute_and_repeated_access_does_not_extend_it() {
        // The core anti-sliding property. A credential read every second for
        // 29 seconds must still expire on schedule at 30.
        let clock = TestClock::new(epoch());
        let cache = cache();
        cache.insert(Uuid::nil(), 7, None, &clock).unwrap();

        for second in 1..30 {
            clock.advance(Duration::from_secs(1));
            assert_eq!(
                cache.get(Uuid::nil(), &clock).unwrap(),
                Some(7),
                "still live at {second} s"
            );
        }

        clock.advance(Duration::from_secs(1));
        assert_eq!(
            cache.get(Uuid::nil(), &clock).unwrap(),
            None,
            "30 accesses must not have pushed the deadline out"
        );
    }

    #[test]
    fn an_entry_never_outlives_the_credentials_own_bound() {
        let clock = TestClock::new(epoch());
        let cache = cache();
        // Credential expires in 5 s; the TTL would have allowed 30.
        let bound = epoch() + chrono::Duration::seconds(5);
        cache.insert(Uuid::nil(), 7, Some(bound), &clock).unwrap();

        clock.advance(Duration::from_secs(4));
        assert_eq!(cache.get(Uuid::nil(), &clock).unwrap(), Some(7));

        clock.advance(Duration::from_secs(2)); // t = 6 s, still inside the TTL
        assert_eq!(
            cache.get(Uuid::nil(), &clock).unwrap(),
            None,
            "the credential's own expiry must win over the longer TTL"
        );
    }

    #[test]
    fn a_bound_beyond_the_ttl_does_not_extend_the_entry() {
        let clock = TestClock::new(epoch());
        let cache = cache();
        let bound = epoch() + chrono::Duration::days(365);
        cache.insert(Uuid::nil(), 7, Some(bound), &clock).unwrap();
        clock.advance(Duration::from_secs(31));
        assert_eq!(cache.get(Uuid::nil(), &clock).unwrap(), None);
    }

    #[test]
    fn a_backwards_wall_clock_jump_does_not_resurrect_an_entry() {
        // Monotonic deadline holds the TTL, so an operator setting the clock
        // back an hour cannot extend a cached revoked credential.
        let clock = TestClock::new(epoch());
        let cache = cache();
        cache.insert(Uuid::nil(), 7, None, &clock).unwrap();

        clock.advance(Duration::from_secs(31));
        assert_eq!(cache.get(Uuid::nil(), &clock).unwrap(), None);

        cache.insert(Uuid::nil(), 7, None, &clock).unwrap();
        clock.advance(Duration::from_secs(31));
        clock.advance_wall_only(chrono::Duration::hours(-1));
        assert_eq!(
            cache.get(Uuid::nil(), &clock).unwrap(),
            None,
            "wall clock went backwards but the monotonic deadline still holds"
        );
    }

    #[test]
    fn invalidate_drops_an_entry_immediately() {
        let clock = TestClock::new(epoch());
        let cache = cache();
        cache.insert(Uuid::nil(), 7, None, &clock).unwrap();
        cache.invalidate(Uuid::nil()).unwrap();
        assert_eq!(cache.get(Uuid::nil(), &clock).unwrap(), None);
    }

    // ── Boundedness ──────────────────────────────────────────────────────────

    #[test]
    fn the_cache_never_exceeds_its_capacity() {
        let clock = TestClock::new(epoch());
        let cache = BoundedTtlCache::new(4, MAX_TTL);
        for i in 0..100u32 {
            cache
                .insert(Uuid::from_u128(i as u128), i, None, &clock)
                .unwrap();
            assert!(cache.len().unwrap() <= 4, "capacity exceeded at insert {i}");
        }
        assert_eq!(cache.len().unwrap(), 4);
    }

    #[test]
    fn expired_entries_are_reclaimed_before_live_ones_are_evicted() {
        let clock = TestClock::new(epoch());
        let cache = BoundedTtlCache::new(2, MAX_TTL);

        cache.insert(Uuid::from_u128(1), 1, None, &clock).unwrap();
        cache.insert(Uuid::from_u128(2), 2, None, &clock).unwrap();

        // Both entries age out, then a third arrives.
        clock.advance(Duration::from_secs(31));
        cache.insert(Uuid::from_u128(3), 3, None, &clock).unwrap();

        assert_eq!(
            cache.len().unwrap(),
            1,
            "the two dead entries were reclaimed"
        );
        assert_eq!(cache.get(Uuid::from_u128(3), &clock).unwrap(), Some(3));
    }

    #[test]
    fn replacing_an_existing_key_does_not_evict() {
        let clock = TestClock::new(epoch());
        let cache = BoundedTtlCache::new(2, MAX_TTL);
        cache.insert(Uuid::from_u128(1), 1, None, &clock).unwrap();
        cache.insert(Uuid::from_u128(2), 2, None, &clock).unwrap();
        cache.insert(Uuid::from_u128(1), 9, None, &clock).unwrap();

        assert_eq!(cache.len().unwrap(), 2);
        assert_eq!(cache.get(Uuid::from_u128(1), &clock).unwrap(), Some(9));
        assert_eq!(cache.get(Uuid::from_u128(2), &clock).unwrap(), Some(2));
    }

    #[test]
    fn a_zero_capacity_is_raised_to_one() {
        assert_eq!(BoundedTtlCache::<u32>::new(0, MAX_TTL).capacity(), 1);
    }

    // ── Fail closed ──────────────────────────────────────────────────────────

    #[test]
    fn a_poisoned_cache_fails_closed_on_every_operation() {
        let clock = TestClock::new(epoch());
        let cache = Arc::new(cache());

        // Poison the mutex: panic while holding the guard.
        let poisoner = Arc::clone(&cache);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.entries.lock().unwrap();
            panic!("deliberate panic to poison the cache lock");
        })
        .join();

        // Every entry point must return Err — never Ok(None), which the
        // authenticator would read as "cache miss, go to the database" and so
        // continue as if nothing were wrong.
        assert_eq!(cache.get(Uuid::nil(), &clock), Err(CacheError));
        assert_eq!(cache.insert(Uuid::nil(), 1, None, &clock), Err(CacheError));
        assert_eq!(cache.invalidate(Uuid::nil()), Err(CacheError));
        assert_eq!(cache.len(), Err(CacheError));
    }
}
