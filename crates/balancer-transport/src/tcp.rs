use super::{allowed,pool};
use anyhow::{bail,Context,Result};
use balancer_config::transport::TcpService;
use balancer_lb::Pool;
use std::{collections::HashMap,io::Cursor,net::SocketAddr,sync::{Arc,Mutex},time::{Duration,Instant}};
use tokio::{io::{AsyncRead,AsyncReadExt,AsyncWrite,AsyncWriteExt},net::{TcpListener,TcpStream},sync::Semaphore,task::JoinSet};
use tokio_util::sync::CancellationToken;

async fn client_hello(stream: &mut TcpStream) -> Result<(String,Vec<u8>)> {
    let mut acceptor=rustls::server::Acceptor::default();
    let mut prefix=Vec::new();
    let mut buf=[0u8;4096];
    loop {
        let n=stream.read(&mut buf).await?;
        if n==0 || prefix.len()+n>65536 { bail!("incomplete or oversized TLS ClientHello"); }
        prefix.extend_from_slice(&buf[..n]);
        let mut cursor=Cursor::new(&buf[..n]);
        while cursor.position()<n as u64 {
            if acceptor.read_tls(&mut cursor)?==0 { bail!("invalid ClientHello record"); }
            match acceptor.accept() {
                Ok(Some(hello)) => {
                    let name=hello.client_hello().server_name().context("missing SNI")?.to_ascii_lowercase();
                    return Ok((name,prefix));
                }
                Ok(None)=>{},
                Err(_)=>bail!("invalid TLS ClientHello"),
            }
        }
    }
}

fn proxy_line(peer: SocketAddr, local: SocketAddr)->String {
    match (peer,local) {
        (SocketAddr::V4(a),SocketAddr::V4(b))=>format!("PROXY TCP4 {} {} {} {}\r\n",a.ip(),b.ip(),a.port(),b.port()),
        (SocketAddr::V6(a),SocketAddr::V6(b))=>format!("PROXY TCP6 {} {} {} {}\r\n",a.ip(),b.ip(),a.port(),b.port()),
        _=>"PROXY UNKNOWN\r\n".into()
    }
}
async fn pump<R:AsyncRead+Unpin,W:AsyncWrite+Unpin>(mut read:R,mut write:W,activity:Arc<Mutex<Instant>>)->std::io::Result<()> {
    let mut buf=[0u8;16384];
    loop {
        let n=read.read(&mut buf).await?;
        if n==0 { write.shutdown().await?; return Ok(()); }
        write.write_all(&buf[..n]).await?;
        *activity.lock().unwrap_or_else(|e| e.into_inner())=Instant::now();
    }
}
async fn relay(mut down:TcpStream,mut up:TcpStream,idle:Duration)->Result<()> {
    let activity=Arc::new(Mutex::new(Instant::now()));
    let (dr,dw)=down.split();let (ur,uw)=up.split();
    let watchdog=async {
        loop {
            tokio::time::sleep(idle.min(Duration::from_secs(1))).await;
            if activity.lock().unwrap_or_else(|e|e.into_inner()).elapsed() >= idle { break; }
        }
    };
    tokio::select! {
        result=async{tokio::try_join!(pump(dr,uw,activity.clone()),pump(ur,dw,activity.clone()))}=>{result?;},
        _=watchdog=>{},
    }
    Ok(())
}
async fn connection(mut down:TcpStream,peer:SocketAddr,cfg:Arc<TcpService>,default:Option<Arc<Pool>>,routes:Arc<HashMap<String,Arc<Pool>>>)->Result<()> {
    let local=down.local_addr()?;
    let deadline=Duration::from_millis(cfg.connect_timeout_ms);
    let (selected,prefix)=if routes.is_empty() {(default.context("no TCP target")?,Vec::new())}
        else {
            let (host,prefix)=tokio::time::timeout(deadline,client_hello(&mut down)).await??;
            (routes.get(&host).cloned().or(default).context("unmatched TLS SNI")?,prefix)
        };
    let lease=selected.acquire(peer.ip().to_string().as_bytes()).context("no TCP upstream")?;
    let addr:SocketAddr=lease.endpoint().origin.parse()?;
    let mut up=tokio::time::timeout(deadline,TcpStream::connect(addr)).await??;
    up.set_nodelay(true)?;down.set_nodelay(true)?;
    tokio::time::timeout(deadline,async {
        if cfg.proxy_protocol { up.write_all(proxy_line(peer,local).as_bytes()).await?; }
        up.write_all(&prefix).await
    }).await??;
    relay(down,up,Duration::from_millis(cfg.idle_timeout_ms)).await
}
pub async fn serve(listener:TcpListener,cfg:TcpService,stop:CancellationToken)->Result<()> {
    let default=(!cfg.targets.is_empty()).then(||pool(&cfg.targets,cfg.strategy));
    let routes=Arc::new(cfg.sni.iter().map(|r|(r.host.clone(),pool(&r.targets,cfg.strategy))).collect());
    let limit=Arc::new(Semaphore::new(cfg.max_connections));let cfg=Arc::new(cfg);
    let mut tasks=JoinSet::new();
    tracing::info!(name=%cfg.name,address=%listener.local_addr()?,"TCP transport listening");
    loop {
        tokio::select! {
            biased;
            _=stop.cancelled()=>break,
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            accepted=listener.accept()=>{
                let (stream,peer)=accepted?;
                if !allowed(peer.ip(),&cfg.allow,&cfg.deny) {continue;}
                let Ok(permit)=limit.clone().try_acquire_owned() else {continue;};
                let cfg=cfg.clone();let routes=routes.clone();let default=default.clone();
                tasks.spawn(async move {
                    let _permit=permit;
                    let name=cfg.name.clone();
                    let duration=Duration::from_secs(cfg.max_lifetime_seconds);
                    if let Ok(Err(e))=tokio::time::timeout(duration,connection(stream,peer,cfg,default,routes)).await {
                        tracing::debug!(%name,error=%e,"TCP session ended");
                    }
                });
            }
        }
    }
    drop(listener);
    if tokio::time::timeout(Duration::from_secs(30),async{while tasks.join_next().await.is_some(){}}).await.is_err(){tasks.abort_all();}
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proxy_protocol_is_generated_from_socket_addresses(){
        assert_eq!(proxy_line("192.0.2.1:1234".parse().unwrap(),"192.0.2.2:443".parse().unwrap()),"PROXY TCP4 192.0.2.1 192.0.2.2 1234 443\r\n");
    }
}
