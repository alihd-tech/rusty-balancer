use balancer_config::RateLimitConfig;
use std::{collections::HashMap, net::IpAddr, sync::Mutex, time::{Duration, Instant}};

struct Bucket { tokens: f64, at: Instant }
pub struct RateLimiter {
    config: RateLimitConfig,
    clients: Mutex<HashMap<IpAddr, Bucket>>,
}
impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self { Self { config, clients: Mutex::new(HashMap::new()) } }
    pub fn admit(&self, peer: IpAddr) -> bool { self.admit_at(crate::canonical_ip(peer), Instant::now()) }
    fn admit_at(&self, peer: IpAddr, now: Instant) -> bool {
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        let cap = self.config.max_clients;
        let burst = f64::from(self.config.burst);
        let rate = f64::from(self.config.requests_per_second);
        if !clients.contains_key(&peer) && clients.len() >= cap {
            // Evict fully refilled, idle buckets only; active clients cannot reset their quota.
            let idle = Duration::from_secs_f64((burst / rate).max(60.0));
            clients.retain(|_, b| now.saturating_duration_since(b.at) < idle);
            if clients.len() >= cap { return false; }
        }
        let b = clients.entry(peer).or_insert(Bucket { tokens: burst, at: now });
        b.tokens = (b.tokens + now.saturating_duration_since(b.at).as_secs_f64() * rate).min(burst);
        b.at = now;
        if b.tokens < 1.0 { false } else { b.tokens -= 1.0; true }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn burst_refill_and_capacity() {
        let r = RateLimiter::new(RateLimitConfig { requests_per_second: 2, burst: 2, max_clients: 1 });
        let ip = "192.0.2.1".parse().unwrap();
        let other = "192.0.2.2".parse().unwrap();
        let now = Instant::now();
        assert!(r.admit_at(ip, now)); assert!(r.admit_at(ip, now));
        assert!(!r.admit_at(ip, now)); assert!(!r.admit_at(other, now));
        assert!(r.admit_at(ip, now + Duration::from_millis(500)));
        assert!(r.admit_at(other, now + Duration::from_secs(61)));
    }
}
