use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A single upstream registry mirror.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mirror {
    /// Base URL, e.g. "https://docker.1ms.run"
    pub url: String,
    /// Optional static bearer token (rarely needed; most mirrors are public).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// Top-level configuration for mirror-rs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Port the local proxy listens on.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Address the proxy binds to. Must be 0.0.0.0 (or the host's VM-facing IP)
    /// so the Docker daemon inside the OrbStack VM can reach it.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// The host address as seen from the Docker daemon (inside the OrbStack VM).
    /// This is what gets written into registry-mirrors. On OrbStack, the daemon
    /// reaches the host via `host.docker.internal` (127.0.0.1 is the VM's own
    /// loopback and won't work).
    #[serde(default = "default_host_ip")]
    pub host_ip: String,
    /// Upstream mirrors, in initial preference order.
    #[serde(default)]
    pub mirrors: Vec<Mirror>,
    /// How often (seconds) to re-probe mirror speeds.
    #[serde(default = "default_probe_interval")]
    pub probe_interval_secs: u64,
    /// Path to the Docker daemon config file to manage (registry-mirrors).
    #[serde(default)]
    pub docker_config_path: Option<PathBuf>,
    /// Path to the launchd plist (macOS) for auto-start.
    #[serde(default)]
    pub launchd_plist_path: Option<PathBuf>,
    /// Path to the state file (probe results, chosen mirror).
    #[serde(default)]
    pub state_path: Option<PathBuf>,
    /// Path to the log file.
    #[serde(default)]
    pub log_path: Option<PathBuf>,
    /// Number of bytes to download during speed probe (default 256KB).
    #[serde(default = "default_probe_bytes")]
    pub probe_bytes: u64,
    /// Timeout (ms) for a single probe request.
    #[serde(default = "default_probe_timeout_ms")]
    pub probe_timeout_ms: u64,
}

fn default_port() -> u16 {
    7788
}
fn default_bind() -> String {
    "0.0.0.0".to_string()
}
fn default_host_ip() -> String {
    "host.docker.internal".to_string()
}
fn default_probe_interval() -> u64 {
    300
}
fn default_probe_bytes() -> u64 {
    256 * 1024
}
fn default_probe_timeout_ms() -> u64 {
    8000
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: default_port(),
            bind: default_bind(),
            host_ip: default_host_ip(),
            mirrors: default_mirrors(),
            probe_interval_secs: default_probe_interval(),
            docker_config_path: None,
            launchd_plist_path: None,
            state_path: None,
            log_path: None,
            probe_bytes: default_probe_bytes(),
            probe_timeout_ms: default_probe_timeout_ms(),
        }
    }
}

/// Built-in default mirror list (public, currently reachable).
pub fn default_mirrors() -> Vec<Mirror> {
    [
        "https://docker.1ms.run",
        "https://docker.m.daocloud.io",
        "https://docker.xuanyuan.me",
        "https://dockerproxy.net",
        "https://hub.rat.dev",
    ]
    .iter()
    .map(|u| Mirror {
        url: u.to_string(),
        token: None,
    })
    .collect()
}

impl Config {
    /// Load config from a JSON file, falling back to defaults for missing fields.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            tracing::warn!(path = %path.display(), "config file not found, using defaults");
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        let cfg: Config = serde_json::from_str(&raw)?;
        Ok(cfg)
    }

    /// Write the config to disk (pretty JSON).
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json + "\n")?;
        Ok(())
    }

    /// Resolve the docker config path, defaulting to ~/.orbstack/config/docker.json
    /// (OrbStack) or ~/.docker/daemon.json.
    pub fn resolved_docker_config_path(&self) -> PathBuf {
        if let Some(p) = &self.docker_config_path {
            return p.clone();
        }
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let orb = home.join(".orbstack/config/docker.json");
        if orb.exists() {
            return orb;
        }
        home.join(".docker/daemon.json")
    }

    /// Resolve the mirror-rs config path (default ~/.config/mirror-rs/config.json).
    pub fn resolved_config_path(&self) -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".config/mirror-rs/config.json")
    }
}
