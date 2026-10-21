mod config;
mod daemon;
mod probe;
mod proxy;
mod router;

use clap::{Parser, Subcommand};
use config::Config;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "mirror-rs",
    version,
    about = "Seamless Docker registry mirror proxy with speed-based load balancing"
)]
struct Cli {
    /// Path to config file.
    #[arg(
        short,
        long,
        global = true,
        default_value = "~/.config/mirror-rs/config.json"
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy server (foreground).
    Run,
    /// Probe all mirrors once and print a ranked table.
    Probe,
    /// Show current mirror ranking / status.
    Status,
    /// Install: write docker config, install launchd service, restart docker.
    Setup {
        /// Skip the docker restart (you restart manually).
        #[arg(long)]
        no_restart: bool,
    },
    /// Uninstall: restore docker config, remove launchd service.
    Teardown {
        #[arg(long)]
        no_restart: bool,
    },
    /// Print the default config to stdout.
    Init,
}

fn expand_home(p: &PathBuf) -> PathBuf {
    if let Some(s) = p.to_str().and_then(|s| s.strip_prefix('~')) {
        if let Some(home) = dirs::home_dir() {
            // s may start with '/' (e.g. "/.config/...") — join would replace
            // the whole path, so strip the leading slash first.
            let rel = s.trim_start_matches('/');
            return home.join(rel);
        }
    }
    p.clone()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Init tracing (stderr; launchd redirects to log file).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mirror_rs=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let config_path = expand_home(&cli.config);
    let cfg = Config::load(&config_path)?;

    match cli.command {
        Command::Init => {
            let json = serde_json::to_string_pretty(&Config::default())?;
            println!("{json}");
            Ok(())
        }
        Command::Probe => {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(cfg.probe_timeout_ms))
                .build()?;
            let results =
                probe::probe_all(&client, &cfg.mirrors, cfg.probe_bytes, cfg.probe_timeout_ms)
                    .await;
            print_table(&results);
            Ok(())
        }
        Command::Status => {
            let base = format!("http://127.0.0.1:{}", cfg.port);
            let client = reqwest::Client::new();
            match client.get(format!("{base}/__mirror__/status")).send().await {
                Ok(r) => {
                    let text = r.text().await.unwrap_or_default();
                    println!("{text}");
                }
                Err(e) => {
                    eprintln!("proxy not reachable at {base}: {e}");
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        Command::Setup { no_restart } => {
            setup(&cfg, !no_restart).await?;
            Ok(())
        }
        Command::Teardown { no_restart } => {
            teardown(&cfg, !no_restart).await?;
            Ok(())
        }
        Command::Run => {
            run_server(&cfg).await?;
            Ok(())
        }
    }
}

fn print_table(results: &[probe::ProbeResult]) {
    println!(
        "{:<28} {:>6} {:>12} {:>10} {:>10}",
        "MIRROR", "TTFB(ms)", "THROUGHPUT", "BYTES", "SCORE"
    );
    for r in results {
        let mbps = r.throughput_bps as f64 / (1024.0 * 1024.0);
        let score = if r.ok {
            format!("{:.1}", r.score())
        } else {
            "FAIL".into()
        };
        println!(
            "{:<28} {:>6} {:>9.2}MB/s {:>10} {:>10}",
            truncate(&r.url, 28),
            r.ttfb_ms,
            mbps,
            r.bytes_downloaded,
            score
        );
        if let Some(e) = &r.error {
            println!("    └─ {e}");
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n.saturating_sub(1)])
    }
}

async fn run_server(cfg: &Config) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(
            cfg.probe_timeout_ms.max(30_000),
        ))
        .pool_max_idle_per_host(32)
        .build()?;

    let router = router::Router::new(cfg.mirrors.clone());
    let state = proxy::AppState {
        client: client.clone(),
        router: router.clone(),
    };

    // Start the probe daemon.
    let daemon = daemon::Daemon::new(client, router.clone(), cfg.clone());
    let stop = daemon.stop.clone();
    tokio::spawn(async move { daemon.run().await });

    // Do an initial probe before serving so the first request is already routed well.
    {
        let d = daemon::Daemon::new(reqwest::Client::new(), router.clone(), cfg.clone());
        d.probe_once().await;
    }

    let app = proxy::build_router(state);
    use std::str::FromStr;
    let addr = std::net::SocketAddr::from_str(&format!("{}:{}", cfg.bind, cfg.port))
        .unwrap_or_else(|_| std::net::SocketAddr::from(([0, 0, 0, 0], cfg.port)));
    tracing::info!(%addr, "mirror-rs proxy listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let server = axum::serve(listener, app);

    // Graceful shutdown on SIGINT/SIGTERM.
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        #[cfg(unix)]
        {
            let mut sig =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            tokio::select! {
                _ = sig.recv() => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        r = server => { r?; }
        _ = shutdown => {
            tracing::info!("shutting down");
            *stop.write().await = true;
        }
    }
    Ok(())
}

// ---------- setup / teardown ----------

async fn setup(cfg: &Config, do_restart: bool) -> anyhow::Result<()> {
    let docker_cfg = cfg.resolved_docker_config_path();
    tracing::info!(path = %docker_cfg.display(), "configuring docker registry-mirrors");

    // 1. Back up + write docker config with our proxy as the (only) mirror.
    //    The URL must be the host IP as seen from the Docker daemon (inside the
    //    OrbStack VM), not 127.0.0.1 (which is the VM's own loopback).
    let proxy_url = format!("http://{}:{}", cfg.host_ip, cfg.port);
    write_docker_config(&docker_cfg, &[proxy_url.clone()])?;

    // 2. Install launchd service (macOS) so the proxy auto-starts.
    install_launchd(cfg, &proxy_url)?;

    // 3. Start the proxy now (if not already running) so the restart has it up.
    ensure_proxy_running(cfg)?;

    // 4. Restart docker to pick up the new registry-mirrors.
    if do_restart {
        restart_docker()?;
    } else {
        println!("(skipped docker restart; run `orb restart` or `sudo systemctl restart docker` when ready)");
    }

    println!("✓ mirror-rs installed");
    println!("  proxy (host):  http://{}:{}", cfg.host_ip, cfg.port);
    println!("  docker cfg:    {}", docker_cfg.display());
    println!("  verify:        mirror-rs status   |   docker pull alpine");
    Ok(())
}

async fn teardown(cfg: &Config, do_restart: bool) -> anyhow::Result<()> {
    remove_launchd(cfg)?;
    // Restore docker config: remove our proxy mirror (keep any others).
    let docker_cfg = cfg.resolved_docker_config_path();
    let proxy_url = format!("http://{}:{}", cfg.host_ip, cfg.port);
    remove_docker_mirror(&docker_cfg, &proxy_url)?;
    if do_restart {
        restart_docker()?;
    }
    println!("✓ mirror-rs removed");
    Ok(())
}

/// Write registry-mirrors into the docker config, preserving other keys.
fn write_docker_config(path: &PathBuf, mirrors: &[String]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut val: serde_json::Value = if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        serde_json::from_str(&raw).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    // Backup.
    if path.exists() {
        let bak = path.with_extension("json.mirror-rs.bak");
        std::fs::copy(path, &bak)?;
        tracing::info!(backup = %bak.display(), "backed up docker config");
    }
    val["registry-mirrors"] = serde_json::json!(mirrors);
    let json = serde_json::to_string_pretty(&val)?;
    std::fs::write(path, json + "\n")?;
    Ok(())
}

/// Remove our proxy from registry-mirrors, keeping the rest.
fn remove_docker_mirror(path: &PathBuf, proxy_url: &str) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(path)?;
    let mut val: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));
    if let Some(arr) = val
        .get_mut("registry-mirrors")
        .and_then(|v| v.as_array_mut())
    {
        arr.retain(|m| m.as_str() != Some(proxy_url));
        if arr.is_empty() {
            val.as_object_mut().unwrap().remove("registry-mirrors");
        }
    }
    let json = serde_json::to_string_pretty(&val)?;
    std::fs::write(path, json + "\n")?;
    Ok(())
}

/// Install a launchd LaunchAgent so the proxy starts at login and on boot.
fn install_launchd(cfg: &Config, proxy_url: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let bin = std::env::current_exe()?;
        let config_path = cfg.resolved_config_path();
        let plist_path = cfg.launchd_plist_path.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap()
                .join("Library/LaunchAgents/com.mirror-rs.plist")
        });
        let log_path = cfg.log_path.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap()
                .join(".config/mirror-rs/mirror-rs.log")
        });
        if let Some(parent) = plist_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.mirror-rs</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>--config</string>
        <string>{config_path}</string>
        <string>run</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>{log_path}</string>
    <key>StandardErrorPath</key><string>{log_path}</string>
    <key>EnvironmentVariables</key>
    <dict><key>RUST_LOG</key><string>mirror_rs=info</string></dict>
</dict>
</plist>
"#,
            bin = bin.display(),
            config_path = config_path.display(),
            log_path = log_path.display(),
        );
        std::fs::write(&plist_path, plist)?;
        // Load it (ignore "already loaded" errors).
        let _ = std::process::Command::new("launchctl")
            .args(["unload", plist_path.to_str().unwrap()])
            .output();
        let out = std::process::Command::new("launchctl")
            .args(["load", plist_path.to_str().unwrap()])
            .output()?;
        if !out.status.success() {
            tracing::warn!(
                stderr = String::from_utf8_lossy(&out.stderr).to_string(),
                "launchctl load"
            );
        }
        tracing::info!(plist = %plist_path.display(), "installed launchd agent");
        let _ = proxy_url;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (cfg, proxy_url);
        tracing::warn!("launchd is macOS-only; on Linux use a systemd unit instead");
        Ok(())
    }
}

fn remove_launchd(cfg: &Config) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let plist_path = cfg.launchd_plist_path.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap()
                .join("Library/LaunchAgents/com.mirror-rs.plist")
        });
        if plist_path.exists() {
            let _ = std::process::Command::new("launchctl")
                .args(["unload", plist_path.to_str().unwrap()])
                .output();
            std::fs::remove_file(&plist_path)?;
            tracing::info!(plist = %plist_path.display(), "removed launchd agent");
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cfg;
        Ok(())
    }
}

/// Start the proxy in the background if it isn't already listening.
fn ensure_proxy_running(cfg: &Config) -> anyhow::Result<()> {
    let base = format!("http://127.0.0.1:{}/healthz", cfg.port);
    let client = reqwest::blocking::Client::new();
    if client
        .get(&base)
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
    {
        tracing::info!("proxy already running");
        return Ok(());
    }
    // launchd should have started it; give it a moment.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let ok = client
        .get(&base)
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if !ok {
        tracing::warn!("proxy not responding yet; launchd will (re)start it. You may need to run `mirror-rs run` manually.");
    }
    Ok(())
}

fn restart_docker() -> anyhow::Result<()> {
    // Prefer OrbStack on macOS.
    let orb = which("orb");
    if orb.is_some() {
        tracing::info!("restarting docker via `orb restart -a`");
        let out = std::process::Command::new("orb")
            .args(["restart", "-a"])
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "orb restart -a failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    } else {
        tracing::info!("restarting docker via systemctl");
        let out = std::process::Command::new("sudo")
            .args(["systemctl", "restart", "docker"])
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "systemctl restart docker failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    // Wait for docker to come back.
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if which("docker").map(|d| {
            std::process::Command::new(&d)
                .arg("info")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }) == Some(true)
        {
            tracing::info!("docker is back up");
            return Ok(());
        }
    }
    anyhow::bail!("docker did not come back up within 30s")
}

fn which(bin: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("which").arg(bin).output().ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(PathBuf::from(s));
        }
    }
    None
}
