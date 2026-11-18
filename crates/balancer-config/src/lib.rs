use anyhow::{bail, Context, Result};
use http::{uri::Authority, Uri};
use serde::Deserialize;
use std::{collections::{BTreeMap, HashSet}, net::SocketAddr, path::{Path, PathBuf}};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    #[serde(default = "timeout_default")]
    pub upstream_timeout_ms: u64,
    #[serde(default)]
    pub pools: BTreeMap<String, PoolConfig>,
    pub routes: Vec<RouteConfig>,
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
                if uri.scheme_str() != Some("http") || uri.path() != "/" ||
                    uri.query().is_some() || authority.as_str().contains('@') ||
                    endpoint.url.contains('#') || endpoint.weight == 0 || endpoint.weight > 100 {
                    bail!("pool {name}: endpoints must be HTTP origins with weight 1..=100");
                }
                endpoint.url = format!("http://{}", authority.as_str().to_ascii_lowercase());
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
