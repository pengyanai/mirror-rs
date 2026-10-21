use crate::config::Mirror;
use crate::probe::ProbeResult;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Holds the current ordered list of mirrors (best first) and the raw mirror
/// definitions. Updated by the probe loop; read by the proxy on every request.
#[derive(Clone)]
pub struct Router {
    /// Mirrors in current preference order (index 0 = fastest).
    pub order: Arc<RwLock<Vec<usize>>>,
    /// All mirror definitions (stable indices).
    pub mirrors: Arc<Vec<Mirror>>,
    /// Latest probe results, keyed by mirror index.
    pub results: Arc<RwLock<Vec<ProbeResult>>>,
    cooldowns: moka::future::Cache<String, ()>,
}

impl Router {
    pub fn new(mirrors: Vec<Mirror>) -> Self {
        let n = mirrors.len();
        let order: Vec<usize> = (0..n).collect();
        Self {
            order: Arc::new(RwLock::new(order)),
            mirrors: Arc::new(mirrors),
            results: Arc::new(RwLock::new(Vec::new())),
            cooldowns: moka::future::Cache::builder()
                .max_capacity(n.max(1) as u64)
                .time_to_live(std::time::Duration::from_secs(5))
                .build(),
        }
    }

    /// Return mirrors in current preference order (best first).
    pub async fn ordered_mirrors(&self) -> Vec<Mirror> {
        let order = self.order.read().await.clone();
        let mut ready = Vec::new();
        let mut cooled = Vec::new();
        for i in order {
            let mirror = self.mirrors[i].clone();
            if self.cooldowns.get(&mirror.url).await.is_some() {
                cooled.push(mirror);
            } else {
                ready.push(mirror);
            }
        }
        // Keep cooled mirrors as a last resort when every upstream is failing.
        ready.extend(cooled);
        ready
    }

    pub async fn cool_down(&self, mirror: &Mirror) {
        self.cooldowns.insert(mirror.url.clone(), ()).await;
    }

    /// Update the ordering from fresh probe results.
    pub async fn update_from_probes(&self, results: Vec<ProbeResult>) {
        // Map url -> index.
        let url_to_idx: std::collections::HashMap<&str, usize> = self
            .mirrors
            .iter()
            .enumerate()
            .map(|(i, m)| (m.url.as_str(), i))
            .collect();
        let mut new_results = vec![
            ProbeResult {
                url: String::new(),
                ttfb_ms: 0,
                throughput_bps: 0,
                bytes_downloaded: 0,
                ok: false,
                error: Some("not probed".into()),
                probed_at_ms: 0,
            };
            self.mirrors.len()
        ];
        let mut order: Vec<usize> = Vec::new();
        for r in &results {
            if let Some(&idx) = url_to_idx.get(r.url.as_str()) {
                new_results[idx] = r.clone();
                if r.ok {
                    order.push(idx);
                }
            }
        }
        // Append any mirrors that failed (so we still try them last).
        for i in 0..self.mirrors.len() {
            if !order.contains(&i) {
                order.push(i);
            }
        }
        *self.results.write().await = new_results;
        *self.order.write().await = order;
    }

    /// Get the current best mirror index (0 if none).
    #[allow(dead_code)]
    pub async fn best_index(&self) -> usize {
        self.order.read().await.first().copied().unwrap_or(0)
    }
}
