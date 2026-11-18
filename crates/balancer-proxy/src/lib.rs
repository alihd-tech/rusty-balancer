use axum::{body::Body, response::{IntoResponse, Response}};
use balancer_config::HealthConfig;
use balancer_lb::{Endpoint, Lease, Pool};
use bytes::Bytes;
use http::{header::{self, HeaderMap, HeaderName}, HeaderValue, Method, Request, StatusCode, Uri, Version};
use http_body::{Body as HttpBody, Frame, SizeHint};
use hyper::body::Incoming;
use hyper_util::{client::legacy::{connect::HttpConnector, Client}, rt::TokioExecutor};
use std::{net::IpAddr, pin::Pin, sync::Arc, task::{Context, Poll}, time::Duration};

pub type HttpClient = Client<HttpConnector, Body>;

pub fn client() -> HttpClient {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(Duration::from_secs(5)));
    Client::builder(TokioExecutor::new()).build(connector)
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    let nominated: Vec<HeaderName> = headers.get_all(header::CONNECTION).iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in nominated { headers.remove(name); }
    for name in ["connection", "proxy-connection", "keep-alive", "proxy-authenticate",
        "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade"] {
        headers.remove(name);
    }
}

struct LeasedBody {
    inner: Pin<Box<Incoming>>,
    lease: Option<Lease>,
}

impl HttpBody for LeasedBody {
    type Data = Bytes;
    type Error = hyper::Error;
    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>)
        -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let result = self.inner.as_mut().poll_frame(cx);
        if matches!(&result, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            self.lease.take();
        }
        result
    }
    fn is_end_stream(&self) -> bool { self.inner.is_end_stream() }
    fn size_hint(&self) -> SizeHint { self.inner.size_hint() }
}

pub async fn forward(client: &HttpClient, pool: &Pool, mut req: Request<Body>,
    peer: IpAddr, host: Option<String>, timeout: Duration) -> Response {
    if req.method() == Method::CONNECT || req.headers().contains_key(header::UPGRADE) {
        return (StatusCode::NOT_IMPLEMENTED, "tunneling and upgrades are not supported").into_response();
    }
    let Some(lease) = pool.acquire(peer.to_string().as_bytes()) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no healthy upstream").into_response();
    };
    let origin = &lease.endpoint().origin;
    let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if !path.starts_with('/') {
        return (StatusCode::BAD_REQUEST, "invalid request target").into_response();
    }
    let uri: Uri = match format!("{origin}{path}").parse() {
        Ok(uri) => uri,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid request target").into_response(),
    };
    let Some(authority) = uri.authority() else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let Ok(upstream_host) = HeaderValue::from_str(authority.as_str()) else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    *req.uri_mut() = uri;
    *req.version_mut() = Version::HTTP_11;
    let headers = req.headers_mut();
    strip_hop_headers(headers);
    // The directly connected client is authoritative. Never trust inbound proxy identity headers.
    let forwarded: Vec<_> = headers.keys().filter(|n| n.as_str().starts_with("x-forwarded-")
        || n.as_str() == "forwarded" || n.as_str() == "x-real-ip").cloned().collect();
    for name in forwarded { headers.remove(name); }
    headers.insert(header::HOST, upstream_host);
    if let Ok(value) = HeaderValue::from_str(&peer.to_string()) {
        headers.insert("x-forwarded-for", value);
    }
    headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    if let Some(host) = host.and_then(|h| HeaderValue::from_str(&h).ok()) {
        headers.insert("x-forwarded-host", host);
    }
    match tokio::time::timeout(timeout, client.request(req)).await {
        Ok(Ok(response)) => {
            let (mut parts, body) = response.into_parts();
            strip_hop_headers(&mut parts.headers);
            Response::from_parts(parts, Body::new(LeasedBody { inner: Box::pin(body), lease: Some(lease) }))
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, upstream = %lease.endpoint().origin, "upstream request failed");
            (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response()
        }
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "upstream response header timeout").into_response(),
    }
}

pub async fn health_worker(client: HttpClient, endpoint: Arc<Endpoint>, config: HealthConfig) {
    let mut interval = tokio::time::interval(Duration::from_millis(config.interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut successes = 0u32;
    let mut failures = 0u32;
    loop {
        interval.tick().await;
        let request = Request::builder().method(Method::GET)
            .uri(format!("{}{}", endpoint.origin, config.path)).body(Body::empty());
        let healthy = match request {
            Ok(request) => matches!(tokio::time::timeout(Duration::from_millis(config.timeout_ms),
                client.request(request)).await, Ok(Ok(response)) if response.status().is_success()),
            Err(_) => false,
        };
        if healthy {
            failures = 0;
            successes = successes.saturating_add(1);
            if successes >= config.healthy_threshold && !endpoint.is_healthy() {
                endpoint.set_healthy(true);
                tracing::info!(upstream = %endpoint.origin, "upstream recovered");
            }
        } else {
            successes = 0;
            failures = failures.saturating_add(1);
            if failures >= config.unhealthy_threshold && endpoint.is_healthy() {
                endpoint.set_healthy(false);
                tracing::warn!(upstream = %endpoint.origin, "upstream unhealthy");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_connection_nominated_headers() {
        let mut h = HeaderMap::new();
        h.insert("connection", HeaderValue::from_static("x-private, keep-alive"));
        h.insert("x-private", HeaderValue::from_static("secret"));
        h.insert("content-type", HeaderValue::from_static("text/plain"));
        strip_hop_headers(&mut h);
        assert!(!h.contains_key("x-private"));
        assert!(!h.contains_key("connection"));
        assert!(h.contains_key("content-type"));
    }
}
