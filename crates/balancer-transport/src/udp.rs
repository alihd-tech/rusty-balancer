use super::{allowed,pool};
use anyhow::Result;
use balancer_config::transport::UdpService;
use balancer_lb::Lease;
use std::{collections::HashMap,net::SocketAddr,sync::Arc,time::Duration};
use tokio::{net::UdpSocket,sync::mpsc,task::JoinSet,time::Instant};
use tokio_util::sync::CancellationToken;

async fn session(listener:Arc<UdpSocket>,peer:SocketAddr,lease:Lease,mut input:mpsc::Receiver<Vec<u8>>,cfg:Arc<UdpService>)->Result<()> {
    let target:SocketAddr=lease.endpoint().origin.parse()?;
    let bind=if target.is_ipv4() {"0.0.0.0:0"}else{"[::]:0"};
    let socket=UdpSocket::bind(bind).await?;
    socket.connect(target).await?;
    let idle=Duration::from_millis(cfg.idle_timeout_ms);
    let lifetime=Instant::now()+Duration::from_secs(cfg.max_lifetime_seconds);
    let mut deadline=(Instant::now()+idle).min(lifetime);
    let mut buf=vec![0u8;cfg.max_datagram_bytes+1];
    loop {
        tokio::select! {
            _=tokio::time::sleep_until(deadline)=>break,
            frame=input.recv()=>{
                let Some(frame)=frame else {break;};
                // Bound even if the socket becomes backpressured.
                tokio::time::timeout_at(deadline,socket.send(&frame)).await??;
                deadline=(Instant::now()+idle).min(lifetime);
            }
            n=socket.recv(&mut buf)=>{
                let n=n?;
                if n>cfg.max_datagram_bytes {continue;}
                tokio::time::timeout_at(deadline,listener.send_to(&buf[..n],peer)).await??;
                deadline=(Instant::now()+idle).min(lifetime);
            }
        }
        if Instant::now()>=lifetime {break;}
    }
    Ok(())
}
pub async fn serve(listener:UdpSocket,cfg:UdpService,stop:CancellationToken)->Result<()> {
    let listener=Arc::new(listener);let cfg=Arc::new(cfg);let pool=pool(&cfg.targets,cfg.strategy);
    let mut sessions:HashMap<SocketAddr,mpsc::Sender<Vec<u8>>>=HashMap::new();
    let mut tasks=JoinSet::new();
    let mut buf=vec![0u8;cfg.max_datagram_bytes+1];
    tracing::info!(name=%cfg.name,address=%listener.local_addr()?,"UDP transport listening");
    loop {
        tokio::select! {
            biased;
            _=stop.cancelled()=>break,
            Some(result)=tasks.join_next(),if !tasks.is_empty()=>{
                let peer=result?;sessions.remove(&peer);
            }
            received=listener.recv_from(&mut buf)=>{
                let (n,peer)=received?;
                if n>cfg.max_datagram_bytes || !allowed(peer.ip(),&cfg.allow,&cfg.deny) {continue;}
                if let Some(tx)=sessions.get(&peer) {let _=tx.try_send(buf[..n].to_vec());continue;}
                if sessions.len()>=cfg.max_sessions {continue;}
                let Some(lease)=pool.acquire(peer.ip().to_string().as_bytes()) else{continue;};
                let (tx,rx)=mpsc::channel(8);
                let _=tx.try_send(buf[..n].to_vec()); sessions.insert(peer,tx);
                let listener=listener.clone();let cfg=cfg.clone();
                tasks.spawn(async move {
                    if let Err(e)=session(listener,peer,lease,rx,cfg).await {tracing::debug!(error=%e,"UDP session ended");}
                    peer
                });
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some(){}
    Ok(())
}
