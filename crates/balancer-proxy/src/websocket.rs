//! HTTP/1.1 WebSocket tunnel. The upstream authenticates the handshake; no arbitrary upgrades.
use super::{forwarding_headers, strip_hop_headers, HttpClient};
use axum::{body::Body, response::{IntoResponse, Response}};
use balancer_lb::Pool;
use base64::{engine::general_purpose::STANDARD, Engine};
use http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, Version};
use hyper_util::rt::TokioIo;
use sha1::{Digest, Sha1};
use std::{net::IpAddr, time::Duration};
use tokio::sync::OwnedSemaphorePermit;

fn token(headers: &HeaderMap, name: header::HeaderName, expected: &str) -> bool {
    headers.get_all(name).iter().filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',')).any(|v| v.trim().eq_ignore_ascii_case(expected))
}
fn single<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a HeaderValue> {
    let mut values = h.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() { None } else { Some(value) }
}
fn expected_accept(headers: &HeaderMap) -> Option<String> {
    let key = single(headers, "sec-websocket-key")?.to_str().ok()?;
    if STANDARD.decode(key).ok()?.len() != 16 { return None; }
    let mut hash = Sha1::new();
    hash.update(key.as_bytes());
    hash.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    Some(STANDARD.encode(hash.finalize()))
}

pub async fn forward(client: &HttpClient, pool: &Pool, mut req: Request<Body>,
    peer: IpAddr, host: Option<String>, secure: bool, deadline: Duration,
    max_lifetime: Duration, permit: OwnedSemaphorePermit) -> Response {
    use http_body::Body as _;
    if req.method() != Method::GET || req.version() != Version::HTTP_11 ||
        !token(req.headers(), header::CONNECTION, "upgrade") ||
        !single(req.headers(), "upgrade").is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket")) ||
        !single(req.headers(), "sec-websocket-version").is_some_and(|v| v == "13") ||
        !req.body().is_end_stream() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(accept) = expected_accept(req.headers()) else { return StatusCode::BAD_REQUEST.into_response(); };
    let Some(lease) = pool.acquire(peer.to_string().as_bytes()) else { return StatusCode::SERVICE_UNAVAILABLE.into_response(); };
    let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let Ok(uri) = format!("{}{path}", lease.endpoint().origin).parse::<Uri>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(authority) = uri.authority() else { return StatusCode::BAD_GATEWAY.into_response(); };
    let Ok(upstream_host) = HeaderValue::from_str(authority.as_str()) else { return StatusCode::BAD_GATEWAY.into_response(); };
    let downstream = hyper::upgrade::on(&mut req);
    *req.uri_mut() = uri;
    strip_hop_headers(req.headers_mut());
    // Do not resurrect arbitrary connection-nominated fields.
    if expected_accept(req.headers()).as_deref() != Some(accept.as_str()) ||
        !single(req.headers(), "sec-websocket-version").is_some_and(|v| v == "13") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    forwarding_headers(req.headers_mut(), upstream_host, peer, host, secure);
    req.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    req.headers_mut().insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    let mut response = match tokio::time::timeout(deadline, client.request(req)).await {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => {
            lease.endpoint().observe_failure();
            return StatusCode::BAD_GATEWAY.into_response();
        },
        Err(_) => {
            lease.endpoint().observe_failure();
            return StatusCode::GATEWAY_TIMEOUT.into_response();
        },
    };
    if response.status() != StatusCode::SWITCHING_PROTOCOLS ||
        !token(response.headers(), header::CONNECTION, "upgrade") ||
        !single(response.headers(), "upgrade").is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket")) ||
        !single(response.headers(), "sec-websocket-accept").is_some_and(|v| v.as_bytes() == accept.as_bytes()) {
        lease.endpoint().observe_failure();
        return StatusCode::BAD_GATEWAY.into_response();
    }
    lease.endpoint().observe_success();
    let upstream = hyper::upgrade::on(&mut response);
    let (mut parts, _) = response.into_parts();
    strip_hop_headers(&mut parts.headers);
    if !single(&parts.headers, "sec-websocket-accept").is_some_and(|v| v.as_bytes() == accept.as_bytes()) {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    parts.headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    tokio::spawn(async move {
        let _permit = permit;
        let _lease = lease;
        let tunnel = async {
            let (Ok(down), Ok(up)) = tokio::join!(downstream, upstream) else { return; };
            let mut down = TokioIo::new(down);
            let mut up = TokioIo::new(up);
            if let Err(error) = tokio::io::copy_bidirectional(&mut down, &mut up).await {
                tracing::debug!(%error, "websocket tunnel closed");
            }
        };
        if tokio::time::timeout(max_lifetime, tunnel).await.is_err() {
            tracing::debug!("websocket maximum lifetime reached");
        }
    });
    Response::from_parts(parts, Body::empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rfc_handshake_accept() {
        let mut h = HeaderMap::new();
        h.insert("sec-websocket-key", HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="));
        assert_eq!(expected_accept(&h).as_deref(), Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
        h.append("sec-websocket-key", HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="));
        assert!(expected_accept(&h).is_none());
    }
}
