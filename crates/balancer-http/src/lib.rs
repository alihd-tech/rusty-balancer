use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    response::{IntoResponse, Response},
    Extension, Router,
};
use balancer_config::{hostname, path_matches, Config, RouteConfig, TlsConfig};
use balancer_lb::Pool;
use balancer_middleware::{cache::Cache, rate::RateLimiter};
use balancer_proxy::HttpClient;
use http::{header, StatusCode, Uri};
use ipnet::IpNet;
use std::{
    collections::{BTreeMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use std::{io::ErrorKind, path::Path};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tower::ServiceExt;
use tower_http::{compression::CompressionLayer, services::ServeDir};
pub mod control;

struct Runtime {
    routes: Vec<RouteConfig>,
    pools: BTreeMap<String, Arc<Pool>>,
    client: HttpClient,
    timeout: Duration,
    policies: Vec<PolicyRuntime>,
    in_flight: Arc<Semaphore>,
    idle: Duration,
    security_headers: bool,
    trusted_proxies: Vec<IpNet>,
}

struct PolicyRuntime {
    rate: Option<RateLimiter>,
    cache: Option<Cache>,
}

pub struct Server {
    pub router: Router,
    runtime: Arc<Runtime>,
    health_tasks: Vec<JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        for task in &self.health_tasks {
            task.abort();
        }
    }
}

pub fn build(config: Config) -> Server {
    let permits = Arc::new(Semaphore::new(config.traffic.max_in_flight));
    build_with_permits(config, permits)
}

fn build_with_permits(config: Config, permits: Arc<Semaphore>) -> Server {
    let client = balancer_proxy::client();
    let mut pools = BTreeMap::new();
    let mut health_tasks = Vec::new();
    for (name, pool_cfg) in &config.pools {
        let pool = Arc::new(Pool::new(pool_cfg));
        if let Some(health) = &pool_cfg.health {
            for endpoint in pool.endpoints() {
                health_tasks.push(tokio::spawn(balancer_proxy::health_worker(
                    client.clone(),
                    Arc::clone(endpoint),
                    health.clone(),
                )));
            }
        }
        pools.insert(name.clone(), pool);
    }
    let policies = config
        .routes
        .iter()
        .map(|r| PolicyRuntime {
            rate: r.policy.rate_limit.clone().map(RateLimiter::new),
            cache: r.policy.cache.clone().map(Cache::new),
        })
        .collect();
    let compression = config.traffic.compression;
    let runtime = Arc::new(Runtime {
        routes: config.routes,
        pools,
        client,
        timeout: Duration::from_millis(config.upstream_timeout_ms),
        policies,
        in_flight: permits,
        idle: Duration::from_millis(config.traffic.response_idle_ms),
        security_headers: config.traffic.security_headers,
        trusted_proxies: config.traffic.trusted_proxies,
    });
    let router = Router::new().fallback(dispatch).with_state(runtime.clone());
    let router = if compression {
        router.layer(
            CompressionLayer::new()
                .gzip(true)
                .br(true)
                .zstd(true)
                .no_deflate(),
        )
    } else {
        router
    };
    Server {
        router,
        runtime,
        health_tasks,
    }
}

fn choose_route<'a>(
    routes: &'a [RouteConfig],
    host: Option<&str>,
    path: &str,
) -> Option<&'a RouteConfig> {
    routes
        .iter()
        .filter(|r| {
            (r.host.is_none() || r.host.as_deref() == host) && path_matches(&r.path_prefix, path)
        })
        .max_by_key(|r| (r.host.is_some(), r.path_prefix.len()))
}

#[derive(Clone)]
struct HttpsIngress(Arc<HashSet<String>>);

pub fn https_router(router: Router, tls: &TlsConfig) -> Router {
    router.layer(Extension(HttpsIngress(Arc::new(
        tls.domains().into_iter().collect(),
    ))))
}

// Only validated, canonical configured names may appear in redirect Locations.
pub fn redirect_router(tls: &TlsConfig) -> Router {
    let domains: Arc<HashSet<String>> = Arc::new(tls.domains().into_iter().collect());
    let port = tls.external_port();
    Router::new().fallback(move |request: Request| {
        let domains = domains.clone();
        async move {
            if request.method() == http::Method::CONNECT || !request.uri().path().starts_with('/') {
                return StatusCode::BAD_REQUEST.into_response();
            }
            let host = match request_host(&request) {
                Ok(Some(raw)) => hostname(raw),
                _ => None,
            };
            let Some(host) = host.filter(|h| domains.contains(h)) else {
                return StatusCode::MISDIRECTED_REQUEST.into_response();
            };
            let authority = if port == 443 {
                host
            } else {
                format!("{host}:{port}")
            };
            let path = request
                .uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/");
            let location = format!("https://{authority}{path}");
            match http::HeaderValue::from_str(&location) {
                Ok(value) => {
                    (StatusCode::PERMANENT_REDIRECT, [(header::LOCATION, value)]).into_response()
                }
                Err(_) => StatusCode::BAD_REQUEST.into_response(),
            }
        }
    })
}

fn request_host(request: &Request) -> Result<Option<&str>, StatusCode> {
    let authority = request.uri().authority().map(|a| a.as_str());
    let mut values = request.headers().get_all(header::HOST).iter();
    let host_header = values
        .next()
        .map(|v| v.to_str())
        .transpose()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    if values.next().is_some() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let (Some(a), Some(b)) = (authority, host_header) {
        if !a.eq_ignore_ascii_case(b) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    Ok(authority.or(host_header))
}

async fn dispatch(
    State(runtime): State<Arc<Runtime>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ingress: Option<Extension<HttpsIngress>>,
    mut request: Request,
) -> Response {
    let secure = ingress.is_some();
    let mut response = dispatch_inner(&runtime, peer, ingress, &mut request).await;
    if runtime.security_headers {
        balancer_middleware::security_headers(response.headers_mut(), secure);
    }
    response
}

async fn dispatch_inner(
    runtime: &Runtime,
    peer: SocketAddr,
    ingress: Option<Extension<HttpsIngress>>,
    request: &mut Request,
) -> Response {
    let path = request.uri().path();
    if request.method() == http::Method::CONNECT {
        return StatusCode::NOT_IMPLEMENTED.into_response();
    }
    if path.len() > 8192
        || request
            .uri()
            .path_and_query()
            .is_some_and(|p| p.as_str().len() > 8192)
    {
        return StatusCode::URI_TOO_LONG.into_response();
    }
    if !path.starts_with('/') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let raw_host = match request_host(request) {
        Ok(host) => host,
        Err(status) => return status.into_response(),
    };
    let client_ip = match balancer_middleware::client_ip(
        peer.ip(),
        request.headers(),
        &runtime.trusted_proxies,
    ) {
        Ok(ip) => ip,
        Err(status) => return status.into_response(),
    };
    let host = raw_host.and_then(hostname);
    if raw_host.is_some() && host.is_none() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(Extension(https)) = &ingress {
        if !host.as_ref().is_some_and(|host| https.0.contains(host)) {
            return StatusCode::MISDIRECTED_REQUEST.into_response();
        }
    }
    // Do not let a Connection header suppress credentials or other end-to-end policy headers.
    if request
        .headers()
        .get_all(header::CONNECTION)
        .iter()
        .any(|value| {
            value.to_str().map_or(true, |v| {
                v.split(',').any(|token| {
                    let token = token.trim().to_ascii_lowercase();
                    !matches!(
                        token.as_str(),
                        "close" | "keep-alive" | "upgrade" | "te" | "trailer"
                    )
                })
            })
        })
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(route) = choose_route(&runtime.routes, host.as_deref(), path) else {
        return (StatusCode::NOT_FOUND, "no matching route").into_response();
    };
    let path = path.to_owned();
    let forwarded_host = raw_host.map(str::to_owned);
    let index = runtime
        .routes
        .iter()
        .position(|r| std::ptr::eq(r, route))
        .expect("matched route");
    let policy = &route.policy;
    let state = &runtime.policies[index];
    if !balancer_middleware::allowed(policy, client_ip) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if state.rate.as_ref().is_some_and(|r| !r.admit(client_ip)) {
        return (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "1")]).into_response();
    }
    let Ok(permit) = runtime.in_flight.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let request = std::mem::replace(request, Request::new(Body::empty()));
    let mut request = match balancer_middleware::bound_request(request, policy).await {
        Ok(request) => request,
        Err(response) => return *response,
    };
    let secure = ingress.is_some();
    if let Some(pool) = &route.pool {
        if request.headers().contains_key(header::UPGRADE) {
            if !policy.websocket {
                return StatusCode::NOT_IMPLEMENTED.into_response();
            }
            return balancer_proxy::websocket::forward(
                &runtime.client,
                &runtime.pools[pool],
                request,
                client_ip,
                forwarded_host,
                secure,
                runtime.timeout,
                Duration::from_secs(policy.websocket_max_seconds.unwrap_or(3600)),
                permit,
            )
            .await;
        }
        let cache_key = state
            .cache
            .as_ref()
            .filter(|_| balancer_middleware::cache::eligible(&request))
            .map(|_| {
                format!(
                    "{}|{}|{}",
                    secure,
                    forwarded_host.as_deref().unwrap_or(""),
                    request
                        .uri()
                        .path_and_query()
                        .map(|p| p.as_str())
                        .unwrap_or("/")
                )
            });
        let epoch = state.cache.as_ref().map(Cache::epoch).unwrap_or(0);
        if let (Some(cache), Some(key)) = (&state.cache, &cache_key) {
            if let Some(response) = cache.get(key) {
                return balancer_middleware::limits::guard(response, permit, runtime.idle);
            }
        }
        let response = balancer_proxy::forward(
            &runtime.client,
            &runtime.pools[pool],
            request,
            client_ip,
            forwarded_host,
            secure,
            runtime.timeout,
        )
        .await;
        let response = match (&state.cache, cache_key) {
            (Some(cache), Some(key)) => cache.store(key, response, runtime.idle, epoch).await,
            _ => response,
        };
        return balancer_middleware::limits::guard(response, permit, runtime.idle);
    }
    if let Some(dir) = &route.static_dir {
        if request.headers().contains_key(header::UPGRADE) {
            return StatusCode::NOT_IMPLEMENTED.into_response();
        }
        // A mounted static directory does not include its route prefix on disk.
        let suffix = path
            .strip_prefix(route.path_prefix.as_str())
            .unwrap_or(path.as_str());
        let mut target = if route.path_prefix == "/" {
            path.to_owned()
        } else if suffix.is_empty() {
            "/".to_owned()
        } else {
            suffix.to_owned()
        };
        if let Some(query) = request.uri().query() {
            target.push('?');
            target.push_str(query);
        }
        let Ok(uri) = target.parse::<Uri>() else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if !static_path_safe(dir, uri.path()).await {
            return StatusCode::FORBIDDEN.into_response();
        }
        *request.uri_mut() = uri;
        return match ServeDir::new(dir).oneshot(request).await {
            Ok(mut response) => {
                // ServeDir redirects directory requests to a trailing slash.
                // Preserve the external mount prefix in root-relative redirects.
                if route.path_prefix != "/" {
                    let location = response
                        .headers()
                        .get(header::LOCATION)
                        .and_then(|value| value.to_str().ok())
                        .filter(|value| value.starts_with('/') && !value.starts_with("//"))
                        .and_then(|value| {
                            http::HeaderValue::from_str(&format!("{}{value}", route.path_prefix))
                                .ok()
                        });
                    if let Some(location) = location {
                        response.headers_mut().insert(header::LOCATION, location);
                    }
                }
                balancer_middleware::limits::guard(response.map(Body::new), permit, runtime.idle)
            }
            Err(never) => match never {},
        };
    }
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

// ServeDir decodes URL escapes and follows symlinks. Protect the configured
// static root against traversal and symlink escapes. Static directories must
// still be immutable to untrusted users (TOCTOU is not prevented here).
async fn static_path_safe(root: &Path, encoded_path: &str) -> bool {
    let decoded = match percent_encoding::percent_decode_str(encoded_path).decode_utf8() {
        Ok(path) => path,
        Err(_) => return false,
    };
    let relative = decoded.trim_start_matches('/');
    let mut current = root.to_path_buf();
    match tokio::fs::symlink_metadata(&current).await {
        Ok(meta) if meta.file_type().is_symlink() => return false,
        Err(error) if error.kind() != ErrorKind::NotFound => return false,
        _ => {}
    }
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains('\\') || segment.contains('\0') {
            return false;
        }
        current.push(segment);
        match tokio::fs::symlink_metadata(&current).await {
            Ok(meta) if meta.file_type().is_symlink() => return false,
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(_) => return false,
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use http::Request;
    #[cfg(unix)]
    #[tokio::test]
    async fn denies_static_symlink_escape_and_encoded_traversal() {
        use std::os::unix::fs::symlink;
        let temp = std::env::temp_dir().join(format!(
            "rusty-static-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = temp.join("public");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("safe.txt"), "ok").unwrap();
        std::fs::write(temp.join("secret.txt"), "private").unwrap();
        symlink(temp.join("secret.txt"), root.join("link.txt")).unwrap();
        assert!(static_path_safe(&root, "/safe.txt").await);
        assert!(!static_path_safe(&root, "/link.txt").await);
        assert!(!static_path_safe(&root, "/%2e%2e/secret.txt").await);
        std::fs::remove_dir_all(temp).unwrap();
    }
    const ROUTES: &str = r#"
listen = "127.0.0.1:8080"
[[routes]]
static_dir = "public"
[[routes]]
host = "example.test"
static_dir = "host"
[[routes]]
host = "example.test"
path_prefix = "/api"
static_dir = "api"
"#;
    #[test]
    fn exact_host_and_longest_path_win() {
        let cfg = Config::parse(ROUTES).unwrap();
        assert_eq!(
            choose_route(&cfg.routes, Some("example.test"), "/api/x")
                .unwrap()
                .path_prefix,
            "/api"
        );
        assert_eq!(
            choose_route(&cfg.routes, Some("example.test"), "/apix")
                .unwrap()
                .path_prefix,
            "/"
        );
        assert!(choose_route(&cfg.routes, Some("other.test"), "/api")
            .unwrap()
            .host
            .is_none());
    }
    #[tokio::test]
    async fn rejects_connection_nominated_credentials_and_long_targets() {
        let server = build(Config::parse(ROUTES).unwrap());
        let make = |path: &str, connection: &str| {
            let mut r = Request::builder()
                .uri(path)
                .header("connection", connection)
                .body(Body::empty())
                .unwrap();
            r.extensions_mut()
                .insert(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()));
            r
        };
        assert_eq!(
            server
                .router
                .clone()
                .oneshot(make("/", "authorization"))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            server
                .router
                .clone()
                .oneshot(make(&format!("/{}", "a".repeat(8192)), "close"))
                .await
                .unwrap()
                .status(),
            StatusCode::URI_TOO_LONG
        );
    }
    #[tokio::test]
    async fn redirect_preserves_path_and_rejects_unknown_host() {
        let cfg = Config::parse(
            r#"
listen = "127.0.0.1:8080"
[tls]
listen = "127.0.0.1:8443"
redirect_http = true
[[tls.certificates]]
domains = ["example.test"]
cert = "cert.pem"
key = "key.pem"
[[routes]]
static_dir = "public"
"#,
        )
        .unwrap();
        let app = redirect_router(cfg.tls.as_ref().unwrap());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/a?q=1")
                    .method("POST")
                    .header("host", "EXAMPLE.TEST:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://example.test:8443/a?q=1"
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("host", "attacker.test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    }

    #[tokio::test]
    async fn serves_static_files_from_a_mount() {
        let cfg = Config::parse(&format!(
            r#"
listen = "127.0.0.1:8080"
[[routes]]
path_prefix = "/assets"
static_dir = "{}/../../examples/public"
"#,
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let server = build(cfg);
        let mut req = Request::builder()
            .uri("/assets/index.html")
            .header("host", "example.test")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("192.0.2.1:1234".parse::<SocketAddr>().unwrap()));
        let response = server.router.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 8192).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("Rusty Balancer"));
    }

    #[tokio::test]
    async fn route_limits_and_security_headers() {
        let cfg = Config::parse(
            r#"
listen = "127.0.0.1:8080"
[traffic]
security_headers = true
max_in_flight = 1
[[routes]]
static_dir = "public"
[routes.policy]
allow = ["192.0.2.0/24"]
max_body_bytes = 4
[routes.policy.rate_limit]
requests_per_second = 1
burst = 1
max_clients = 10
"#,
        )
        .unwrap();
        let server = build(cfg);
        let request = |peer: &str, body: &'static str| {
            let mut req = Request::builder()
                .uri("/")
                .method("POST")
                .body(Body::from(body))
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
            req
        };
        let response = server
            .router
            .clone()
            .oneshot(request("198.51.100.1:1234", ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let response = server
            .router
            .clone()
            .oneshot(request("192.0.2.1:1234", "12345"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let response = server
            .router
            .clone()
            .oneshot(request("192.0.2.1:1235", ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn websocket_upgrade_relays_frames_and_releases_permit() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::net::{TcpListener, TcpStream};
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = upstream.local_addr().unwrap();
        let backend = tokio::spawn(async move {
            let (stream, _) = upstream.accept().await.unwrap();
            let mut io = BufReader::new(stream);
            let mut line = String::new();
            loop {
                line.clear();
                assert!(io.read_line(&mut line).await.unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
            }
            io.get_mut().write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n").await.unwrap();
            let mut frame = [0u8; 8];
            io.read_exact(&mut frame).await.unwrap();
            assert_eq!(frame, [0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2]);
            io.get_mut()
                .write_all(&[0x81, 0x02, b'h', b'i'])
                .await
                .unwrap();
        });
        let cfg = Config::parse(&format!(
            r#"
listen = "127.0.0.1:8080"
[traffic]
max_in_flight = 1
[pools.backend]
[[pools.backend.endpoints]]
url = "http://{addr}"
[[routes]]
pool = "backend"
[routes.policy]
websocket = true
websocket_max_seconds = 5
"#
        ))
        .unwrap();
        let server = build(cfg);
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let router = server.router.clone();
        let task = tokio::spawn(async move {
            axum::serve(
                proxy,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(8), async {
            let mut io = BufReader::new(TcpStream::connect(proxy_addr).await.unwrap());
            io.get_mut().write_all(b"GET /ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").await.unwrap();
            let mut line = String::new();
            io.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("HTTP/1.1 101"));
            loop {
                line.clear();
                assert!(io.read_line(&mut line).await.unwrap() > 0);
                if line == "\r\n" { break; }
            }
            let mut request = Request::builder().uri("/").body(Body::empty()).unwrap();
            request.extensions_mut().insert(ConnectInfo("192.0.2.1:1234".parse::<SocketAddr>().unwrap()));
            let blocked = server.router.clone().oneshot(request).await.unwrap();
            assert_eq!(blocked.status(), StatusCode::SERVICE_UNAVAILABLE);
            io.get_mut().write_all(&[0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2]).await.unwrap();
            let mut echo = [0; 4];
            io.read_exact(&mut echo).await.unwrap();
            assert_eq!(echo, [0x81, 0x02, b'h', b'i']);
            drop(io);
            backend.await.unwrap();
            // The backend is closed; a subsequent normal request should reach it and fail with 502,
            // rather than remain blocked by the tunnel permit.
            loop {
                let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
                req.extensions_mut().insert(ConnectInfo("192.0.2.1:1234".parse::<SocketAddr>().unwrap()));
                let response = server.router.clone().oneshot(req).await.unwrap();
                if response.status() != StatusCode::SERVICE_UNAVAILABLE {
                    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn forwards_path_query_and_replaces_spoofed_header() {
        let upstream = Router::new().fallback(|req: axum::extract::Request| async move {
            format!(
                "{}|{}|{}",
                req.uri(),
                req.headers()["x-forwarded-for"].to_str().unwrap(),
                req.headers()["x-forwarded-proto"].to_str().unwrap()
            )
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let cfg = Config::parse(&format!(
            r#"
listen = "127.0.0.1:8080"
[pools.backend]
[[pools.backend.endpoints]]
url = "http://{addr}"
[[routes]]
pool = "backend"
"#
        ))
        .unwrap();
        let server = build(cfg);
        let mut req = Request::builder()
            .uri("/api?q=yes")
            .header("host", "example.test")
            .header("x-forwarded-for", "spoofed")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("192.0.2.1:1234".parse::<SocketAddr>().unwrap()));
        let response = server.router.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"/api?q=yes|192.0.2.1|http");
        let tls = Config::parse(
            r#"
listen = "127.0.0.1:8080"
[tls]
listen = "127.0.0.1:8443"
[[tls.certificates]]
domains = ["example.test"]
cert = "cert.pem"
key = "key.pem"
[[routes]]
static_dir = "public"
"#,
        )
        .unwrap()
        .tls
        .unwrap();
        let mut req = Request::builder()
            .uri("/api?q=yes")
            .header("host", "example.test")
            .header("x-forwarded-proto", "http")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("192.0.2.1:1234".parse::<SocketAddr>().unwrap()));
        let response = https_router(server.router.clone(), &tls)
            .oneshot(req)
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"/api?q=yes|192.0.2.1|https");
        task.abort();
    }
}
