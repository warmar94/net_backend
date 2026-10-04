//! Rate limiting: the [`RateLimiter`] seam and a ready in-memory implementation.
//!
//! Every installed limiter (the app's from [`NetBackendServer::rate_limiter`], then the modules',
//! e.g. the auth module's login buckets) is asked **twice** per routed request:
//!
//! 1. [`RateLimitStage::BeforeAuth`], before the authenticator runs (`user: None`): limit by client
//!    address and route here, so failed token guesses and the authenticator's own lookups are
//!    throttled too;
//! 2. [`RateLimitStage::AfterAuth`], after authentication, with the user when there is one: limit
//!    per user here.
//!
//! A limiter counts in the stage(s) it cares about and allows the other. The first refusal answers
//! 429 `rate_limited` with `{"retry_after_ms":N}` and a `Retry-After` header.
//!
//! **Client address:** [`RateLimitKey::ip`] is the [`ClientIp`](crate::http::ClientIp): the
//! connection's peer, or, when the peer is one of `http.trusted_proxies` (e.g. Caddy on the same
//! machine), the address it reports in `X-Forwarded-For`. Never configure a proxy as trusted that
//! passes on a client's own `X-Forwarded-For` unchecked.
//!
//! [`MemoryRateLimiter`] is a set of [`RateRule`]s over [`KeyedBuckets`] (token buckets per key in a
//! bounded map): enough for one server process. Its state is lost on restart and not shared between
//! instances.
//!
//! [`NetBackendServer::rate_limiter`]: crate::NetBackendServer::rate_limiter

use std::collections::HashMap;
use std::hash::Hash;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use net_backend_protocol::UserId;

/// When a [`RateLimiter`] is asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RateLimitStage {
    /// Before authentication: `user` is always `None`.
    BeforeAuth,
    /// After authentication: `user` is set when the request is authenticated.
    AfterAuth,
}

/// What a request is limited by.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RateLimitKey {
    /// The client address ([`ClientIp`](crate::http::ClientIp)); `None` if unknown.
    pub ip: Option<IpAddr>,
    /// The matched route pattern (`/v1/storage/{collection}`), never the raw path.
    pub route: Option<String>,
    /// The authenticated user, if any (always `None` in [`RateLimitStage::BeforeAuth`]).
    pub user: Option<UserId>,
    /// Which of the two checks this is.
    pub stage: RateLimitStage,
}

impl RateLimitKey {
    /// A key (for tests of a limiter).
    pub fn new(ip: Option<IpAddr>, route: Option<String>, user: Option<UserId>, stage: RateLimitStage) -> Self {
        Self { ip, route, user, stage }
    }
}

/// The answer of a [`RateLimiter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RateDecision {
    /// Let the request through.
    Allow,
    /// Refuse it; the client may retry after this many milliseconds.
    Deny {
        /// Milliseconds until a retry may succeed.
        retry_after_ms: u64,
    },
}

impl RateDecision {
    /// Whether this is [`RateDecision::Allow`].
    pub fn is_allow(self) -> bool {
        matches!(self, RateDecision::Allow)
    }
}

/// Decides whether a request may proceed. Must be fast and non-blocking (a token bucket in
/// memory, e.g. [`MemoryRateLimiter`]).
pub trait RateLimiter: Send + Sync + 'static {
    /// Check (and count) one request at one stage.
    fn check(&self, key: &RateLimitKey) -> RateDecision;
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    last_ms: u64,
}

/// Token buckets per key: each key may spend `burst` requests at once, and earns one back every
/// `period / burst`. The map holds at most `max_keys` keys: when it is full, buckets that refilled
/// completely are dropped first (they hold no state), then the least recently used half.
///
/// ```
/// use std::time::Duration;
/// use net_backend_server::rate_limit::KeyedBuckets;
///
/// let buckets = KeyedBuckets::new(3, Duration::from_secs(60), 10_000);
/// assert!(buckets.check("ada").is_allow());
/// ```
#[derive(Debug)]
pub struct KeyedBuckets<K> {
    burst: f64,
    refill_per_ms: f64,
    max_keys: usize,
    origin: Instant,
    map: Mutex<HashMap<K, Bucket>>,
}

impl<K: Hash + Eq + Clone> KeyedBuckets<K> {
    /// `burst` requests per `period` per key (both at least 1), at most `max_keys` keys (at least 16).
    pub fn new(burst: u32, period: Duration, max_keys: usize) -> Self {
        let burst = f64::from(burst.max(1));
        let period_ms = (period.as_millis().max(1)) as f64;
        Self { burst, refill_per_ms: burst / period_ms, max_keys: max_keys.max(16), origin: Instant::now(), map: Mutex::new(HashMap::new()) }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Take one token for `key` (now).
    pub fn check(&self, key: K) -> RateDecision {
        self.check_at(key, self.now_ms())
    }

    /// Whether `key` has a token left, without taking it (now).
    pub fn peek(&self, key: &K) -> RateDecision {
        self.peek_at(key, self.now_ms())
    }

    /// Give back one token taken for `key` (e.g. a failure counted up front that did not happen).
    pub fn refund(&self, key: &K) {
        self.refund_at(key, self.now_ms());
    }

    /// [`refund`](Self::refund) at an explicit time in milliseconds (tests).
    pub fn refund_at(&self, key: &K, now_ms: u64) {
        let mut map = self.lock();
        if let Some(bucket) = map.get_mut(key) {
            let tokens = self.refilled(*bucket, now_ms) + 1.0;
            if tokens >= self.burst {
                map.remove(key);
            } else {
                *bucket = Bucket { tokens, last_ms: now_ms.max(bucket.last_ms) };
            }
        }
    }

    /// Forget `key` (its bucket is full again).
    pub fn reset(&self, key: &K) {
        self.lock().remove(key);
    }

    /// The number of keys held.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no key is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<K, Bucket>> {
        self.map.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn refilled(&self, bucket: Bucket, now_ms: u64) -> f64 {
        let elapsed = now_ms.saturating_sub(bucket.last_ms) as f64;
        (bucket.tokens + elapsed * self.refill_per_ms).min(self.burst)
    }

    fn deny(&self, tokens: f64) -> RateDecision {
        let missing = (1.0 - tokens).max(0.0);
        let ms = (missing / self.refill_per_ms).ceil();
        RateDecision::Deny { retry_after_ms: if ms.is_finite() && ms >= 1.0 { ms as u64 } else { 1 } }
    }

    /// [`peek`](Self::peek) at an explicit time in milliseconds (tests).
    pub fn peek_at(&self, key: &K, now_ms: u64) -> RateDecision {
        let map = self.lock();
        match map.get(key) {
            Some(bucket) => {
                let tokens = self.refilled(*bucket, now_ms);
                if tokens >= 1.0 {
                    RateDecision::Allow
                } else {
                    self.deny(tokens)
                }
            }
            None => RateDecision::Allow,
        }
    }

    /// [`check`](Self::check) at an explicit time in milliseconds (tests).
    pub fn check_at(&self, key: K, now_ms: u64) -> RateDecision {
        let mut map = self.lock();
        if !map.contains_key(&key) && map.len() >= self.max_keys {
            self.evict(&mut map, now_ms);
        }
        let bucket = map.entry(key).or_insert(Bucket { tokens: self.burst, last_ms: now_ms });
        let tokens = self.refilled(*bucket, now_ms);
        bucket.last_ms = now_ms.max(bucket.last_ms);
        if tokens >= 1.0 {
            bucket.tokens = tokens - 1.0;
            RateDecision::Allow
        } else {
            bucket.tokens = tokens;
            self.deny(tokens)
        }
    }

    fn evict(&self, map: &mut HashMap<K, Bucket>, now_ms: u64) {
        map.retain(|_, bucket| self.refilled(*bucket, now_ms) < self.burst);
        if map.len() >= self.max_keys {
            let mut ages: Vec<u64> = map.values().map(|b| b.last_ms).collect();
            ages.sort_unstable();
            let median = ages.get(ages.len() / 2).copied().unwrap_or(0);
            map.retain(|_, bucket| bucket.last_ms > median);
        }
    }
}

/// The default IPv6 prefix a client address is limited by: one /64 (a home line or a VPS usually
/// holds a whole /64, so counting single IPv6 addresses would let one client rotate freely).
pub const DEFAULT_IPV6_PREFIX: u8 = 64;

/// The key a client address is counted under: IPv4 as it is, IPv6 cut to its first `v6_prefix`
/// bits (an IPv4-mapped IPv6 address counts as the IPv4 address).
pub fn ip_key(ip: IpAddr, v6_prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let bits = u32::from(v6_prefix.min(128));
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            IpAddr::V6(Ipv6Addr::from(u128::from(v6) & mask))
        }
    }
}

/// Keys remembered for a time (`ttl`), at most `max_keys` (when full, expired keys go first, then
/// the oldest half). For "seen recently" sets: known login networks, used tickets.
#[derive(Debug)]
pub struct RecentSet<K> {
    ttl_ms: u64,
    max_keys: usize,
    origin: Instant,
    map: Mutex<HashMap<K, u64>>,
}

impl<K: Hash + Eq + Clone> RecentSet<K> {
    /// A set remembering keys for `ttl`, at most `max_keys` (at least 16).
    pub fn new(ttl: Duration, max_keys: usize) -> Self {
        Self { ttl_ms: u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX), max_keys: max_keys.max(16), origin: Instant::now(), map: Mutex::new(HashMap::new()) }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<K, u64>> {
        self.map.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Remember `key` (now); true if it was not remembered before (or had expired).
    pub fn insert(&self, key: K) -> bool {
        self.insert_at(key, self.now_ms())
    }

    /// Whether `key` is remembered and not expired (now).
    pub fn contains(&self, key: &K) -> bool {
        self.contains_at(key, self.now_ms())
    }

    /// [`insert`](Self::insert) at an explicit time in milliseconds (tests).
    pub fn insert_at(&self, key: K, now_ms: u64) -> bool {
        let mut map = self.lock();
        if !map.contains_key(&key) && map.len() >= self.max_keys {
            let ttl = self.ttl_ms;
            map.retain(|_, at| now_ms.saturating_sub(*at) < ttl);
            if map.len() >= self.max_keys {
                let mut ages: Vec<u64> = map.values().copied().collect();
                ages.sort_unstable();
                let median = ages.get(ages.len() / 2).copied().unwrap_or(0);
                map.retain(|_, at| *at > median);
            }
        }
        match map.insert(key, now_ms) {
            Some(at) => now_ms.saturating_sub(at) >= self.ttl_ms,
            None => true,
        }
    }

    /// [`contains`](Self::contains) at an explicit time in milliseconds (tests).
    pub fn contains_at(&self, key: &K, now_ms: u64) -> bool {
        self.lock().get(key).is_some_and(|at| now_ms.saturating_sub(*at) < self.ttl_ms)
    }
}

/// What a [`RateRule`] counts by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RateScope {
    /// The client address (IPv6 by its /64 or the configured prefix), before authentication
    /// (requests without a known address pass).
    Ip,
    /// The authenticated user, after authentication (anonymous requests pass).
    User,
}

/// One limit of a [`MemoryRateLimiter`]: `burst` requests per `period`, per client address or per
/// user, on some routes (each route counted separately) or on every route.
#[derive(Clone, Debug)]
pub struct RateRule {
    scope: RateScope,
    burst: u32,
    period: Duration,
    routes: Vec<String>,
}

impl RateRule {
    /// `burst` requests per `period` per client address.
    pub fn per_ip(burst: u32, period: Duration) -> Self {
        Self { scope: RateScope::Ip, burst, period, routes: Vec::new() }
    }

    /// `burst` requests per `period` per authenticated user.
    pub fn per_user(burst: u32, period: Duration) -> Self {
        Self { scope: RateScope::User, burst, period, routes: Vec::new() }
    }

    /// Only these route patterns (as registered, e.g. `/v1/auth/login`); each has its own
    /// buckets. Without routes the rule covers every route (one bucket per address or user).
    pub fn routes<I, S>(mut self, routes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.routes = routes.into_iter().map(Into::into).collect();
        self
    }

    /// What it counts by.
    pub fn scope(&self) -> RateScope {
        self.scope
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Subject {
    Ip(IpAddr),
    User(i64),
}

/// A [`RateLimiter`] made of [`RateRule`]s, each with its own [`KeyedBuckets`].
///
/// ```
/// use std::time::Duration;
/// use net_backend_server::rate_limit::{MemoryRateLimiter, RateRule};
///
/// let limiter = MemoryRateLimiter::new()
///     .rule(RateRule::per_ip(300, Duration::from_secs(60)))                    // every route
///     .rule(RateRule::per_user(30, Duration::from_secs(60)).routes(["/v1/game/craft"]));
/// // NetBackendServer::new(config).rate_limiter(limiter)
/// # let _ = limiter;
/// ```
#[derive(Debug)]
pub struct MemoryRateLimiter {
    rules: Vec<(RateRule, KeyedBuckets<(usize, Subject)>)>,
    max_keys: usize,
    v6_prefix: u8,
}

impl Default for MemoryRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryRateLimiter {
    /// No rules yet; at most 100 000 keys per rule.
    pub fn new() -> Self {
        Self { rules: Vec::new(), max_keys: 100_000, v6_prefix: DEFAULT_IPV6_PREFIX }
    }

    /// Count IPv6 clients by this prefix (default 64; 128 = single addresses).
    pub fn ipv6_prefix(mut self, prefix: u8) -> Self {
        self.v6_prefix = prefix.min(128);
        self
    }

    /// The most keys each rule keeps (later rules only; at least 16).
    pub fn max_keys(mut self, max_keys: usize) -> Self {
        self.max_keys = max_keys;
        self
    }

    /// Add a rule.
    pub fn rule(mut self, rule: RateRule) -> Self {
        let buckets = KeyedBuckets::new(rule.burst, rule.period, self.max_keys);
        self.rules.push((rule, buckets));
        self
    }
}

impl RateLimiter for MemoryRateLimiter {
    fn check(&self, key: &RateLimitKey) -> RateDecision {
        for (rule, buckets) in &self.rules {
            let subject = match (rule.scope, key.stage) {
                (RateScope::Ip, RateLimitStage::BeforeAuth) => key.ip.map(|ip| Subject::Ip(ip_key(ip, self.v6_prefix))),
                (RateScope::User, RateLimitStage::AfterAuth) => key.user.map(|u| Subject::User(u.get())),
                _ => None,
            };
            let Some(subject) = subject else { continue };
            let route_index = if rule.routes.is_empty() {
                0
            } else {
                match key.route.as_deref().and_then(|route| rule.routes.iter().position(|r| r == route)) {
                    Some(index) => index + 1,
                    None => continue,
                }
            };
            let decision = buckets.check((route_index, subject));
            if !decision.is_allow() {
                return decision;
            }
        }
        RateDecision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_refill_and_say_when() {
        let buckets = KeyedBuckets::new(2, Duration::from_secs(10), 100);
        assert!(buckets.check_at("a", 0).is_allow());
        assert!(buckets.check_at("a", 0).is_allow());
        assert_eq!(buckets.check_at("a", 0), RateDecision::Deny { retry_after_ms: 5000 });
        assert_eq!(buckets.peek_at(&"a", 4000), RateDecision::Deny { retry_after_ms: 1000 });
        assert!(buckets.peek_at(&"a", 5000).is_allow());
        assert!(buckets.check_at("b", 0).is_allow(), "keys are independent");
        assert!(buckets.check_at("a", 5000).is_allow());
        buckets.reset(&"a");
        assert!(buckets.check_at("a", 5000).is_allow());
        assert!(buckets.check_at("a", 5000).is_allow());
        // A refunded token is back; a full bucket is forgotten.
        assert!(!buckets.peek_at(&"a", 5000).is_allow());
        buckets.refund_at(&"a", 5000);
        assert!(buckets.peek_at(&"a", 5000).is_allow());
        let held = buckets.len();
        buckets.refund_at(&"a", 5000);
        assert_eq!(buckets.len(), held - 1, "full again: forgotten");
        buckets.refund_at(&"never", 0);
    }

    #[test]
    fn memory_is_bounded() {
        let buckets = KeyedBuckets::new(1, Duration::from_secs(3600), 16);
        for n in 0..1000u32 {
            let _ = buckets.check_at(n, u64::from(n));
            assert!(buckets.len() <= 16, "{}", buckets.len());
        }
        // The newest keys keep their state.
        assert!(!buckets.check_at(999, 1000).is_allow());
        // Full buckets are dropped first: with a fast refill, old keys are evicted as "full".
        let fast = KeyedBuckets::new(1, Duration::from_millis(1), 16);
        for n in 0..100u32 {
            let _ = fast.check_at(n, u64::from(n) * 10);
        }
        assert!(fast.len() <= 16);
    }

    #[test]
    fn ipv6_clients_count_by_prefix() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap_or(IpAddr::from([0, 0, 0, 0]));
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap_or(IpAddr::from([0, 0, 0, 0]));
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap_or(IpAddr::from([0, 0, 0, 0]));
        assert_eq!(ip_key(a, 64), ip_key(b, 64));
        assert_ne!(ip_key(a, 64), ip_key(c, 64));
        assert_ne!(ip_key(a, 128), ip_key(b, 128));
        assert_eq!(ip_key("::ffff:203.0.113.9".parse().unwrap_or(a), 64), IpAddr::from([203, 0, 113, 9]));
        let limiter = MemoryRateLimiter::new().rule(RateRule::per_ip(1, Duration::from_secs(60)));
        let key = |ip| RateLimitKey::new(Some(ip), None, None, RateLimitStage::BeforeAuth);
        assert!(limiter.check(&key(a)).is_allow());
        assert!(!limiter.check(&key(b)).is_allow(), "the same /64 shares a bucket");
        assert!(limiter.check(&key(c)).is_allow());
    }

    #[test]
    fn recent_sets_expire_and_stay_bounded() {
        let set = RecentSet::new(Duration::from_millis(100), 16);
        assert!(set.insert_at("a", 0));
        assert!(!set.insert_at("a", 50), "still remembered");
        assert!(set.contains_at(&"a", 99) && !set.contains_at(&"a", 200));
        assert!(set.insert_at("a", 300), "expired: new again");
        for n in 0..100u64 {
            set.insert_at(format!("k{n}").leak() as &str, 1000 + n);
        }
        assert!(set.lock().len() <= 16);
    }

    #[test]
    fn rules_by_scope_and_route() {
        let ip: IpAddr = [203, 0, 113, 9].into();
        let limiter = MemoryRateLimiter::new()
            .rule(RateRule::per_ip(1, Duration::from_secs(60)).routes(["/v1/auth/login"]))
            .rule(RateRule::per_user(2, Duration::from_secs(60)));
        let login = |stage, user| RateLimitKey::new(Some(ip), Some("/v1/auth/login".into()), user, stage);
        assert!(limiter.check(&login(RateLimitStage::BeforeAuth, None)).is_allow());
        assert!(!limiter.check(&login(RateLimitStage::BeforeAuth, None)).is_allow());
        // Another route and an unknown address are not covered by the ip rule.
        assert!(limiter.check(&RateLimitKey::new(Some(ip), Some("/v1/info".into()), None, RateLimitStage::BeforeAuth)).is_allow());
        assert!(limiter.check(&RateLimitKey::new(None, Some("/v1/auth/login".into()), None, RateLimitStage::BeforeAuth)).is_allow());
        // The user rule counts after authentication only.
        let user = Some(UserId(7));
        let other = RateLimitKey::new(Some(ip), Some("/v1/info".into()), user, RateLimitStage::AfterAuth);
        assert!(limiter.check(&other).is_allow());
        assert!(limiter.check(&other).is_allow());
        assert!(!limiter.check(&other).is_allow());
        assert!(limiter.check(&RateLimitKey::new(Some(ip), None, Some(UserId(8)), RateLimitStage::AfterAuth)).is_allow());
    }
}
