//! Process-local control plane. Each new request snapshots one immutable generation.
use crate::{build_with_permits, Server};
use anyhow::{bail, Result};
use arc_swap::ArcSwap;
use axum::{extract::{Request, State}, response::Response, Router};
use balancer_config::Config;
use balancer_observability::Metrics;
use serde::Serialize;
use std::{sync::{Arc, Mutex, Weak}, fmt::Write};
use tokio::sync::Semaphore;
use tower::ServiceExt;

pub struct Generation {
    pub revision: u64,
    pub config: Config,
    server: Server,
}
struct History { previous: Option<Config>, retired: Weak<Generation> }

pub struct Control {
    current: ArcSwap<Generation>,
    history: Mutex<History>,
    permits: Arc<Semaphore>,
    metrics: Metrics,
}

#[derive(Serialize)]
pub struct EndpointStatus { pub origin: String, pub weight: u32, pub eligible: bool, pub observed: bool, pub active: usize }
#[derive(Serialize)]
pub struct PoolStatus { pub name: String, pub endpoints: Vec<EndpointStatus> }
#[derive(Serialize)]
pub struct RouteStatus { pub index: usize, pub host: Option<String>, pub path_prefix: String, pub pool: Option<String>, pub static_files: bool, pub cache: Option<balancer_middleware::cache::CacheStats> }
#[derive(Serialize)]
pub struct Snapshot { pub revision: u64, pub ready: bool, pub in_flight: usize, pub pools: Vec<PoolStatus>, pub routes: Vec<RouteStatus> }

impl Control {
    pub fn new(config: Config) -> Arc<Self> {
        let permits = Arc::new(Semaphore::new(config.traffic.max_in_flight));
        let server = build_with_permits(config.clone(), permits.clone());
        Arc::new(Self { current: ArcSwap::from_pointee(Generation { revision: 1, config, server }),
            history: Mutex::new(History { previous: None, retired: Weak::new() }), permits, metrics: Metrics::default() })
    }
    pub fn current(&self) -> Arc<Generation> { self.current.load_full() }
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new().fallback(gateway).with_state(self.clone())
    }
    pub fn apply(&self, expected: u64, mut next: Config) -> Result<u64> {
        next.validate()?;
        let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        self.swap(expected, next, &mut history)
    }
    pub fn rollback(&self, expected: u64) -> Result<u64> {
        let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let Some(next) = history.previous.clone() else { bail!("no previous configuration"); };
        self.swap(expected, next, &mut history)
    }
    fn swap(&self, expected: u64, next: Config, history: &mut History) -> Result<u64> {
        let old = self.current();
        if expected != old.revision { bail!("revision conflict"); }
        if history.retired.upgrade().is_some() { bail!("previous generation still draining"); }
        let prior = &old.config;
        if next.listen != prior.listen || next.tls != prior.tls || next.admin != prior.admin ||
            next.traffic.max_in_flight != prior.traffic.max_in_flight ||
            next.traffic.max_connections != prior.traffic.max_connections || next.transport != prior.transport || next.http3 != prior.http3 {
            bail!("listener, TLS, transport, admin identity and global concurrency limit require restart");
        }
        let revision = old.revision.checked_add(1).ok_or_else(|| anyhow::anyhow!("revision exhausted"))?;
        let server = build_with_permits(next.clone(), self.permits.clone());
        self.current.store(Arc::new(Generation { revision, config: next, server }));
        history.previous = Some(prior.clone());
        history.retired = Arc::downgrade(&old);
        tracing::info!(revision, previous_revision=expected, "configuration generation activated");
        Ok(revision)
    }
    pub fn snapshot(&self) -> Snapshot {
        let generation = self.current();
        let r = &generation.server.runtime;
        let pools = r.pools.iter().map(|(name,pool)| PoolStatus { name: name.clone(),
            endpoints: pool.endpoints().iter().map(|e| EndpointStatus { origin: e.origin.clone(),
                weight: e.weight, eligible: e.is_healthy(), observed: e.has_observation(), active: e.active() }).collect() }).collect();
        let routes = r.routes.iter().enumerate().map(|(index, route)| RouteStatus {
            index, host: route.host.clone(), path_prefix: route.path_prefix.clone(), pool: route.pool.clone(),
            static_files: route.static_dir.is_some(), cache: r.policies[index].cache.as_ref().map(|c| c.stats()),
        }).collect();
        let ready = r.routes.iter().all(|route| route.pool.as_ref().map_or(true, |name|
            r.pools[name].endpoints().iter().any(|e| e.is_ready())));
        Snapshot { revision: generation.revision, ready,
            in_flight: generation.config.traffic.max_in_flight.saturating_sub(self.permits.available_permits()), pools, routes }
    }
    pub fn purge(&self, expected: u64, route: Option<usize>) -> Result<usize> {
        let _history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let generation = self.current();
        if expected != generation.revision { bail!("revision conflict"); }
        let policies = &generation.server.runtime.policies;
        if let Some(i) = route {
            let cache = policies.get(i).and_then(|p| p.cache.as_ref()).ok_or_else(|| anyhow::anyhow!("route has no cache"))?;
            return Ok(cache.purge());
        }
        Ok(policies.iter().filter_map(|p| p.cache.as_ref()).map(|c| c.purge()).sum())
    }
    pub fn prometheus(&self) -> String {
        let mut out = self.metrics.render();
        let s = self.snapshot();
        let _ = writeln!(out, "# TYPE rusty_balancer_config_revision gauge\nrusty_balancer_config_revision {}\n# TYPE rusty_balancer_in_flight gauge\nrusty_balancer_in_flight {}\n# TYPE rusty_balancer_ready gauge\nrusty_balancer_ready {}", s.revision, s.in_flight, u8::from(s.ready));
        let _ = writeln!(out, "# TYPE rusty_balancer_upstream_eligible gauge\n# TYPE rusty_balancer_upstream_active gauge");
        for pool in s.pools {
            for endpoint in pool.endpoints {
                let labels = format!("pool=\"{}\",origin=\"{}\"", escape(&pool.name), escape(&endpoint.origin));
                let _ = writeln!(out, "rusty_balancer_upstream_eligible{{{labels}}} {}\nrusty_balancer_upstream_active{{{labels}}} {}", u8::from(endpoint.eligible), endpoint.active);
            }
        }
        let _ = writeln!(out, "# TYPE rusty_balancer_cache_entries gauge\n# TYPE rusty_balancer_cache_body_bytes gauge\n# TYPE rusty_balancer_cache_generation_hits gauge\n# TYPE rusty_balancer_cache_generation_misses gauge");
        for route in s.routes {
            if let Some(c) = route.cache {
                let labels = format!("route=\"{}\"", route.index);
                let _ = writeln!(out, "rusty_balancer_cache_entries{{{labels}}} {}\nrusty_balancer_cache_body_bytes{{{labels}}} {}\nrusty_balancer_cache_generation_hits{{{labels}}} {}\nrusty_balancer_cache_generation_misses{{{labels}}} {}", c.entries,c.body_bytes,c.hits,c.misses);
            }
        }
        out
    }
}
fn escape(value: &str) -> String { value.replace('\\', "\\\\").replace('\n', "\\n").replace('"', "\\\"") }

async fn gateway(State(control): State<Arc<Control>>, request: Request) -> Response {
    let generation = control.current();
    let revision = generation.revision;
    let method = request.method().clone();
    let obs = control.metrics.begin();
    let response = generation.server.router.clone().oneshot(request).await.expect("infallible router");
    let status = response.status().as_u16();
    let elapsed = obs.finish(status);
    // No request path, query, credentials, cookies or remote IP in default access logs.
    tracing::info!(revision, %method, status, elapsed_ms=elapsed.as_secs_f64()*1000.0, "request");
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::{Body,to_bytes}, extract::ConnectInfo};
    use std::net::SocketAddr;
    #[tokio::test]
    async fn revision_conflicts_invalid_changes_and_rollback() {
        let cfg = Config::parse("listen = \"127.0.0.1:8080\"\n[[routes]]\nstatic_dir = \"public\"").unwrap();
        let control = Control::new(cfg.clone());
        let mut changed = cfg.clone();
        changed.routes[0].path_prefix = "/new".into();
        assert_eq!(control.apply(1, changed).unwrap(), 2);
        assert!(control.apply(1, cfg.clone()).is_err());
        let mut invalid = cfg.clone();
        invalid.listen.set_port(9999);
        assert!(control.apply(2, invalid).is_err());
        assert_eq!(control.current().revision, 2);
        assert_eq!(control.rollback(2).unwrap(), 3);
        let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo("127.0.0.1:9000".parse::<SocketAddr>().unwrap()));
        let result = control.router().oneshot(req).await.unwrap();
        let _ = to_bytes(result.into_body(), 1024).await.unwrap();
        assert!(control.prometheus().contains("rusty_balancer_requests_total 1"));
        assert_eq!(control.snapshot().revision, 3);
    }
    #[tokio::test]
    async fn readiness_waits_for_observed_upstream_health() {
        let cfg = Config::parse("listen = \"127.0.0.1:8080\"\n[pools.app]\n[[pools.app.endpoints]]\nurl = \"http://127.0.0.1:9001\"\n[[routes]]\npool = \"app\"").unwrap();
        let control = Control::new(cfg);
        assert!(!control.snapshot().ready);
        control.current().server.runtime.pools["app"].endpoints()[0].observe_success();
        assert!(control.snapshot().ready);
    }
    #[tokio::test]
    async fn shared_admission_is_preserved_across_reload() {
        let cfg = Config::parse("listen = \"127.0.0.1:8080\"\n[traffic]\nmax_in_flight = 1\n[[routes]]\nstatic_dir = \"public\"").unwrap();
        let control = Control::new(cfg.clone());
        let permit = control.permits.clone().try_acquire_owned().unwrap();
        control.apply(1, cfg).unwrap();
        assert_eq!(control.snapshot().in_flight, 1);
        assert!(control.current().server.runtime.in_flight.clone().try_acquire_owned().is_err());
        drop(permit);
        assert_eq!(control.snapshot().in_flight, 0);
    }

    #[tokio::test]
    async fn protects_draining_generation() {
        let cfg = Config::parse("listen = \"127.0.0.1:8080\"\n[[routes]]\nstatic_dir = \"public\"").unwrap();
        let control = Control::new(cfg.clone());
        let old = control.current();
        control.apply(1, cfg.clone()).unwrap();
        assert!(control.apply(2, cfg.clone()).is_err());
        drop(old);
        assert_eq!(control.apply(2, cfg).unwrap(), 3);
    }
}
