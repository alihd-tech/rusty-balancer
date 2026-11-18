//! Dedicated loopback management API, absent unless configured.
use anyhow::{bail, Context, Result};
use axum::{extract::{DefaultBodyLimit, State, Request}, http::{header, StatusCode},
    middleware::{self, Next}, response::{IntoResponse, Response}, routing::{get, post, put}, Json, Router};
use axum_server::tls_rustls::RustlsConfig;
use balancer_config::{Config, MAX_CONFIG_BYTES};
use balancer_http::control::Control;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use std::{io::Read, path::{Path, PathBuf}, sync::Arc};

pub struct Token([u8;32]);
impl Token {
    pub fn load(path: &Path) -> Result<Self> {
        let meta = std::fs::symlink_metadata(path).context("inspect admin token file")?;
        if !meta.is_file() || meta.file_type().is_symlink() { bail!("admin token must be a regular non-symlink file"); }
        let file = std::fs::File::open(path).context("open admin token file")?;
        let meta = file.metadata()?;
        if !meta.is_file() { bail!("admin token must be a regular file"); }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 { bail!("admin token file must be private (chmod 600)"); }
        }
        let mut raw = String::new();
        file.take(259).read_to_string(&mut raw)?;
        if raw.len() > 258 { bail!("admin token file too large"); }
        let raw = raw.trim_end_matches(['\r','\n']);
        if !(32..=256).contains(&raw.len()) || !raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            bail!("admin token must be 32..=256 URL-safe characters");
        }
        Ok(Self(Sha256::digest(raw.as_bytes()).into()))
    }
    fn matches(&self, candidate: &str) -> bool {
        let digest: [u8;32] = Sha256::digest(candidate.as_bytes()).into();
        bool::from(self.0.ct_eq(&digest))
    }
}
struct Api { control: Arc<Control>, token: Token, config_path: PathBuf,
    manual: Option<RustlsConfig>, mutation: tokio::sync::Mutex<()>, admission: tokio::sync::Semaphore }

pub fn router(control: Arc<Control>, token: Token, config_path: PathBuf, manual: Option<RustlsConfig>) -> Router {
    let state = Arc::new(Api { control, token, config_path, manual, mutation: tokio::sync::Mutex::new(()), admission: tokio::sync::Semaphore::new(8) });
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/status", get(status))
        .route("/v1/upstreams", get(upstreams))
        .route("/v1/routes", get(routes))
        .route("/v1/certificates", get(certificates))
        .route("/v1/certificates/reload", post(reload_certificates))
        .route("/v1/config", put(apply))
        .route("/v1/config/reload", post(reload))
        .route("/v1/config/rollback", post(rollback))
        .route("/v1/cache/purge", post(purge))
        .layer(DefaultBodyLimit::max(7 * MAX_CONFIG_BYTES))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(api): State<Arc<Api>>, request: Request, next: Next) -> Response {
    let mut values = request.headers().get_all(header::AUTHORIZATION).iter();
    let token = values.next().and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    let allowed = values.next().is_none() && token.is_some_and(|t| t.len() <= 256 && api.token.matches(t));
    let mut response = if allowed {
        // Bound the full management request including JSON upload. Mutations may finish once started.
        match api.admission.try_acquire() {
            Ok(_permit) => match tokio::time::timeout(std::time::Duration::from_secs(15), next.run(request)).await {
                Ok(response) => response, Err(_) => error(StatusCode::REQUEST_TIMEOUT, "management deadline exceeded"),
            },
            Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "management capacity reached"),
        }
    } else {
        (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], Json(json!({"error":"unauthorized"}))).into_response()
    };
    response.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
}
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":message}))).into_response()
}
fn revision_result(result: Result<u64>) -> Response {
    match result {
        Ok(revision) => Json(json!({"revision":revision,"persisted":false})).into_response(),
        Err(e) => {
            let _ = e;
            tracing::warn!("configuration mutation rejected");
            error(StatusCode::CONFLICT, "configuration rejected; check revision, restart-only fields and server logs")
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision { expected_revision: u64 }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Apply { expected_revision: u64, toml: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Purge { expected_revision: u64, route: Option<usize> }

async fn live() -> Json<serde_json::Value> { Json(json!({"live":true})) }
async fn ready(State(api): State<Arc<Api>>) -> Response {
    let s = api.control.snapshot();
    (if s.ready {StatusCode::OK} else {StatusCode::SERVICE_UNAVAILABLE},
        Json(json!({"ready":s.ready,"revision":s.revision}))).into_response()
}
async fn status(State(api): State<Arc<Api>>) -> impl IntoResponse { Json(api.control.snapshot()) }
async fn upstreams(State(api): State<Arc<Api>>) -> impl IntoResponse {
    let s = api.control.snapshot(); Json(json!({"revision":s.revision,"pools":s.pools}))
}
async fn routes(State(api): State<Arc<Api>>) -> impl IntoResponse {
    let s = api.control.snapshot(); Json(json!({"revision":s.revision,"routes":s.routes}))
}
async fn metrics(State(api): State<Arc<Api>>) -> Response {
    ([(header::CONTENT_TYPE,"text/plain; version=0.0.4; charset=utf-8")], api.control.prometheus()).into_response()
}
async fn certificates(State(api): State<Arc<Api>>) -> impl IntoResponse {
    let g = api.control.current();
    let tls = g.config.tls.as_ref().map(|t| json!({
        "mode":if t.acme.is_some() {"acme"} else {"manual"},
        "configured_domains":t.domains(),
        "manual_reload_available":api.manual.is_some()
    }));
    Json(json!({"revision":g.revision,"tls":tls}))
}
async fn apply(State(api): State<Arc<Api>>, Json(body): Json<Apply>) -> Response {
    let _guard = api.mutation.lock().await;
    let config = match Config::parse(&body.toml) {
        Ok(config) => config, Err(_) => return error(StatusCode::UNPROCESSABLE_ENTITY,"invalid TOML or policy"),
    };
    revision_result(api.control.apply(body.expected_revision, config))
}
async fn reload(State(api): State<Arc<Api>>, Json(body): Json<Revision>) -> Response {
    let _guard = api.mutation.lock().await;
    let path = api.config_path.clone();
    let config = match tokio::task::spawn_blocking(move || Config::load(path)).await {
        Ok(Ok(config)) => config, _ => return error(StatusCode::UNPROCESSABLE_ENTITY,"configured file cannot be loaded or validated"),
    };
    revision_result(api.control.apply(body.expected_revision, config))
}
async fn rollback(State(api): State<Arc<Api>>, Json(body): Json<Revision>) -> Response {
    let _guard = api.mutation.lock().await;
    revision_result(api.control.rollback(body.expected_revision))
}
async fn purge(State(api): State<Arc<Api>>, Json(body): Json<Purge>) -> Response {
    let _guard = api.mutation.lock().await;
    match api.control.purge(body.expected_revision, body.route) {
        Ok(removed) => { tracing::info!(removed, "cache purged"); Json(json!({"removed":removed,"revision":body.expected_revision})).into_response() }
        Err(_) => error(StatusCode::CONFLICT,"revision conflict or route has no cache"),
    }
}
async fn reload_certificates(State(api): State<Arc<Api>>, Json(body): Json<Revision>) -> Response {
    let _guard = api.mutation.lock().await;
    let g = api.control.current();
    if g.revision != body.expected_revision { return error(StatusCode::CONFLICT,"revision conflict"); }
    let (Some(tls), Some(handle)) = (&g.config.tls, &api.manual) else {
        return error(StatusCode::CONFLICT,"manual TLS not enabled");
    };
    match balancer_tls::reload_manual(tls, handle) {
        Ok(()) => { tracing::info!("manual certificates reloaded"); Json(json!({"reloaded":true,"revision":g.revision})).into_response() }
        Err(_) => error(StatusCode::UNPROCESSABLE_ENTITY,"certificate validation failed; previous certificates retained"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;
    #[tokio::test]
    async fn auth_and_revision_contract() {
        let config = "listen = \"127.0.0.1:8080\"\n[[routes]]\nstatic_dir = \"public\"";
        let control = Control::new(Config::parse(config).unwrap());
        let secret = "01234567890123456789012345678901";
        let app = router(control.clone(), Token(Sha256::digest(secret).into()), "unused".into(), None);
        let response = app.clone().oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let request = |revision| Request::builder().method("PUT").uri("/v1/config")
            .header("authorization",format!("Bearer {secret}")).header("content-type","application/json")
            .body(Body::from(json!({"expected_revision":revision,"toml":config}).to_string())).unwrap();
        let response = app.clone().oneshot(request(1)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"],"no-store");
        assert_eq!(app.oneshot(request(1)).await.unwrap().status(), StatusCode::CONFLICT);
        assert_eq!(control.current().revision, 2);
        let app = router(control.clone(), Token(Sha256::digest(secret).into()), "unused".into(), None);
        let bad = Request::builder().method("PUT").uri("/v1/config").header("authorization",format!("Bearer {secret}"))
            .header("content-type","application/json").body(Body::from(json!({"expected_revision":2,"toml":"not valid TOML"}).to_string())).unwrap();
        assert_eq!(app.oneshot(bad).await.unwrap().status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(control.current().revision, 2);
    }
}
