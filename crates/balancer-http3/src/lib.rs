//! QUIC HTTP/3 ingress. The existing HTTP router remains the sole application data plane.
use anyhow::{Context,Result};
use axum::{body::Body,extract::ConnectInfo,Router};
use balancer_config::{TlsConfig,transport::Http3Config};
use bytes::{Buf,Bytes,BytesMut};
use http::{header,Method,Request,Response,StatusCode,Version};
use http_body_util::BodyExt;
use std::{net::SocketAddr,sync::Arc,time::Duration};
use tokio::{sync::Semaphore,task::JoinSet};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

pub struct Bound { endpoint:quinn::Endpoint,cfg:Http3Config }
impl Bound {
    pub fn bind(cfg:Http3Config,tls:&TlsConfig)->Result<Self> {
        let mut crypto=(*balancer_tls::manual_config(tls)?).clone();
        crypto.alpn_protocols=vec![b"h3".to_vec()];
        crypto.max_early_data_size=0;
        let crypto=quinn::crypto::rustls::QuicServerConfig::try_from(crypto)?;
        let mut server=quinn::ServerConfig::with_crypto(Arc::new(crypto));
        let mut transport=quinn::TransportConfig::default();
        transport.max_concurrent_bidi_streams(32u32.into()).max_concurrent_uni_streams(8u32.into())
            .stream_receive_window(65536u32.into()).receive_window(524288u32.into())
            .max_idle_timeout(Some(Duration::from_secs(30).try_into()?));
        server.transport_config(Arc::new(transport));
        server.migration(false);
        let endpoint=quinn::Endpoint::server(server,cfg.listen).context("bind HTTP/3 endpoint")?;
        Ok(Self{endpoint,cfg})
    }
    pub async fn serve(self,router:Router,stop:CancellationToken)->Result<()> {
        let connections=Arc::new(Semaphore::new(self.cfg.max_connections));
        let requests=Arc::new(Semaphore::new(self.cfg.max_requests));
        let cfg=Arc::new(self.cfg);
        let mut tasks=JoinSet::new();
        tracing::info!(address=%self.endpoint.local_addr()?,"HTTP/3 listener bound");
        loop {
            tokio::select! {
                biased;
                _=stop.cancelled()=>break,
                Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
                incoming=self.endpoint.accept()=>{
                    let Some(incoming)=incoming else{break;};
                    let Ok(permit)=connections.clone().try_acquire_owned() else{incoming.refuse();continue;};
                    // Validate client address before accepting handshake to bound amplification.
                    if !incoming.remote_address_validated() {
                        if incoming.may_retry() { let _=incoming.retry(); }
                        else { incoming.refuse(); }
                        continue;
                    }
                    let (router,cfg,requests)=(router.clone(),cfg.clone(),requests.clone());
                    tasks.spawn(async move {
                        let _permit=permit;
                        if let Ok(Ok(conn))=tokio::time::timeout(Duration::from_secs(10),incoming).await {
                            if let Err(error)=connection(conn,router,cfg,requests).await {tracing::debug!(%error,"HTTP/3 connection ended");}
                        }
                    });
                }
            }
        }
        self.endpoint.close(0u32.into(),b"shutdown");
        tasks.abort_all();
        while tasks.join_next().await.is_some(){}
        self.endpoint.wait_idle().await;
        Ok(())
    }
}

async fn connection(conn:quinn::Connection,router:Router,cfg:Arc<Http3Config>,requests:Arc<Semaphore>)->Result<()> {
    let peer=conn.remote_address();
    let mut h3=h3::server::builder().max_field_section_size(16384)
        .build(h3_quinn::Connection::new(conn)).await?;
    let mut tasks=JoinSet::new();
    loop {
        tokio::select! {
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            accepted=h3.accept()=>{
                match accepted? {
                    Some(resolver)=>{
                        let Ok(permit)=requests.clone().try_acquire_owned() else{drop(resolver);continue;};
                        let router=router.clone();let cfg=cfg.clone();
                        tasks.spawn(async move {
                            let _permit=permit;
                            let deadline=Duration::from_millis(cfg.request_timeout_ms);
                            let _=tokio::time::timeout(deadline,async move{
                                let (request,stream)=resolver.resolve_request().await.map_err(anyhow::Error::from)?;
                                handle(request,stream,router,peer,cfg.max_body_bytes).await
                            }).await;
                        });
                    }
                    None=>break,
                }
            }
        }
    }
    tasks.abort_all();
    Ok(())
}

type Stream=h3::server::RequestStream<h3_quinn::BidiStream<Bytes>,Bytes>;
async fn reject(stream:&mut Stream,status:StatusCode)->Result<()> {
    stream.stop_sending(h3::error::Code::H3_REQUEST_CANCELLED);
    stream.send_response(Response::builder().status(status).body(())?).await?;
    stream.finish().await?;
    Ok(())
}
async fn handle(request:Request<()>,mut stream:Stream,router:Router,peer:SocketAddr,max:usize)->Result<()> {
    if request.method()==Method::CONNECT || request.headers().contains_key(header::UPGRADE) ||
        request.uri().scheme_str()!=Some("https") || request.uri().authority().is_none(){
        return reject(&mut stream,StatusCode::BAD_REQUEST).await;
    }
    let declared=request.headers().get(header::CONTENT_LENGTH).map(|v|v.to_str().ok().and_then(|s|s.parse::<usize>().ok()));
    if matches!(declared,Some(None)) || declared.flatten().is_some_and(|n|n>max) {
        return reject(&mut stream,StatusCode::PAYLOAD_TOO_LARGE).await;
    }
    let mut body=BytesMut::new();
    while let Some(mut chunk)=stream.recv_data().await? {
        if chunk.remaining()>max.saturating_sub(body.len()) {return reject(&mut stream,StatusCode::PAYLOAD_TOO_LARGE).await;}
        body.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
    }
    if declared.flatten().is_some_and(|n|n!=body.len()){
        return reject(&mut stream,StatusCode::BAD_REQUEST).await;
    }
    let (mut parts,_)=request.into_parts();
    parts.version=Version::HTTP_3;
    let mut request=Request::from_parts(parts,Body::from(body.freeze()));
    request.extensions_mut().insert(ConnectInfo(peer));
    let response=router.oneshot(request).await.expect("infallible router");
    let (mut parts,mut body)=response.into_parts();
    parts.version=Version::HTTP_3;
    for h in ["connection","keep-alive","proxy-connection","transfer-encoding","upgrade","trailer"] {parts.headers.remove(h);}
    stream.send_response(Response::from_parts(parts,())).await?;
    while let Some(frame)=body.frame().await {
        let frame=frame?;
        if let Ok(data)=frame.into_data(){stream.send_data(data).await?;}
    }
    stream.finish().await?;
    Ok(())
}
