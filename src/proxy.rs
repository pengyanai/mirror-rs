use crate::config::Mirror;
use crate::router::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

/// Hop-by-hop headers that must not be forwarded between client and upstream.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

#[derive(Clone)]
pub struct AppState {
    pub client: reqwest::Client,
    pub router: Router,
}

enum UpstreamError {
    Http(u16),
    Conn(String),
}

impl UpstreamError {
    fn is_404(&self) -> bool {
        matches!(self, UpstreamError::Http(404))
    }
    fn describe(&self) -> String {
        match self {
            UpstreamError::Http(c) => format!("HTTP {c}"),
            UpstreamError::Conn(m) => m.clone(),
        }
    }
}

/// Build the axum router with the proxy as the fallback (catch-all) plus
/// internal endpoints.
pub fn build_router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/healthz", axum::routing::get(healthz))
        .route("/__mirror__/status", axum::routing::get(status))
        .route("/__mirror__/probe", axum::routing::get(probe_now))
        .fallback(proxy_handler)
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn status(State(state): State<AppState>) -> Response {
    let order = state.router.order.read().await;
    let results = state.router.results.read().await;
    let mirrors = &state.router.mirrors;
    let payload: Vec<serde_json::Value> = order
        .iter()
        .map(|&i| {
            let m = &mirrors[i];
            let r = results.get(i);
            serde_json::json!({
                "url": m.url,
                "rank": i,
                "ok": r.map(|x| x.ok).unwrap_or(false),
                "ttfb_ms": r.map(|x| x.ttfb_ms).unwrap_or(0),
                "throughput_bps": r.map(|x| x.throughput_bps).unwrap_or(0),
                "score": r.map(|x| x.score()).unwrap_or(f64::INFINITY),
            })
        })
        .collect();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "order": payload })),
    )
        .into_response()
}

async fn probe_now(State(state): State<AppState>) -> Response {
    let mirrors = state.router.mirrors.clone();
    let results = crate::probe::probe_all(&state.client, &mirrors, 256 * 1024, 8000).await;
    state.router.update_from_probes(results.clone()).await;
    let payload: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "url": r.url,
                "ok": r.ok,
                "ttfb_ms": r.ttfb_ms,
                "throughput_bps": r.throughput_bps,
                "bytes": r.bytes_downloaded,
                "score": r.score(),
                "error": r.error,
            })
        })
        .collect();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "results": payload })),
    )
        .into_response()
}

/// The transparent proxy handler: route to the best mirror, fail over on error.
async fn proxy_handler(State(state): State<AppState>, req: Request) -> Response {
    let method = req.method().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    // Forwarded headers (strip hop-by-hop + host).
    let mut fwd = HeaderMap::new();
    for (k, v) in req.headers() {
        let lk = k.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lk.as_str()) || lk == "host" {
            continue;
        }
        fwd.append(k, v.clone());
    }

    // Read request body (pulls are GET/HEAD => empty; bounded for safety).
    let body = match axum::body::to_bytes(req.into_body(), 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "body too large").into_response();
        }
    };

    let mirrors = state.router.ordered_mirrors().await;
    let mut last_err: Option<UpstreamError> = None;
    let mut all_404 = true;

    for mirror in &mirrors {
        let t0 = std::time::Instant::now();
        match forward_once(&state.client, mirror, &method, &path_and_query, &fwd, &body).await {
            Ok(resp) => {
                tracing::info!(
                    mirror = %mirror.url,
                    method = %method,
                    path = %path_and_query,
                    ms = t0.elapsed().as_millis() as u64,
                    "served request"
                );
                return resp;
            }
            Err(e) => {
                if !e.is_404() {
                    all_404 = false;
                }
                tracing::warn!(mirror = %mirror.url, err = %e.describe(), "upstream failed, failing over");
                last_err = Some(e);
            }
        }
    }

    // All mirrors failed.
    let is_404 = last_err.as_ref().map(|e| e.is_404()).unwrap_or(false);
    let msg = last_err
        .map(|e| e.describe())
        .unwrap_or_else(|| "no mirrors configured".into());
    let status = if all_404 && is_404 {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_GATEWAY
    };
    tracing::error!(%msg, "all mirrors failed");
    (status, format!("mirror-rs: all upstreams failed: {msg}")).into_response()
}

/// Forward a single request to one mirror, handling the Docker token dance.
async fn forward_once(
    client: &reqwest::Client,
    mirror: &Mirror,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response, UpstreamError> {
    let base = mirror.url.trim_end_matches('/');
    let url = format!("{base}{path_and_query}");

    let resp = send(client, mirror, method, &url, headers, body).await?;
    let status = resp.status();

    // 401 => perform the registry token dance and retry once.
    if status.as_u16() == 401 {
        if let Some(wa) = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
        {
            if let Some(token) = crate::probe::fetch_token(client, wa).await {
                let mut h2 = headers.clone();
                h2.insert("Authorization", format!("Bearer {token}").parse().unwrap());
                let resp2 = send(client, mirror, method, &url, &h2, body).await?;
                return to_axum_response(resp2).await;
            }
        }
        return Err(UpstreamError::Http(401));
    }

    // 404 / 5xx => soft failure, let the caller try the next mirror.
    if status.as_u16() == 404 || status.is_server_error() {
        return Err(UpstreamError::Http(status.as_u16()));
    }

    to_axum_response(resp).await
}

async fn send(
    client: &reqwest::Client,
    mirror: &Mirror,
    method: &Method,
    url: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<reqwest::Response, UpstreamError> {
    let mut req = client.request(method.clone(), url);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    if !body.is_empty() {
        req = req.body(body.to_vec());
    }
    if let Some(tok) = &mirror.token {
        req = req.header("Authorization", format!("Bearer {tok}"));
    }
    req.send()
        .await
        .map_err(|e| UpstreamError::Conn(e.to_string()))
}

/// Convert a reqwest response into an axum response, streaming the body.
async fn to_axum_response(resp: reqwest::Response) -> Result<Response, UpstreamError> {
    let status = resp.status();
    let mut builder = Response::builder().status(status);
    for (k, v) in resp.headers() {
        let lk = k.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lk.as_str()) {
            continue;
        }
        builder = builder.header(k, v);
    }
    let stream = resp.bytes_stream();
    let body = Body::from_stream(stream);
    builder
        .body(body)
        .map_err(|e| UpstreamError::Conn(format!("build response: {e}")))
}
