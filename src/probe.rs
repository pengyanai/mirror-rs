use crate::config::Mirror;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Result of probing a single mirror.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProbeResult {
    pub url: String,
    /// Time to first byte of the manifest request (ms).
    pub ttfb_ms: u64,
    /// Measured download throughput (bytes/sec) over the sampled blob window.
    pub throughput_bps: u64,
    /// Total bytes actually downloaded during the blob sample.
    pub bytes_downloaded: u64,
    /// Whether the mirror responded successfully.
    pub ok: bool,
    /// Human-readable error if the probe failed.
    pub error: Option<String>,
    /// Monotonic timestamp (ms since process start) of the probe.
    pub probed_at_ms: u128,
}

impl ProbeResult {
    /// A composite score: lower is better. We weight TTFB heavily and add a
    /// throughput penalty so a fast-but-slow mirror ranks below a balanced one.
    pub fn score(&self) -> f64 {
        if !self.ok {
            return f64::INFINITY;
        }
        let mbps = self.throughput_bps as f64 / (1024.0 * 1024.0);
        let throughput_penalty = if mbps < 0.05 {
            5000.0 // effectively dead
        } else {
            (1.0 / mbps) * 100.0 // higher for slower mirrors
        };
        self.ttfb_ms as f64 + throughput_penalty
    }
}

/// Probe a single mirror:
///  1. GET the alpine manifest (handles the 401 token dance) -> TTFB + a layer digest.
///  2. GET a Range slice of that blob -> real download throughput.
pub async fn probe_mirror(
    client: &reqwest::Client,
    mirror: &Mirror,
    probe_bytes: u64,
    timeout_ms: u64,
) -> ProbeResult {
    let base = mirror.url.trim_end_matches('/');
    let manifest_url = format!("{base}/v2/library/alpine/manifests/latest");
    let timeout = Duration::from_millis(timeout_ms);
    let start = Instant::now();

    // --- Step 1: manifest (with token dance) ---
    let manifest = match fetch_manifest(client, mirror, &manifest_url, timeout).await {
        Ok(m) => m,
        Err(e) => {
            return ProbeResult {
                url: base.to_string(),
                ttfb_ms: start.elapsed().as_millis() as u64,
                throughput_bps: 0,
                bytes_downloaded: 0,
                ok: false,
                error: Some(e),
                probed_at_ms: now_ms(),
            };
        }
    };
    let ttfb_ms = start.elapsed().as_millis() as u64;

    // Pick the largest layer digest for a meaningful throughput sample.
    let digest = manifest
        .get("layers")
        .and_then(|l| l.as_array())
        .and_then(|arr| {
            arr.iter()
                .max_by_key(|x| x.get("size").and_then(|s| s.as_u64()).unwrap_or(0))
        })
        .and_then(|l| l.get("digest"))
        .and_then(|d| d.as_str())
        .map(|s| s.to_string());

    let (throughput_bps, bytes_downloaded) = match digest {
        Some(d) => {
            let blob_url = format!("{base}/v2/library/alpine/blobs/{d}");
            match fetch_blob_sample(client, mirror, &blob_url, probe_bytes, timeout).await {
                Ok((bps, n)) => (bps, n),
                Err(e) => {
                    // Manifest worked but blob failed — still "ok" for liveness,
                    // but report the error and zero throughput.
                    return ProbeResult {
                        url: base.to_string(),
                        ttfb_ms,
                        throughput_bps: 0,
                        bytes_downloaded: 0,
                        ok: true,
                        error: Some(format!("blob sample failed: {e}")),
                        probed_at_ms: now_ms(),
                    };
                }
            }
        }
        None => (0, 0),
    };

    ProbeResult {
        url: base.to_string(),
        ttfb_ms,
        throughput_bps,
        bytes_downloaded,
        ok: true,
        error: None,
        probed_at_ms: now_ms(),
    }
}

/// GET a manifest, performing the registry token dance on 401. If the result is
/// an OCI image index (no `layers`), resolve to a platform manifest (linux/amd64
/// or arm64) so we can extract a layer digest.
async fn fetch_manifest(
    client: &reqwest::Client,
    mirror: &Mirror,
    url: &str,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let mut manifest = get_json(client, mirror, url, timeout).await?;

    // If it's an index (has "manifests", no "layers"), resolve a platform child.
    if manifest.get("layers").is_none()
        && manifest
            .get("manifests")
            .and_then(|m| m.as_array())
            .is_some()
    {
        let base = url
            .rsplit_once("/manifests/")
            .map(|(b, _)| b)
            .unwrap_or(url);
        let child_digest = pick_platform_manifest(&manifest);
        if let Some(digest) = child_digest {
            let child_url = format!("{base}/manifests/{digest}");
            manifest = get_json(client, mirror, &child_url, timeout).await?;
        }
    }
    Ok(manifest)
}

/// Pick the best platform manifest digest from an OCI index (prefer linux amd64/arm64).
fn pick_platform_manifest(index: &serde_json::Value) -> Option<String> {
    let manifests = index.get("manifests")?.as_array()?;
    let mut best: Option<&serde_json::Value> = None;
    let mut best_score = i32::MIN;
    for m in manifests {
        let p = m.get("platform")?;
        let os = p.get("os").and_then(|o| o.as_str()).unwrap_or("");
        let arch = p.get("architecture").and_then(|a| a.as_str()).unwrap_or("");
        if os != "linux" {
            continue;
        }
        let score = match arch {
            "amd64" | "arm64" => 2,
            "arm" => 1,
            _ => 0,
        };
        if score > best_score {
            best_score = score;
            best = Some(m);
        }
    }
    best.and_then(|m| {
        m.get("digest")
            .and_then(|d| d.as_str())
            .map(|s| s.to_string())
    })
}

/// GET a JSON resource with the token dance.
async fn get_json(
    client: &reqwest::Client,
    mirror: &Mirror,
    url: &str,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let mut req = client.get(url).timeout(timeout);
    if let Some(tok) = &mirror.token {
        req = req.header("Authorization", format!("Bearer {tok}"));
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.as_u16() == 401 {
        let wa = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or("401 without WWW-Authenticate")?;
        let token = fetch_token(client, &wa).await.ok_or("token fetch failed")?;
        let resp2 = client
            .get(url)
            .timeout(timeout)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp2.status().is_success() {
            return Err(format!("retry HTTP {}", resp2.status()));
        }
        return resp2.json().await.map_err(|e| e.to_string());
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    resp.json().await.map_err(|e| e.to_string())
}

/// GET a Range slice of a blob and measure throughput.
async fn fetch_blob_sample(
    client: &reqwest::Client,
    mirror: &Mirror,
    url: &str,
    probe_bytes: u64,
    timeout: Duration,
) -> Result<(u64, u64), String> {
    let mut req = client.get(url).timeout(timeout).header(
        "Range",
        format!("bytes=0-{}", probe_bytes.saturating_sub(1)),
    );
    if let Some(tok) = &mirror.token {
        req = req.header("Authorization", format!("Bearer {tok}"));
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    // 206 = partial content (range honored), 200 = full body (range ignored).
    if status.as_u16() == 401 {
        let wa = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or("401 without WWW-Authenticate")?;
        let token = fetch_token(client, &wa).await.ok_or("token fetch failed")?;
        let resp2 = client
            .get(url)
            .timeout(timeout)
            .header(
                "Range",
                format!("bytes=0-{}", probe_bytes.saturating_sub(1)),
            )
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp2.status().is_success() {
            return Err(format!("blob retry HTTP {}", resp2.status()));
        }
        return stream_throughput(resp2, probe_bytes).await;
    }
    if !(status.is_success() || status.as_u16() == 206) {
        return Err(format!("blob HTTP {status}"));
    }
    stream_throughput(resp, probe_bytes).await
}

/// Stream up to `probe_bytes` and return (throughput_bps, bytes_downloaded).
async fn stream_throughput(
    resp: reqwest::Response,
    probe_bytes: u64,
) -> Result<(u64, u64), String> {
    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut bytes_downloaded: u64 = 0;
    let dl_start = Instant::now();
    while bytes_downloaded < probe_bytes {
        match stream.next().await {
            Some(Ok(chunk)) => bytes_downloaded += chunk.len() as u64,
            Some(Err(e)) => return Err(e.to_string()),
            None => break,
        }
    }
    let dl_secs = dl_start.elapsed().as_secs_f64().max(0.001);
    let throughput_bps = (bytes_downloaded as f64 / dl_secs) as u64;
    Ok((throughput_bps, bytes_downloaded))
}

/// Probe all mirrors concurrently and return results sorted by score (best first).
pub async fn probe_all(
    client: &reqwest::Client,
    mirrors: &[Mirror],
    probe_bytes: u64,
    timeout_ms: u64,
) -> Vec<ProbeResult> {
    let futs = mirrors
        .iter()
        .map(|m| async move { probe_mirror(client, m, probe_bytes, timeout_ms).await });
    let mut results = futures_util::future::join_all(futs).await;
    results.sort_by(|a, b| {
        a.score()
            .partial_cmp(&b.score())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

/// Parse a WWW-Authenticate Bearer challenge into key/value params.
fn parse_www_authenticate(wa: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let rest = wa
        .trim()
        .strip_prefix("Bearer")
        .map(|s| s.trim())
        .unwrap_or(wa);
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(eq) = part.find('=') {
            let k = part[..eq].trim().to_string();
            let v = part[eq + 1..].trim().trim_matches('"').to_string();
            map.insert(k, v);
        }
    }
    map
}

/// Fetch a bearer token from the auth realm described in the challenge.
pub async fn fetch_token(client: &reqwest::Client, www_authenticate: &str) -> Option<String> {
    let params = parse_www_authenticate(www_authenticate);
    let realm = params.get("realm")?.clone();
    let mut url = realm;
    let mut query = Vec::new();
    if let Some(s) = params.get("service") {
        query.push(format!("service={}", urlencoding::encode(s)));
    }
    if let Some(s) = params.get("scope") {
        query.push(format!("scope={}", urlencoding::encode(s)));
    }
    if !query.is_empty() {
        url.push('?');
        url.push_str(&query.join("&"));
    }
    let resp = client.get(&url).send().await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    json.get("token")
        .or_else(|| json.get("access_token"))?
        .as_str()
        .map(|s| s.to_string())
}

fn now_ms() -> u128 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_millis()
}
