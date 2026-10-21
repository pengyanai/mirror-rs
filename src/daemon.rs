use crate::config::Config;
use crate::router::Router;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Background daemon: periodically probes mirrors and updates the router.
pub struct Daemon {
    pub client: reqwest::Client,
    pub router: Router,
    pub config: Arc<Config>,
    pub tokens: crate::auth::TokenCache,
    /// Shared flag to stop the daemon.
    pub stop: Arc<RwLock<bool>>,
}

impl Daemon {
    pub fn new(client: reqwest::Client, router: Router, config: Config) -> Self {
        Self {
            client,
            router,
            config: Arc::new(config),
            tokens: crate::auth::TokenCache::default(),
            stop: Arc::new(RwLock::new(false)),
        }
    }

    /// Run the probe loop until stopped.
    pub async fn run(self) {
        let period = std::time::Duration::from_secs(self.config.probe_interval_secs.max(1));
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        self.probe_once().await;
        loop {
            interval.tick().await;
            if *self.stop.read().await {
                tracing::info!("daemon stopping");
                return;
            }
            self.probe_once().await;
        }
    }

    /// Perform a single probe cycle and update the router + state file.
    pub async fn probe_once(&self) {
        let mirrors = self.router.mirrors.clone();
        let results = crate::probe::probe_all(
            &self.client,
            &self.tokens,
            &mirrors,
            self.config.probe_bytes,
            self.config.probe_timeout_ms,
        )
        .await;
        let best = results.first().cloned();
        self.router.update_from_probes(results.clone()).await;
        if let Some(b) = &best {
            tracing::info!(
                best = %b.url,
                ttfb_ms = b.ttfb_ms,
                throughput_bps = b.throughput_bps,
                "probe complete, best mirror updated"
            );
        }
        // Persist state.
        if let Some(state_path) = &self.config.state_path {
            let _ = write_state(state_path, &results);
        }
    }
}

/// Write probe results to the state file.
fn write_state(path: &Path, results: &[crate::probe::ProbeResult]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&serde_json::json!({
        "probed_at": chrono_now_iso(),
        "results": results,
    }))
    .unwrap_or_default();
    std::fs::write(path, json + "\n")
}

fn chrono_now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn initial_probe_runs_once_then_uses_configured_cadence() {
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let upstream = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().fallback(move || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"layers":[]}))
                    }
                }),
            )
            .await
            .unwrap();
        });
        let cfg = Config {
            mirrors: vec![crate::config::Mirror { url, token: None }],
            probe_interval_secs: 1,
            ..Config::default()
        };
        let daemon = Daemon::new(
            reqwest::Client::new(),
            Router::new(cfg.mirrors.clone()),
            cfg,
        );
        let task = tokio::spawn(daemon.run());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let observed = hits.load(Ordering::SeqCst);
        task.abort();
        upstream.abort();
        assert_eq!(
            observed, 2,
            "one-second cadence must not round up to five seconds"
        );
    }
}
