# mirror-rs

Seamless, transparent Docker registry mirror proxy with **speed-based load
balancing** — written in Rust.

## Why a proxy (and not just `registry-mirrors`)?

Docker's `registry-mirrors` is a **fallback list**, not a load balancer: the
daemon tries them in order and only moves to the next one after the previous
fails. It never picks the fastest. So a "seamless auto-load" tool can't just
rewrite the config — it has to hijack at the **registry-protocol layer**.

`mirror-rs` does exactly that:

```
docker pull nginx
   │  (daemon's registry-mirrors = http://host.docker.internal:7788)
   ▼
┌─────────────────────────────────────────────┐
│  mirror-rs  (local proxy, 0.0.0.0:7788)     │
│                                             │
│  1. per-request: route to the FASTEST mirror│
│  2. on error:  fail over to the next        │
│  3. 401:       do the registry token dance  │
│  4. every 5min: re-probe all mirrors        │
│     (TTFB + real blob throughput)           │
│     and re-rank — NO docker restart needed  │
└─────────────────────────────────────────────┘
   │            │              │
   ▼            ▼              ▼
 daocloud    1ms.run     xuanyuan.me   (upstream mirrors)
```

Because the routing decision happens **per request inside the proxy**, the
daemon never needs to restart when speeds change — that's what makes it
"无感" (seamless). The only restart is the one-time `setup`.

## Features

- **Speed-based routing** — probes each mirror's TTFB *and* real blob
  throughput (downloads a 256KB slice of an actual layer, not just a ping).
- **Transparent failover** — if the best mirror dies mid-pull, the next one
  takes over automatically.
- **Token auth** — handles the Docker registry `WWW-Authenticate` Bearer dance
  (401 → fetch token → retry) for both manifests and blobs.
- **OCI index resolution** — correctly resolves multi-arch image indexes to a
  platform manifest (linux/amd64|arm64) so probes measure real layers.
- **Auto-start** — installs a macOS launchd agent (`KeepAlive`), so the proxy
  survives reboots and crashes.
- **Safe config management** — backs up `docker.json` before editing, preserves
  other keys (e.g. `ipv6`), and `teardown` restores cleanly.

## Install

### Homebrew (macOS / Linux)

```bash
brew install pengyanai/tap/mirror-rs
```

### pip (any platform with a prebuilt binary)

```bash
pip install git+https://github.com/pengyanai/mirror-rs
```

The wheel bundles the prebuilt binary for your platform (macOS arm64/x86_64,
Linux x86_64/aarch64) — no Rust toolchain needed.

### From source

```bash
cargo build --release
# binary: target/release/mirror-rs
```

## Setup (one-time)

```bash
# 1. (optional) edit config: mirrors, port, probe interval
~/.config/mirror-rs/config.json        # created on first run, or:
mirror-rs init                          # print default config

# 2. install: writes docker config + launchd agent + restarts docker
mirror-rs setup
```

`setup` does:
1. Backs up `~/.orbstack/config/docker.json` (OrbStack) or
   `~/.docker/daemon.json`.
2. Sets `registry-mirrors` to `http://host.docker.internal:7788` (the host
   address as seen from the Docker daemon inside the OrbStack VM — `127.0.0.1`
   would be the VM's own loopback and won't work).
3. Installs `~/Library/LaunchAgents/com.mirror-rs.plist` (auto-start + KeepAlive).
4. Restarts Docker (`orb restart -a` on OrbStack, `systemctl restart docker`
   elsewhere).

## Usage

```bash
mirror-rs probe      # one-shot speed test, ranked table
mirror-rs status     # current mirror ranking (live)
mirror-rs run        # run the proxy in the foreground (debugging)
mirror-rs teardown   # remove: restore docker config + uninstall launchd
```

The proxy also exposes:
- `GET /healthz` — liveness
- `GET /__mirror__/status` — current ranking (JSON)
- `GET /__mirror__/probe` — trigger an immediate re-probe

## Config

`~/.config/mirror-rs/config.json`:

```json
{
  "port": 7788,
  "bind": "0.0.0.0",
  "host_ip": "host.docker.internal",
  "probe_interval_secs": 300,
  "probe_bytes": 262144,
  "probe_timeout_ms": 8000,
  "mirrors": [
    { "url": "https://docker.m.daocloud.io" },
    { "url": "https://docker.1ms.run" },
    { "url": "https://docker.xuanyuan.me" }
  ]
}
```

- `bind` — must be `0.0.0.0` so the OrbStack VM can reach the host proxy.
- `host_ip` — what gets written into `registry-mirrors`. On OrbStack use
  `host.docker.internal`. On a Linux host running Docker natively, use
  `127.0.0.1`.
- `mirrors` — your candidate list. Order is the *initial* preference; the
  probe re-ranks by measured speed.

## Scoring

`score = ttfb_ms + throughput_penalty`, where
`throughput_penalty = 100 / (MB/s)` (capped at 5000 if < 0.05 MB/s). Lower is
better. This weights latency heavily while still rewarding real download
speed, so a low-latency-but-slow mirror ranks below a balanced one.

## Verified (2026-10-20, OrbStack 2.2.4 / Docker 29.8.2 / macOS arm64)

- `docker pull hello-world` — every request (ping, manifest, blobs) served by
  the proxy, all routed to the fastest mirror (`docker.m.daocloud.io`).
- `docker pull nginx` (~140MB) — 16.4s ≈ 8.5 MB/s sustained through the proxy.
- Failover — with the #1 mirror dead, requests transparently fall back.
- Containers with `unless-stopped` restart policy auto-recover from the
  one-time `orb restart` (3.7s).

## Notes / limitations

- The one-time `setup` restarts Docker (brief interruption). After that, speed
  changes are handled live with **no** restart.
- Mirrors that return 404 for a manifest (e.g. `dockerproxy.net`) are treated
  as dead and excluded.
- The proxy binds `0.0.0.0` on the host — it's a local registry proxy, not a
  public service. Don't expose the port to the internet.
