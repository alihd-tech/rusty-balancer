use anyhow::{Context, Result};
use balancer_config::{Algorithm, EndpointConfig, PoolConfig, transport::{Strategy, Target, TransportConfig}};
use balancer_lb::Pool;
use std::{net::IpAddr, sync::Arc};
use tokio::{net::{TcpListener,UdpSocket}, task::JoinSet};
use tokio_util::sync::CancellationToken;
mod tcp;
mod udp;

fn pool(targets: &[Target], strategy: Strategy) -> Arc<Pool> {
    let algorithm = match strategy { Strategy::RoundRobin=>Algorithm::RoundRobin, Strategy::WeightedRoundRobin=>Algorithm::WeightedRoundRobin,
        Strategy::LeastConnections=>Algorithm::LeastConnections, Strategy::ConsistentHash=>Algorithm::ConsistentHash };
    Arc::new(Pool::new(&PoolConfig { algorithm, health:None, endpoints:targets.iter().map(|t|EndpointConfig {
        url:t.address.to_string(), weight:t.weight }).collect() }))
}
fn allowed(ip: IpAddr, allow: &[ipnet::IpNet], deny: &[ipnet::IpNet]) -> bool {
    let ip = match ip { IpAddr::V6(ip6)=>ip6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip), _=>ip };
    !deny.iter().any(|n|n.contains(&ip)) && (allow.is_empty() || allow.iter().any(|n|n.contains(&ip)))
}
pub struct Bound {
    tcp: Vec<(TcpListener,balancer_config::transport::TcpService)>,
    udp: Vec<(UdpSocket,balancer_config::transport::UdpService)>,
}
impl Bound {
    pub async fn bind(config: TransportConfig) -> Result<Self> {
        let mut tcp=Vec::new(); let mut udp=Vec::new();
        for t in config.tcp { tcp.push((TcpListener::bind(t.listen).await.with_context(||format!("bind TCP {}",t.name))?,t)); }
        for u in config.udp { udp.push((UdpSocket::bind(u.listen).await.with_context(||format!("bind UDP {}",u.name))?,u)); }
        Ok(Self{tcp,udp})
    }
    pub async fn serve(self, stop: CancellationToken) -> Result<()> {
        let mut tasks=JoinSet::new();
        for (l,c) in self.tcp { tasks.spawn(tcp::serve(l,c,stop.clone())); }
        for (l,c) in self.udp { tasks.spawn(udp::serve(l,c,stop.clone())); }
        while let Some(done)=tasks.join_next().await { done??; }
        Ok(())
    }
}
