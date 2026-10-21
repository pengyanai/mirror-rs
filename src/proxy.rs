use crate::config::Mirror;
use crate::router::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;
    use tower::ServiceExt;

    struct Server {
        url: String,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn serve(app: axum::Router) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Server { url, task }
    }
    fn mirror(server: &Server) -> Mirror {
        Mirror {
            url: server.url.clone(),
            token: None,
        }
    }
    fn app(mirrors: Vec<Mirror>) -> axum::Router {
        build_router(AppState {
            client: reqwest::Client::new(),
            router: Router::new(mirrors),
            tokens: crate::auth::TokenCache::default(),
        })
    }
    async fn request(app: axum::Router, path: &str) -> Response {
        app.oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }
    async fn bytes(resp: Response) -> Bytes {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
    }
    async fn authenticated_upstream(
        status: StatusCode,
        expires_in: u64,
    ) -> (Server, Arc<AtomicUsize>) {
        let tokens = Arc::new(AtomicUsize::new(0));
        let count = tokens.clone();
        let upstream = serve(axum::Router::new()
            .route("/token", axum::routing::get(move || { let count = count.clone(); async move {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                axum::Json(serde_json::json!({"token":"test-token", "expires_in":expires_in}))
            }}))
            .fallback(move |req: Request| async move {
                if req.headers().get("authorization").is_some_and(|h| h == "Bearer test-token") {
                    (status, "authenticated").into_response()
                } else {
                    let host = req.headers().get("host").unwrap().to_str().unwrap();
                    (StatusCode::UNAUTHORIZED, [("www-authenticate", format!("Bearer realm=\"http://{host}/token\",service=\"registry\",scope=\"repository:library/alpine:pull\""))]).into_response()
                }
            })).await;
        (upstream, tokens)
    }

    #[tokio::test]
    async fn authenticated_retry_404_fails_over() {
        let (upstream, _) = authenticated_upstream(StatusCode::NOT_FOUND, 60).await;
        let backup = serve(axum::Router::new().fallback(|| async { "backup" })).await;
        let response = request(
            app(vec![mirror(&upstream), mirror(&backup)]),
            "/v2/library/alpine/manifests/latest",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(bytes(response).await, "backup");
    }

    #[tokio::test]
    async fn authenticated_retry_503_fails_over() {
        let (upstream, _) = authenticated_upstream(StatusCode::SERVICE_UNAVAILABLE, 60).await;
        let backup = serve(axum::Router::new().fallback(|| async { "backup" })).await;
        let response = request(
            app(vec![mirror(&upstream), mirror(&backup)]),
            "/v2/library/alpine/manifests/latest",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(bytes(response).await, "backup");
    }

    #[tokio::test]
    async fn concurrent_token_fetches_are_coalesced_and_cached_per_repository() {
        let (upstream, tokens) = authenticated_upstream(StatusCode::OK, 60).await;
        let app = app(vec![mirror(&upstream)]);
        let responses = futures_util::future::join_all(
            (0..12).map(|_| request(app.clone(), "/v2/library/alpine/manifests/latest")),
        )
        .await;
        for response in responses {
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(
            tokens.load(Ordering::SeqCst),
            1,
            "concurrent misses should fetch one token"
        );
        assert_eq!(
            request(app.clone(), "/v2/library/alpine/blobs/sha256:abc")
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            tokens.load(Ordering::SeqCst),
            1,
            "manifest and blob share repository token"
        );
        assert_eq!(
            request(app, "/v2/other/repo/manifests/latest")
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            tokens.load(Ordering::SeqCst),
            2,
            "another repository gets its own token"
        );
    }

    #[tokio::test]
    async fn token_cache_honors_expiry() {
        let (upstream, tokens) = authenticated_upstream(StatusCode::OK, 1).await;
        let app = app(vec![mirror(&upstream)]);
        for _ in 0..2 {
            assert_eq!(
                request(app.clone(), "/v2/library/alpine/manifests/latest")
                    .await
                    .status(),
                StatusCode::OK
            );
        }
        assert_eq!(tokens.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(
            request(app, "/v2/library/alpine/manifests/latest")
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(tokens.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn client_authorization_is_not_forwarded_to_mirror() {
        let upstream = serve(axum::Router::new().fallback(|req: Request| async move {
            assert!(
                req.headers().get("authorization").is_none(),
                "registry credential leaked"
            );
            "safe"
        }))
        .await;
        let response = app(vec![mirror(&upstream)])
            .oneshot(
                Request::builder()
                    .uri("/v2/")
                    .header("authorization", "Bearer registry-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn server_errors_cool_down_but_missing_resources_do_not() {
        for status in [StatusCode::SERVICE_UNAVAILABLE, StatusCode::NOT_FOUND] {
            let hits = Arc::new(AtomicUsize::new(0));
            let count = hits.clone();
            let primary = serve(axum::Router::new().fallback(move || {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    status
                }
            }))
            .await;
            let backup = serve(axum::Router::new().fallback(|| async { "backup" })).await;
            let app = app(vec![mirror(&primary), mirror(&backup)]);
            for path in ["/v2/a/manifests/one", "/v2/a/manifests/two"] {
                assert_eq!(request(app.clone(), path).await.status(), StatusCode::OK);
            }
            assert_eq!(
                hits.load(Ordering::SeqCst),
                if status.is_server_error() { 1 } else { 2 }
            );
        }
    }

    #[tokio::test]
    async fn active_stream_survives_thirty_second_total_duration() {
        use futures_util::StreamExt;
        let upstream = serve(axum::Router::new().fallback(|| async {
            Body::from_stream(futures_util::stream::unfold(0, |index| async move {
                if index == 64 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                Some((
                    Ok::<_, std::io::Error>(Bytes::from(vec![index; 1024])),
                    index + 1,
                ))
            }))
        }))
        .await;
        let state = AppState {
            client: crate::proxy_client().unwrap(),
            router: Router::new(vec![mirror(&upstream)]),
            tokens: crate::auth::TokenCache::default(),
        };
        let response = request(build_router(state), "/v2/a/blobs/sha256:abc").await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let mut actual = Vec::new();
        while let Some(chunk) = stream.next().await {
            actual.extend_from_slice(&chunk.unwrap());
        }
        let expected: Vec<u8> = (0..64).flat_map(|index| vec![index; 1024]).collect();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn all_cooled_mirrors_remain_available_as_fallback() {
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let upstream = serve(axum::Router::new().fallback(move || {
            let count = count.clone();
            async move {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::OK
                }
            }
        }))
        .await;
        let app = app(vec![mirror(&upstream)]);
        assert_eq!(
            request(app.clone(), "/v2/a/manifests/one").await.status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            request(app, "/v2/a/manifests/two").await.status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn streaming_response_preserves_bytes_and_delivers_before_completion() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let rx = Arc::new(tokio::sync::Mutex::new(Some(rx)));
        let chunk = Bytes::from(vec![0x9d; 1024 * 1024]);
        let expected = chunk.clone();
        let upstream = serve(axum::Router::new().fallback(move || {
            let rx = rx.clone();
            let chunk = chunk.clone();
            async move {
                let receiver = rx.lock().await.take().unwrap();
                let stream =
                    futures_util::stream::once(async move { Ok::<_, std::io::Error>(chunk) })
                        .chain(futures_util::stream::once(async move {
                            receiver.await.unwrap();
                            Ok(Bytes::from_static(b"tail\x00\xff"))
                        }));
                Body::from_stream(stream)
            }
        }))
        .await;
        use futures_util::StreamExt;
        let response = request(app(vec![mirror(&upstream)]), "/v2/a/blobs/sha256:abc").await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let mut received = Vec::new();
        while received.len() < expected.len() {
            let part = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            received.extend_from_slice(&part);
        }
        assert_eq!(&received, expected.as_ref());
        tx.send(()).unwrap();
        while let Some(part) = stream.next().await {
            received.extend_from_slice(&part.unwrap());
        }
        let mut expected = expected.to_vec();
        expected.extend_from_slice(b"tail\x00\xff");
        assert_eq!(received, expected);
    }
}

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
    pub tokens: crate::auth::TokenCache,
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
    let results =
        crate::probe::probe_all(&state.client, &state.tokens, &mirrors, 256 * 1024, 8000).await;
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
        if HOP_BY_HOP.contains(&k.as_str()) || k == "host" || k == "authorization" {
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
        match forward_once(
            &state.client,
            &state.tokens,
            mirror,
            &method,
            &path_and_query,
            &fwd,
            &body,
        )
        .await
        {
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
                if matches!(&e, UpstreamError::Conn(_) | UpstreamError::Http(500..=599)) {
                    state.router.cool_down(mirror).await;
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
    tokens: &crate::auth::TokenCache,
    mirror: &Mirror,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response, UpstreamError> {
    let base = mirror.url.trim_end_matches('/');
    let url = format!("{base}{path_and_query}");

    let key = crate::auth::TokenKey::new(base, path_and_query);
    let cached = tokens.get(&key).await;
    let token = cached.as_deref().or(mirror.token.as_deref());
    let mut resp = send(client, method, &url, headers, body, token).await?;

    if resp.status() == StatusCode::UNAUTHORIZED {
        if let Some(rejected) = cached.as_deref() {
            tokens.invalidate(&key, rejected).await;
        }
        let challenge = resp
            .headers()
            .get_all("www-authenticate")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect::<Vec<_>>()
            .join(", ");
        drop(resp);
        let token = tokens
            .fetch(client, key, &challenge, std::time::Duration::from_secs(8))
            .await
            .map_err(|_| UpstreamError::Http(401))?;
        resp = send(client, method, &url, headers, body, Some(&token)).await?;
    }

    let status = resp.status();
    if matches!(status.as_u16(), 401 | 404) || status.is_server_error() {
        return Err(UpstreamError::Http(status.as_u16()));
    }
    to_axum_response(resp).await
}

async fn send(
    client: &reqwest::Client,
    method: &Method,
    url: &str,
    headers: &HeaderMap,
    body: &Bytes,
    token: Option<&str>,
) -> Result<reqwest::Response, UpstreamError> {
    let mut req = client.request(method.clone(), url);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    if !body.is_empty() {
        req = req.body(body.clone());
    }
    if let Some(token) = token {
        req = req.bearer_auth(token);
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
        if HOP_BY_HOP.contains(&k.as_str()) {
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
