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
    /// Shared flag to stop the daemon.
    pub stop: Arc<RwLock<bool>>,
}

impl Daemon {
    pub fn new(client: reqwest::Client, router: Router, config: Config) -> Self {
        Self {
            client,
            router,
            config: Arc::new(config),
            stop: Arc::new(RwLock::new(false)),
        }
    }

    /// Run the probe loop until stopped.
    pub async fn run(self) {
        let interval = std::time::Duration::from_secs(self.config.probe_interval_secs);
        loop {
            self.probe_once().await;
            // Sleep in small increments so we can react to stop quickly.
            let mut elapsed = std::time::Duration::from_secs(0);
            while elapsed < interval {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                elapsed += std::time::Duration::from_secs(5);
                if *self.stop.read().await {
                    tracing::info!("daemon stopping");
                    return;
                }
            }
        }
    }

    /// Perform a single probe cycle and update the router + state file.
    pub async fn probe_once(&self) {
        let mirrors = self.router.mirrors.clone();
        let results = crate::probe::probe_all(
            &self.client,
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
    // Avoid a chrono dependency; use a simple UTC timestamp via std.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}
