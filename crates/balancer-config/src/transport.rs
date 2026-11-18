use anyhow::{bail, Result};
use serde::Deserialize;
use std::{collections::HashSet, net::SocketAddr};

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TransportConfig {
    pub tcp: Vec<TcpService>,
    pub udp: Vec<UdpService>,
}
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Target { pub address: SocketAddr, #[serde(default = "one")] pub weight: u32 }
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Strategy { #[default] RoundRobin, WeightedRoundRobin, LeastConnections, ConsistentHash }
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TcpService {
    pub name: String,
    pub listen: SocketAddr,
    #[serde(default)]
    pub strategy: Strategy,
    #[serde(default)]
    pub targets: Vec<Target>,
    #[serde(default)]
    pub sni: Vec<SniRoute>,
    #[serde(default = "tcp_limit")]
    pub max_connections: usize,
    #[serde(default = "connect_timeout")]
    pub connect_timeout_ms: u64,
    #[serde(default = "idle_timeout")]
    pub idle_timeout_ms: u64,
    #[serde(default = "lifetime")]
    pub max_lifetime_seconds: u64,
    #[serde(default)]
    pub proxy_protocol: bool,
    #[serde(default)]
    pub allow: Vec<ipnet::IpNet>,
    #[serde(default)]
    pub deny: Vec<ipnet::IpNet>,
}
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SniRoute { pub host: String, pub targets: Vec<Target> }
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UdpService {
    pub name: String,
    pub listen: SocketAddr,
    #[serde(default)]
    pub strategy: Strategy,
    pub targets: Vec<Target>,
    #[serde(default = "udp_limit")]
    pub max_sessions: usize,
    #[serde(default = "idle_timeout")]
    pub idle_timeout_ms: u64,
    #[serde(default = "lifetime")]
    pub max_lifetime_seconds: u64,
    #[serde(default = "datagram_limit")]
    pub max_datagram_bytes: usize,
    #[serde(default)]
    pub allow: Vec<ipnet::IpNet>,
    #[serde(default)]
    pub deny: Vec<ipnet::IpNet>,
}
fn one() -> u32 { 1 }
fn tcp_limit() -> usize { 256 }
fn udp_limit() -> usize { 128 }
fn connect_timeout() -> u64 { 5000 }
fn idle_timeout() -> u64 { 30000 }
fn lifetime() -> u64 { 3600 }
fn datagram_limit() -> usize { 4096 }
fn valid_targets(targets: &[Target]) -> bool {
    let mut seen = HashSet::new();
    !targets.is_empty() && targets.len() <= 64 && targets.iter().all(|t|
        t.address.port() != 0 && !t.address.ip().is_unspecified() &&
        !t.address.ip().is_multicast() && (1..=100).contains(&t.weight) && seen.insert(t.address))
}
fn valid_common(name: &str, listen: SocketAddr, idle: u64, life: u64) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') &&
        listen.port() != 0 && (100..=300000).contains(&idle) && (1..=86400).contains(&life)
}
impl TransportConfig {
    pub fn validate(&mut self, http: SocketAddr, tls: Option<SocketAddr>, admin: Option<SocketAddr>) -> Result<()> {
        if self.tcp.len() + self.udp.len() > 32 { bail!("too many transport services"); }
        let mut names = HashSet::new();
        let mut tcp_ports = HashSet::from([http.port()]);
        tcp_ports.extend(tls.map(|a| a.port()));
        tcp_ports.extend(admin.map(|a| a.port()));
        let mut udp_ports = HashSet::new();
        let mut tcp_total = 0usize;
        let mut udp_budget = 0usize;
        let mut ring_budget = 0u64;
        for t in &mut self.tcp {
            if !valid_common(&t.name,t.listen,t.idle_timeout_ms,t.max_lifetime_seconds) ||
                !names.insert(t.name.clone()) || !tcp_ports.insert(t.listen.port()) ||
                !(1..=4096).contains(&t.max_connections) || !(100..=30000).contains(&t.connect_timeout_ms) ||
                t.allow.len()+t.deny.len()>1024 || t.sni.len()>64 ||
                (t.targets.is_empty() && t.sni.is_empty()) ||
                (!t.targets.is_empty() && !valid_targets(&t.targets)) {
                bail!("invalid TCP service {}",t.name);
            }
            let mut hosts = HashSet::new();
            for s in &mut t.sni {
                s.host = s.host.to_ascii_lowercase();
                if !crate::is_dns_name(&s.host) || !hosts.insert(s.host.clone()) || !valid_targets(&s.targets) {
                    bail!("invalid SNI target in {}", t.name);
                }
            }
            if t.strategy == Strategy::ConsistentHash {
                ring_budget += t.targets.iter().chain(t.sni.iter().flat_map(|s| &s.targets)).map(|e|u64::from(e.weight)*64).sum::<u64>();
            }
            tcp_total = tcp_total.saturating_add(t.max_connections);
        }
        for u in &self.udp {
            if !valid_common(&u.name,u.listen,u.idle_timeout_ms,u.max_lifetime_seconds) ||
                !names.insert(u.name.clone()) || !udp_ports.insert(u.listen.port()) ||
                !(1..=1024).contains(&u.max_sessions) || !(512..=16384).contains(&u.max_datagram_bytes) ||
                u.allow.len()+u.deny.len()>1024 || !valid_targets(&u.targets) {
                bail!("invalid UDP service {}",u.name);
            }
            if u.strategy == Strategy::ConsistentHash { ring_budget += u.targets.iter().map(|e|u64::from(e.weight)*64).sum::<u64>(); }
            udp_budget = udp_budget.saturating_add(u.max_sessions.saturating_mul(u.max_datagram_bytes*10 + 4096));
        }
        if tcp_total > 8192 || udp_budget > 64*1024*1024 || ring_budget > 2_000_000 { bail!("aggregate transport resource budget exceeded"); }
        Ok(())
    }
}
