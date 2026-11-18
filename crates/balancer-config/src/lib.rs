use anyhow::{bail, Context, Result};
use http::{uri::Authority, Uri};
use serde::Deserialize;
use std::{collections::{BTreeMap, HashSet}, net::SocketAddr, path::{Path, PathBuf}};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub traffic: TrafficConfig,
    #[serde(default = "timeout_default")]
    pub upstream_timeout_ms: u64,
    #[serde(default)]
    pub pools: BTreeMap<String, PoolConfig>,
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub listen: SocketAddr,
    #[serde(default)]
    pub redirect_http: bool,
    pub public_port: Option<u16>,
    #[serde(default)]
    pub certificates: Vec<CertificateConfig>,
    pub acme: Option<AcmeConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertificateConfig {
    pub domains: Vec<String>,
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcmeConfig {
    pub domains: Vec<String>,
    pub email: String,
    pub cache_dir: PathBuf,
    // Staging is deliberately the default.
    #[serde(default)]
    pub production: bool,
}

pub fn is_dns_name(name: &str) -> bool {
    name.len() <= 253 && name.parse::<std::net::IpAddr>().is_err() &&
        name.split('.').all(|label| !label.is_empty() && label.len() <= 63 &&
            !label.starts_with('-') && !label.ends_with('-') &&
            label.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'))
}

impl TlsConfig {
    pub fn domains(&self) -> Vec<String> {
        if let Some(acme) = &self.acme { return acme.domains.clone(); }
        self.certificates.iter().flat_map(|cert| cert.domains.clone()).collect()
    }

    pub fn external_port(&self) -> u16 {
        self.public_port.unwrap_or(self.listen.port())
    }

    fn validate(&mut self, http: SocketAddr) -> Result<()> {
        if self.listen.port() == 0 || self.external_port() == 0 || self.listen == http {
            bail!("TLS requires a distinct listener and nonzero ports");
        }
        if self.certificates.is_empty() == self.acme.is_none() {
            bail!("TLS requires exactly one mode: manual certificates or ACME");
        }
        let mut seen = HashSet::new();
        let mut normalize = |domains: &mut Vec<String>| -> Result<()> {
            if domains.is_empty() { bail!("TLS certificate requires at least one DNS name"); }
            for name in domains {
                *name = name.to_ascii_lowercase();
                if !is_dns_name(name) || !seen.insert(name.clone()) {
                    bail!("invalid or duplicate TLS DNS name: {name}");
                }
            }
            Ok(())
        };
        for cert in &mut self.certificates {
            normalize(&mut cert.domains)?;
            if cert.cert.as_os_str().is_empty() || cert.key.as_os_str().is_empty() {
                bail!("certificate and private key paths are required");
            }
        }
        if let Some(acme) = &mut self.acme {
            normalize(&mut acme.domains)?;
            if acme.domains.len() > 100 || acme.domains.iter().any(|d| !d.contains('.')) {
                bail!("ACME requires 1..=100 fully qualified DNS names");
            }
            if !acme.email.contains('@') || acme.email.chars().any(char::is_whitespace) ||
                acme.cache_dir.as_os_str().is_empty() {
                bail!("ACME requires a contact email and private cache directory");
            }
            if self.external_port() != 443 { bail!("TLS-ALPN-01 requires external port 443"); }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Algorithm {
    #[default]
    RoundRobin,
    WeightedRoundRobin,
    LeastConnections,
    ConsistentHash,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolConfig {
    #[serde(default)]
    pub algorithm: Algorithm,
    pub endpoints: Vec<EndpointConfig>,
    pub health: Option<HealthConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    pub url: String,
    #[serde(default = "one")]
    pub weight: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthConfig {
    pub path: String,
    #[serde(default = "interval_default")]
    pub interval_ms: u64,
    #[serde(default = "timeout_default")]
    pub timeout_ms: u64,
    #[serde(default = "one")]
    pub healthy_threshold: u32,
    #[serde(default = "one")]
    pub unhealthy_threshold: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub host: Option<String>,
    #[serde(default = "root")]
    pub path_prefix: String,
    pub pool: Option<String>,
    pub static_dir: Option<PathBuf>,
    #[serde(default)]
    pub policy: RoutePolicy,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TrafficConfig {
    pub compression: bool,
    pub security_headers: bool,
    pub max_in_flight: usize,
    pub response_idle_ms: u64,
}
impl Default for TrafficConfig {
    fn default() -> Self {
        Self { compression: false, security_headers: false, max_in_flight: 1024, response_idle_ms: 30000 }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutePolicy {
    pub allow: Vec<ipnet::IpNet>,
    pub deny: Vec<ipnet::IpNet>,
    pub max_body_bytes: Option<usize>,
    pub body_timeout_ms: Option<u64>,
    pub rate_limit: Option<RateLimitConfig>,
    pub cache: Option<CacheConfig>,
    pub websocket: bool,
    pub websocket_max_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    pub requests_per_second: u32,
    pub burst: u32,
    pub max_clients: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    pub ttl_seconds: u64,
    pub max_entries: usize,
    pub max_body_bytes: usize,
}

impl RoutePolicy {
    fn validate(&self, proxy: bool) -> Result<()> {
        if self.allow.len() + self.deny.len() > 1024 { bail!("too many IP rules"); }
        if self.max_body_bytes.is_some_and(|n| n == 0 || n > 16 * 1024 * 1024) ||
            self.body_timeout_ms.is_some_and(|n| n == 0 || n > 300_000) ||
            (self.body_timeout_ms.is_some() && self.max_body_bytes.is_none()) {
            bail!("body policy requires max_body_bytes 1..=16777216 and optional timeout 1..=300000");
        }
        if let Some(rate) = &self.rate_limit {
            if !(1..=100_000).contains(&rate.requests_per_second) ||
                !(1..=100_000).contains(&rate.burst) || !(1..=100_000).contains(&rate.max_clients) {
                bail!("invalid rate limit bounds");
            }
        }
        if let Some(cache) = &self.cache {
            if !proxy || !(1..=86400).contains(&cache.ttl_seconds) ||
                !(1..=4096).contains(&cache.max_entries) ||
                !(1..=1024*1024).contains(&cache.max_body_bytes) ||
                cache.max_entries.saturating_mul(cache.max_body_bytes + 16384) > 64 * 1024 * 1024 {
                bail!("cache requires proxy route and bounded capacity (64 MiB per route)");
            }
        }
        if self.websocket && !proxy { bail!("WebSocket requires a proxy route"); }
        if self.websocket_max_seconds.is_some_and(|n| n == 0 || n > 86400) ||
            (self.websocket_max_seconds.is_some() && !self.websocket) {
            bail!("invalid WebSocket lifetime policy");
        }
        Ok(())
    }
}

fn root() -> String { "/".into() }
fn one() -> u32 { 1 }
fn timeout_default() -> u64 { 5_000 }
fn interval_default() -> u64 { 10_000 }

pub fn hostname(value: &str) -> Option<String> {
    if value.contains('@') { return None; }
    let authority: Authority = value.parse().ok()?;
    let host = authority.host().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() { None } else { Some(host) }
}

pub fn path_matches(prefix: &str, path: &str) -> bool {
    prefix == "/" || path == prefix ||
        path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
}

fn valid_path(value: &str) -> bool {
    value.starts_with('/') && !value.starts_with("//") &&
        !value.contains('?') && !value.contains('#') &&
        value.parse::<http::uri::PathAndQuery>().is_ok()
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?;
        Self::parse(&content)
    }

    pub fn parse(content: &str) -> Result<Self> {
        let mut config: Self = toml::from_str(content).context("invalid TOML configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&mut self) -> Result<()> {
        if let Some(tls) = &mut self.tls { tls.validate(self.listen)?; }
        if !(1..=100_000).contains(&self.traffic.max_in_flight) ||
            !(1..=300_000).contains(&self.traffic.response_idle_ms) {
            bail!("invalid traffic resource limits");
        }
        let mut cache_budget = 0usize;
        let mut client_budget = 0usize;
        if self.upstream_timeout_ms == 0 || self.upstream_timeout_ms > 300_000 {
            bail!("upstream_timeout_ms must be in 1..=300000");
        }
        for (name, pool) in &mut self.pools {
            if name.is_empty() || pool.endpoints.is_empty() || pool.endpoints.len() > 128 {
                bail!("pool {name}: expected 1..=128 endpoints and a nonempty name");
            }
            let mut seen = HashSet::new();
            for endpoint in &mut pool.endpoints {
                let uri: Uri = endpoint.url.parse().context("invalid upstream URL")?;
                let Some(authority) = uri.authority() else { bail!("missing upstream authority"); };
                if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.path() != "/" ||
                    uri.query().is_some() || authority.as_str().contains('@') ||
                    endpoint.url.contains('#') || endpoint.weight == 0 || endpoint.weight > 100 {
                    bail!("pool {name}: endpoints must be HTTP(S) origins with weight 1..=100");
                }
                endpoint.url = format!("{}://{}", uri.scheme_str().unwrap(), authority.as_str().to_ascii_lowercase());
                if !seen.insert(endpoint.url.clone()) { bail!("pool {name}: duplicate endpoint"); }
            }
            if let Some(health) = &pool.health {
                if !valid_path(&health.path) || health.interval_ms == 0 ||
                    health.timeout_ms == 0 || health.timeout_ms > health.interval_ms ||
                    health.interval_ms > 3_600_000 ||
                    health.healthy_threshold == 0 || health.unhealthy_threshold == 0 {
                    bail!("pool {name}: invalid health check settings");
                }
            }
        }
        if self.routes.is_empty() { bail!("at least one route is required"); }
        let mut seen = HashSet::new();
        for route in &mut self.routes {
            route.policy.validate(route.pool.is_some())?;
            if let Some(c) = &route.policy.cache { cache_budget = cache_budget.saturating_add(c.max_entries.saturating_mul(c.max_body_bytes + 16384)); }
            if let Some(r) = &route.policy.rate_limit { client_budget = client_budget.saturating_add(r.max_clients); }
            if let Some(host) = &mut route.host {
                let normalized = hostname(host).context("invalid route host")?;
                if host.contains('*') { bail!("wildcard hosts are not supported"); }
                *host = normalized;
            }
            if !valid_path(&route.path_prefix) { bail!("invalid route path prefix"); }
            if route.path_prefix != "/" {
                route.path_prefix = route.path_prefix.trim_end_matches('/').to_string();
            }
            if !seen.insert((route.host.clone(), route.path_prefix.clone())) {
                bail!("duplicate host/path route");
            }
            match (&route.pool, &route.static_dir) {
                (Some(pool), None) if self.pools.contains_key(pool) => {},
                (None, Some(dir)) if !dir.as_os_str().is_empty() => {},
                _ => bail!("route must specify exactly one existing pool or static_dir"),
            }
        }
        if cache_budget > 128 * 1024 * 1024 || client_budget > 200_000 { bail!("aggregate policy capacity exceeded"); }
        if let Some(tls) = &self.tls {
            let domains = tls.domains();
            if self.routes.iter().filter_map(|r| r.host.as_ref()).any(|host| !domains.contains(host)) {
                bail!("every named route must be covered by an explicitly configured TLS domain");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boundary_matching() {
        assert!(path_matches("/api", "/api/v1"));
        assert!(path_matches("/api", "/api"));
        assert!(!path_matches("/api", "/apix"));
        assert_eq!(hostname("Example.COM:8080").as_deref(), Some("example.com"));
    }
    #[test]
    fn tls_modes_and_https_origins() {
        let input = r#"
listen = "127.0.0.1:8080"
[tls]
listen = "127.0.0.1:8443"
[[tls.certificates]]
domains = ["EXAMPLE.TEST"]
cert = "cert.pem"
key = "key.pem"
[pools.backend]
[[pools.backend.endpoints]]
url = "https://example.org"
[[routes]]
host = "example.test"
pool = "backend"
"#;
        let config = Config::parse(input).unwrap();
        assert_eq!(config.tls.unwrap().domains(), vec!["example.test"]);
        assert_eq!(config.pools["backend"].endpoints[0].url, "https://example.org");
        assert!(Config::parse(&input.replace("EXAMPLE.TEST", "*.example.test")).is_err());
        assert!(Config::parse(&input.replace("8443", "8080")).is_err());
        assert!(Config::parse(&input.replace("EXAMPLE.TEST", "other.test")).is_err());
        assert!(Config::parse(&input.replace("https://", "ftp://")).is_err());
    }

    #[test]
    fn acme_requires_public_443_and_defaults_to_staging() {
        let input = r#"
listen = "127.0.0.1:8080"
[tls]
listen = "127.0.0.1:8443"
public_port = 443
[tls.acme]
domains = ["example.org"]
email = "admin@example.org"
cache_dir = "state/acme"
[[routes]]
host = "example.org"
static_dir = "public"
"#;
        assert!(!Config::parse(input).unwrap().tls.unwrap().acme.unwrap().production);
        assert!(Config::parse(&input.replace("public_port = 443", "public_port = 8443")).is_err());
    }

    #[test]
    fn validates_traffic_example_and_rejects_unbounded_policy() {
        let source = include_str!("../../../configs/traffic.example.toml");
        assert!(Config::parse(source).is_ok());
        assert!(Config::parse(&source.replace("max_clients = 10000", "max_clients = 0")).is_err());
        assert!(Config::parse(&source.replace("max_body_bytes = 1048576", "max_body_bytes = 999999999")).is_err());
        assert!(Config::parse(&source.replace("max_entries = 128", "max_entries = 4097")).is_err());
    }

    #[test]
    fn rejects_invalid_pool() {
        let input = r#"
listen = "127.0.0.1:8080"
[[routes]]
pool = "missing"
"#;
        assert!(Config::parse(input).is_err());
    }
    #[test]
    fn rejects_duplicate_routes() {
        let input = r#"
listen = "127.0.0.1:8080"
[[routes]]
host = "EXAMPLE.COM"
static_dir = "public"
[[routes]]
host = "example.com"
static_dir = "other"
"#;
        assert!(Config::parse(input).is_err());
    }
}
