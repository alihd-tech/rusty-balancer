use axum::{body::{Body, to_bytes}, response::Response};
use balancer_config::CacheConfig;
use bytes::Bytes;
use http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use http_body::Body as HttpBody;
use std::{collections::HashMap, sync::{Mutex, atomic::{AtomicU64, Ordering}}, time::{Duration, Instant, SystemTime}};

struct Entry { headers: HeaderMap, body: Bytes, at: Instant, age: u64, ttl: Duration }
pub struct Cache { config: CacheConfig, entries: Mutex<HashMap<String, Entry>>, epoch: AtomicU64, hits: AtomicU64, misses: AtomicU64 }
#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheStats { pub entries: usize, pub body_bytes: usize, pub hits: u64, pub misses: u64 }

pub fn eligible(req: &Request<Body>) -> bool {
    req.method() == Method::GET && req.body().is_end_stream() &&
        !req.headers().keys().any(|h| {
            let n = h.as_str();
            matches!(n, "authorization" | "proxy-authorization" | "cookie" | "range" |
                "cache-control" | "pragma" | "upgrade" | "expect") || n.starts_with("if-") || n.starts_with("sec-websocket-") || n == "connection" && req.headers().get_all("connection").iter().any(|v| v.to_str().map_or(true, |s| s.split(',').any(|x| x.trim().eq_ignore_ascii_case("upgrade"))))
        })
}

fn freshness(headers: &HeaderMap, cap: u64) -> Option<(u64, Duration)> {
    if ["set-cookie", "vary", "content-encoding", "content-range", "www-authenticate", "trailer"]
        .iter().any(|h| headers.contains_key(*h)) { return None; }
    let mut public = false;
    let mut max_age = None;
    for value in headers.get_all(header::CACHE_CONTROL).iter() {
        for directive in value.to_str().ok()?.split(',').map(str::trim) {
            let d = directive.to_ascii_lowercase();
            if d == "public" { public = true; }
            else if let Some(n) = d.strip_prefix("max-age=") {
                if max_age.is_some() { return None; }
                max_age = Some(n.parse::<u64>().ok()?);
            } else { return None; } // intentionally conservative: no other directives
        }
    }
    if !public { return None; }
    if headers.get_all(header::AGE).iter().count() > 1 || headers.get_all(header::DATE).iter().count() != 1 { return None; }
    let age = headers.get(header::AGE).map(|v| v.to_str().ok()?.parse::<u64>().ok()).unwrap_or(Some(0))?;
    let date = httpdate::parse_http_date(headers.get(header::DATE)?.to_str().ok()?).ok()?;
    let apparent = SystemTime::now().duration_since(date).ok()?.as_secs();
    let age = age.max(apparent);
    // Round conservatively and never extend origin freshness.
    let ttl = max_age?.saturating_sub(age.saturating_add(1)).min(cap);
    if ttl == 0 { None } else { Some((age, Duration::from_secs(ttl))) }
}

impl Cache {
    pub fn new(config: CacheConfig) -> Self { Self { config, entries: Mutex::new(HashMap::new()), epoch: AtomicU64::new(0), hits: AtomicU64::new(0), misses: AtomicU64::new(0) } }
    pub fn epoch(&self) -> u64 { self.epoch.load(Ordering::Acquire) }
    pub fn stats(&self) -> CacheStats {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, e| e.at.elapsed() < e.ttl);
        CacheStats { entries: entries.len(), body_bytes: entries.values().map(|e| e.body.len()).sum(),
            hits: self.hits.load(Ordering::Relaxed), misses: self.misses.load(Ordering::Relaxed) }
    }
    pub fn purge(&self) -> usize {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let count = entries.len();
        entries.clear();
        count
    }
    pub fn get(&self, key: &str) -> Option<Response> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let expired = entries.get(key).is_some_and(|e| e.at.elapsed() >= e.ttl);
        if expired { entries.remove(key); }
        let Some(e) = entries.get(key) else { self.misses.fetch_add(1, Ordering::Relaxed); return None; };
        self.hits.fetch_add(1, Ordering::Relaxed);
        let mut response = Response::new(Body::from(e.body.clone()));
        *response.headers_mut() = e.headers.clone();
        let age = e.age.saturating_add(e.at.elapsed().as_secs());
        response.headers_mut().insert(header::AGE, HeaderValue::from_str(&age.to_string()).ok()?);
        Some(response)
    }
    pub async fn store(&self, key: String, response: Response, deadline: Duration, epoch: u64) -> Response {
        if response.status() != StatusCode::OK || key.len() > 4096 ||
            response.body().size_hint().exact().is_none_or(|n| n > self.config.max_body_bytes as u64) {
            return response;
        }
        let header_size: usize = response.headers().iter().map(|(k,v)| k.as_str().len()+v.as_bytes().len()+32).sum();
        if header_size > 8192 { return response; }
        let Some((age, ttl)) = freshness(response.headers(), self.config.ttl_seconds) else { return response; };
        let at = Instant::now();
        let (parts, body) = response.into_parts();
        let bytes = match tokio::time::timeout(deadline, to_bytes(body, self.config.max_body_bytes)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => return http::Response::builder().status(502).body(Body::empty()).unwrap(),
            Err(_) => return http::Response::builder().status(504).body(Body::empty()).unwrap(),
        };
        if at.elapsed() < ttl {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            if self.epoch() != epoch { return Response::from_parts(parts, Body::from(bytes)); }
            entries.retain(|_, e| e.at.elapsed() < e.ttl);
            if entries.len() >= self.config.max_entries && !entries.contains_key(&key) {
                if let Some(old) = entries.iter().min_by_key(|(_,e)| e.at).map(|(k,_)| k.clone()) { entries.remove(&old); }
            }
            entries.insert(key, Entry { headers: parts.headers.clone(), body: bytes.clone(), at, age, ttl });
        }
        Response::from_parts(parts, Body::from(bytes))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn purge_invalidates_in_flight_store_epoch() {
        let cache = Cache::new(CacheConfig { ttl_seconds: 20, max_entries: 2, max_body_bytes: 100 });
        let response = || http::Response::builder().header("date", httpdate::fmt_http_date(SystemTime::now()))
            .header("cache-control", "public, max-age=30").body(Body::from("hello")).unwrap();
        let epoch = cache.epoch();
        cache.store("before".into(), response(), Duration::from_secs(1), epoch).await;
        assert_eq!(cache.purge(), 1);
        cache.store("stale".into(), response(), Duration::from_secs(1), epoch).await;
        assert!(cache.get("before").is_none());
        assert!(cache.get("stale").is_none());
        cache.store("fresh".into(), response(), Duration::from_secs(1), cache.epoch()).await;
        assert!(cache.get("fresh").is_some());
        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 2);
    }

    #[tokio::test]
    async fn only_explicit_public_responses_are_cached() {
        let cache = Cache::new(CacheConfig { ttl_seconds: 20, max_entries: 2, max_body_bytes: 100 });
        let response = |directive: &str| http::Response::builder()
            .header("date", httpdate::fmt_http_date(SystemTime::now()))
            .header("cache-control", directive).body(Body::from("hello")).unwrap();
        cache.store("private".into(), response("private, max-age=30"), Duration::from_secs(1), cache.epoch()).await;
        assert!(cache.get("private").is_none());
        cache.store("public".into(), response("public, max-age=30"), Duration::from_secs(1), cache.epoch()).await;
        assert!(cache.get("public").is_some());
        assert!(!eligible(&Request::builder().header("cookie","session=x").body(Body::empty()).unwrap()));
    }
}
