use balancer_config::{Algorithm, PoolConfig};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}};

#[derive(Debug)]
pub struct Endpoint {
    pub origin: String,
    pub weight: u32,
    healthy: AtomicBool,
    active: AtomicUsize,
}

impl Endpoint {
    pub fn is_healthy(&self) -> bool { self.healthy.load(Ordering::Acquire) }
    pub fn set_healthy(&self, value: bool) { self.healthy.store(value, Ordering::Release); }
    pub fn active(&self) -> usize { self.active.load(Ordering::Relaxed) }
}

#[derive(Debug)]
pub struct Lease {
    endpoint: Arc<Endpoint>,
}
impl Lease {
    pub fn endpoint(&self) -> &Endpoint { &self.endpoint }
}
impl Drop for Lease {
    fn drop(&mut self) { self.endpoint.active.fetch_sub(1, Ordering::Relaxed); }
}

pub struct Pool {
    algorithm: Algorithm,
    endpoints: Vec<Arc<Endpoint>>,
    cursor: AtomicUsize,
    selection: Mutex<()>,
    ring: Vec<(u64, usize)>,
}

fn hash(value: &[u8]) -> u64 {
    value.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

// FNV-1a followed by an avalanche mixer to distribute similar virtual node names.
fn ring_hash(value: &[u8]) -> u64 {
    let mut h = hash(value);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ceb9fe1a85ec53);
    h ^ (h >> 33)
}

impl Pool {
    pub fn new(config: &PoolConfig) -> Self {
        let endpoints: Vec<_> = config.endpoints.iter().map(|e| Arc::new(Endpoint {
            origin: e.url.clone(), weight: e.weight,
            healthy: AtomicBool::new(true), active: AtomicUsize::new(0),
        })).collect();
        let mut ring = Vec::new();
        if config.algorithm == Algorithm::ConsistentHash {
            for (i, e) in endpoints.iter().enumerate() {
                for replica in 0..(e.weight * 64) {
                    ring.push((ring_hash(format!("{}#{replica}", e.origin).as_bytes()), i));
                }
            }
            ring.sort_unstable();
        }
        Self { algorithm: config.algorithm, endpoints, cursor: AtomicUsize::new(0),
            selection: Mutex::new(()), ring }
    }

    pub fn endpoints(&self) -> &[Arc<Endpoint>] { &self.endpoints }

    pub fn acquire(&self, key: &[u8]) -> Option<Lease> {
        self.acquire_excluding(key, &std::collections::HashSet::new())
    }

    pub fn acquire_excluding(&self, key: &[u8], excluded: &std::collections::HashSet<String>) -> Option<Lease> {
        let _guard = self.selection.lock().unwrap_or_else(|e| e.into_inner());
        let candidates: Vec<_> = self.endpoints.iter().enumerate()
            .filter(|(_, e)| e.is_healthy() && !excluded.contains(&e.origin)).collect();
        if candidates.is_empty() { return None; }
        let cursor = self.cursor.fetch_add(1, Ordering::Relaxed);
        let index = match self.algorithm {
            Algorithm::RoundRobin => candidates[cursor % candidates.len()].0,
            Algorithm::WeightedRoundRobin => {
                let total: usize = candidates.iter().map(|(_, e)| e.weight as usize).sum();
                let mut slot = cursor % total;
                let mut chosen = candidates[0].0;
                for (i, e) in &candidates {
                    if slot < e.weight as usize { chosen = *i; break; }
                    slot -= e.weight as usize;
                }
                chosen
            }
            Algorithm::LeastConnections => {
                let start = cursor % candidates.len();
                (0..candidates.len()).map(|i| candidates[(i + start) % candidates.len()])
                    .min_by(|(_, a), (_, b)| {
                        ((a.active() as u128) * (b.weight as u128))
                            .cmp(&((b.active() as u128) * (a.weight as u128)))
                    })?.0
            }
            Algorithm::ConsistentHash => {
                let position = self.ring.partition_point(|(h, _)| *h < ring_hash(key));
                (0..self.ring.len()).map(|offset| self.ring[(position + offset) % self.ring.len()].1)
                    .find(|i| candidates.iter().any(|(j, _)| j == i))?
            }
        };
        let endpoint = Arc::clone(&self.endpoints[index]);
        endpoint.active.fetch_add(1, Ordering::Relaxed);
        Some(Lease { endpoint })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balancer_config::EndpointConfig;
    fn pool(algorithm: Algorithm) -> Pool {
        Pool::new(&PoolConfig { algorithm, health: None, endpoints: vec![
            EndpointConfig { url: "http://a".into(), weight: 1 },
            EndpointConfig { url: "http://b".into(), weight: 3 },
        ] })
    }
    #[test]
    fn round_robin_and_failover() {
        let p = pool(Algorithm::RoundRobin);
        assert_eq!(p.acquire(b"").unwrap().endpoint().origin, "http://a");
        assert_eq!(p.acquire(b"").unwrap().endpoint().origin, "http://b");
        p.endpoints()[0].set_healthy(false);
        assert_eq!(p.acquire(b"").unwrap().endpoint().origin, "http://b");
        p.endpoints()[1].set_healthy(false);
        assert!(p.acquire(b"").is_none());
    }
    #[test]
    fn connection_retry_exclusion_preserves_health() {
        let p = pool(Algorithm::RoundRobin);
        let excluded = std::collections::HashSet::from(["http://a".to_string()]);
        assert_eq!(p.acquire_excluding(b"", &excluded).unwrap().endpoint().origin, "http://b");
        assert!(p.endpoints()[0].is_healthy());
    }
    #[test]
    fn weighted_distribution() {
        let p = pool(Algorithm::WeightedRoundRobin);
        let a = (0..100).filter(|_| p.acquire(b"").unwrap().endpoint().origin == "http://a").count();
        assert_eq!(a, 25);
    }
    #[test]
    fn least_connections_counts_open_leases() {
        let p = pool(Algorithm::LeastConnections);
        let a = p.acquire(b"").unwrap();
        let b = p.acquire(b"").unwrap();
        assert_ne!(a.endpoint().origin, b.endpoint().origin);
        assert_eq!(a.endpoint().active(), 1);
        drop(a);
        assert_eq!(p.endpoints()[0].active(), 0);
    }
    #[test]
    fn hash_affinity_and_failover() {
        let p = pool(Algorithm::ConsistentHash);
        let first = p.acquire(b"client-1").unwrap().endpoint().origin.clone();
        assert_eq!(p.acquire(b"client-1").unwrap().endpoint().origin, first);
        p.endpoints().iter().find(|e| e.origin == first).unwrap().set_healthy(false);
        assert_ne!(p.acquire(b"client-1").unwrap().endpoint().origin, first);
    }
}
