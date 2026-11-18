use axum::{body::Body, extract::{ConnectInfo, Request, State}, response::{IntoResponse, Response}, Router};
use balancer_config::{hostname, path_matches, Config, RouteConfig};
use balancer_lb::Pool;
use balancer_proxy::HttpClient;
use http::{header, StatusCode, Uri};
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::task::JoinHandle;
use tower::ServiceExt;
use tower_http::services::ServeDir;

struct Runtime {
    routes: Vec<RouteConfig>,
    pools: BTreeMap<String, Arc<Pool>>,
    client: HttpClient,
    timeout: Duration,
}

pub struct Server {
    pub router: Router,
    health_tasks: Vec<JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        for task in &self.health_tasks { task.abort(); }
    }
}

pub fn build(config: Config) -> Server {
    let client = balancer_proxy::client();
    let mut pools = BTreeMap::new();
    let mut health_tasks = Vec::new();
    for (name, pool_cfg) in &config.pools {
        let pool = Arc::new(Pool::new(pool_cfg));
        if let Some(health) = &pool_cfg.health {
            for endpoint in pool.endpoints() {
                health_tasks.push(tokio::spawn(balancer_proxy::health_worker(
                    client.clone(), Arc::clone(endpoint), health.clone())));
            }
        }
        pools.insert(name.clone(), pool);
    }
    let runtime = Arc::new(Runtime {
        routes: config.routes, pools, client,
        timeout: Duration::from_millis(config.upstream_timeout_ms),
    });
    let router = Router::new().fallback(dispatch).with_state(runtime);
    Server { router, health_tasks }
}

fn choose_route<'a>(routes: &'a [RouteConfig], host: Option<&str>, path: &str) -> Option<&'a RouteConfig> {
    routes.iter().filter(|r| {
        (r.host.is_none() || r.host.as_deref() == host) && path_matches(&r.path_prefix, path)
    }).max_by_key(|r| (r.host.is_some(), r.path_prefix.len()))
}

async fn dispatch(State(runtime): State<Arc<Runtime>>, ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request) -> Response {
    let path = request.uri().path();
    if !path.starts_with('/') { return StatusCode::BAD_REQUEST.into_response(); }
    let authority = request.uri().authority().map(|a| a.as_str());
    let mut values = request.headers().get_all(header::HOST).iter();
    let host_header = match values.next() {
        Some(value) => match value.to_str() {
            Ok(value) => Some(value),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        },
        None => None,
    };
    if values.next().is_some() { return StatusCode::BAD_REQUEST.into_response(); }
    if let (Some(a), Some(b)) = (authority, host_header) {
        if !a.eq_ignore_ascii_case(b) { return StatusCode::BAD_REQUEST.into_response(); }
    }
    let raw_host = authority.or(host_header);
    let host = raw_host.and_then(hostname);
    if raw_host.is_some() && host.is_none() { return StatusCode::BAD_REQUEST.into_response(); }
    let Some(route) = choose_route(&runtime.routes, host.as_deref(), path) else {
        return (StatusCode::NOT_FOUND, "no matching route").into_response();
    };
    if let Some(pool) = &route.pool {
        let forwarded_host = raw_host.map(str::to_owned);
        return balancer_proxy::forward(&runtime.client, &runtime.pools[pool], request,
            peer.ip(), forwarded_host, runtime.timeout).await;
    }
    if let Some(dir) = &route.static_dir {
        // A mounted static directory does not include its route prefix on disk.
        let suffix = path.strip_prefix(&route.path_prefix).unwrap_or(path);
        let mut target = if route.path_prefix == "/" { path.to_owned() }
            else if suffix.is_empty() { "/".to_owned() } else { suffix.to_owned() };
        if let Some(query) = request.uri().query() { target.push('?'); target.push_str(query); }
        let Ok(uri) = target.parse::<Uri>() else { return StatusCode::BAD_REQUEST.into_response(); };
        *request.uri_mut() = uri;
        return match ServeDir::new(dir).oneshot(request).await {
            Ok(response) => response.map(Body::new),
            Err(never) => match never {},
        };
    }
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Request;
    use axum::body::to_bytes;
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
        assert_eq!(choose_route(&cfg.routes, Some("example.test"), "/api/x").unwrap().path_prefix, "/api");
        assert_eq!(choose_route(&cfg.routes, Some("example.test"), "/apix").unwrap().path_prefix, "/");
        assert!(choose_route(&cfg.routes, Some("other.test"), "/api").unwrap().host.is_none());
    }
    #[tokio::test]
    async fn forwards_path_query_and_replaces_spoofed_header() {
        let upstream = Router::new().fallback(|req: axum::extract::Request| async move {
            format!("{}|{}", req.uri(), req.headers()["x-forwarded-for"].to_str().unwrap())
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap(); });
        let cfg = Config::parse(&format!(r#"
listen = "127.0.0.1:8080"
[pools.backend]
[[pools.backend.endpoints]]
url = "http://{addr}"
[[routes]]
pool = "backend"
"#)).unwrap();
        let server = build(cfg);
        let mut req = Request::builder().uri("/api?q=yes")
            .header("host", "example.test").header("x-forwarded-for", "spoofed")
            .body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo("192.0.2.1:1234".parse::<SocketAddr>().unwrap()));
        let response = server.router.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"/api?q=yes|192.0.2.1");
        task.abort();
    }
}
