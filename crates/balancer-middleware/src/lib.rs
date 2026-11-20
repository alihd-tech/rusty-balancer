pub mod cache;
pub mod limits;
pub mod rate;

use axum::{
    body::Body,
    response::{IntoResponse, Response},
};
use balancer_config::RoutePolicy;
use http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use std::{net::IpAddr, time::Duration};

pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        _ => ip,
    }
}

/// Only accept client-address headers from explicitly configured trusted proxies.
/// Walk from the immediate peer toward the client, stopping at the first
/// untrusted hop. All incoming forwarding headers from direct peers are ignored.
pub fn client_ip(
    peer: IpAddr,
    headers: &HeaderMap,
    trusted: &[ipnet::IpNet],
) -> Result<IpAddr, StatusCode> {
    let peer = canonical_ip(peer);
    if !trusted.iter().any(|net| net.contains(&peer)) {
        return Ok(peer);
    }
    let mut values = headers.get_all("x-forwarded-for").iter();
    let Some(value) = values.next() else {
        return Ok(peer);
    };
    if values.next().is_some() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let raw = value.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;
    if raw.len() > 4096 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let parts: Vec<_> = raw.split(',').collect();
    if parts.is_empty() || parts.len() > 32 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut chain = Vec::with_capacity(parts.len());
    for part in parts {
        let ip = part
            .trim()
            .parse::<IpAddr>()
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        chain.push(canonical_ip(ip));
    }
    let mut current = peer;
    for hop in chain.into_iter().rev() {
        if !trusted.iter().any(|net| net.contains(&current)) {
            break;
        }
        current = hop;
    }
    Ok(current)
}

pub fn allowed(policy: &RoutePolicy, peer: IpAddr) -> bool {
    let ip = canonical_ip(peer);
    !policy.deny.iter().any(|net| net.contains(&ip))
        && (policy.allow.is_empty() || policy.allow.iter().any(|net| net.contains(&ip)))
}

pub fn security_headers(headers: &mut HeaderMap, secure: bool) {
    headers
        .entry("x-content-type-options")
        .or_insert(HeaderValue::from_static("nosniff"));
    headers
        .entry("x-frame-options")
        .or_insert(HeaderValue::from_static("DENY"));
    headers
        .entry("referrer-policy")
        .or_insert(HeaderValue::from_static("strict-origin-when-cross-origin"));
    if secure {
        headers
            .entry("strict-transport-security")
            .or_insert(HeaderValue::from_static("max-age=86400"));
    }
}

pub async fn bound_request(
    req: Request<Body>,
    policy: &RoutePolicy,
) -> Result<Request<Body>, Response> {
    let Some(limit) = policy.max_body_bytes else {
        return Ok(req);
    };
    if req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > limit as u64)
    {
        return Err(StatusCode::PAYLOAD_TOO_LARGE.into_response());
    }
    let (mut parts, body) = req.into_parts();
    let buffer = tokio::time::timeout(
        Duration::from_millis(policy.body_timeout_ms.unwrap_or(10000)),
        axum::body::to_bytes(body, limit),
    )
    .await;
    match buffer {
        Ok(Ok(bytes)) => {
            parts.headers.remove(header::TRANSFER_ENCODING);
            parts.headers.remove(header::TRAILER);
            parts.headers.remove(header::CONTENT_LENGTH);
            Ok(Request::from_parts(parts, Body::from(bytes)))
        }
        Ok(Err(_)) => Err(StatusCode::PAYLOAD_TOO_LARGE.into_response()),
        Err(_) => Err(StatusCode::REQUEST_TIMEOUT.into_response()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deny_overrides_allow_and_mapped_addresses_are_normalized() {
        let p = RoutePolicy {
            allow: vec!["192.0.2.0/24".parse().unwrap()],
            deny: vec!["192.0.2.5/32".parse().unwrap()],
            ..Default::default()
        };
        assert!(allowed(&p, "::ffff:192.0.2.6".parse().unwrap()));
        assert!(!allowed(&p, "192.0.2.5".parse().unwrap()));
        assert!(!allowed(&p, "198.51.100.1".parse().unwrap()));
    }
    #[test]
    fn resolves_only_explicitly_trusted_proxy_hops() {
        let trusted = vec![
            "10.0.0.0/8".parse().unwrap(),
            "192.0.2.0/24".parse().unwrap(),
        ];
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.10, 192.0.2.8"),
        );
        assert_eq!(
            client_ip("10.1.2.3".parse().unwrap(), &headers, &trusted).unwrap(),
            "198.51.100.10".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            client_ip("203.0.113.6".parse().unwrap(), &headers, &trusted).unwrap(),
            "203.0.113.6".parse::<IpAddr>().unwrap()
        );
        headers.insert("x-forwarded-for", HeaderValue::from_static("invalid"));
        assert_eq!(
            client_ip("10.1.2.3".parse().unwrap(), &headers, &trusted),
            Err(StatusCode::BAD_REQUEST)
        );
        assert!(client_ip("203.0.113.6".parse().unwrap(), &headers, &trusted).is_ok());
    }
    #[tokio::test]
    async fn rejects_stream_without_content_length_over_limit() {
        let req = Request::new(Body::from("12345"));
        let policy = RoutePolicy {
            max_body_bytes: Some(4),
            ..Default::default()
        };
        assert_eq!(
            bound_request(req, &policy).await.unwrap_err().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}
