# mirror-rs

用 Rust 编写的 Docker 镜像加速代理。定期测试镜像站的响应速度和下载速度，优先使用得分最好的镜像站，失败时尝试其他站点。

配置完成后，照常使用 `docker pull`，无需修改镜像名称。镜像站排名变化时不需要重启 Docker。

## 安装

支持 macOS 和 Linux，提供 arm64 / x86_64 预编译二进制，无需安装 Rust。

### Homebrew

```bash
brew install pengyanai/tap/mirror-rs
```

### pip

从 GitHub 安装指定版本，构建时会下载对应平台的 Release 二进制：

```bash
pip install "git+https://github.com/pengyanai/mirror-rs.git@v0.1.1"
```

尚未发布到 PyPI，暂不支持直接运行 `pip install mirror-rs`。需要 Git，并能访问 GitHub Release。建议在 virtualenv 中安装，也可以使用 `pipx`。

### 从源码构建

```bash
git clone https://github.com/pengyanai/mirror-rs.git
cd mirror-rs
cargo build --locked --release
./target/release/mirror-rs --version
```

## 开始使用

macOS + OrbStack 可以使用自动配置。先生成配置文件：

```bash
mkdir -p ~/.config/mirror-rs
mirror-rs init > ~/.config/mirror-rs/config.json
```

然后执行：

```bash
mirror-rs setup
mirror-rs status
docker pull alpine
```

`setup` 会备份并修改 Docker 配置、安装 macOS launchd 服务，并重启 Docker。容器会有短暂中断。若要稍后自行重启：

```bash
mirror-rs setup --no-restart
```

Linux 支持运行代理，需要自行配置 Docker 和 systemd；当前 `setup` 的自动服务管理仅支持 macOS。Linux 原生 Docker 通常在 `/etc/docker/daemon.json` 配置：

```json
{
  "registry-mirrors": ["http://127.0.0.1:7788"]
}
```

合并到已有配置后重启 Docker，并自行保持 `mirror-rs run` 常驻。Docker Desktop 等其他环境需要确认 Docker daemon 能访问宿主机地址。

## 常用命令

| 命令 | 用途 |
| --- | --- |
| `mirror-rs probe` | 测试所有镜像站并输出排名 |
| `mirror-rs status` | 查看运行中代理的排名 |
| `mirror-rs run` | 在前台运行代理 |
| `mirror-rs init` | 输出默认配置 |
| `mirror-rs teardown` | 移除 macOS 服务及代理配置，并重启 Docker |

卸载前先运行 `mirror-rs teardown`，再用对应包管理器卸载：

```bash
brew uninstall mirror-rs
```

或：

```bash
pip uninstall mirror-rs
```

## 配置

默认读取 `~/.config/mirror-rs/config.json`，可用 `--config` 指定其他路径。最小示例：

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

- `host_ip`：写入 Docker 配置的宿主机地址。OrbStack 使用 `host.docker.internal`；Linux 原生 Docker 通常使用 `127.0.0.1`。
- `bind`：OrbStack 需要访问宿主机代理，使用 `0.0.0.0`；仅供本机访问时可使用 `127.0.0.1`。
- `mirrors`：候选镜像站，按实测结果动态排序。第三方站点的可用性可能变化。
- `probe_interval_secs`：探测间隔，默认 300 秒。
- `probe_bytes`：每次测速的 layer 采样大小，默认 256 KiB。
- `docker_config_path`：可选，自定义 `setup` 修改的 Docker 配置路径。

提供三个 HTTP 接口：`GET /healthz`、`GET /__mirror__/status`、`GET /__mirror__/probe`。

## 工作方式

Docker 的 `registry-mirrors` 按顺序回退。mirror-rs 作为固定入口，在代理内部根据测速结果选择上游，通过流式转发下载 layer，并处理 Registry Bearer token 认证。

评分同时考虑响应耗时和采样吞吐，分数越低越优先。该排名是网络采样结果，不保证每个镜像、每次请求都达到最高吞吐。传输中断后由 Docker 客户端重试，不会在同一响应流中切换上游。

主要使用 Axum、Tokio 和 reqwest，复用 HTTP 连接池和流式响应 API。实现取舍、验证范围见 [docs/DESIGN.md](docs/DESIGN.md)。

代理用于 Docker Hub 镜像拉取；不提供 push、镜像内容缓存或访问控制。默认监听所有网卡，部署时请限制可访问范围。

## 开发与发布

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked --release
```

推送 `v*` 标签后，GitHub Actions 构建四个平台的二进制，上传 GitHub Release，并在配置 `TAP_TOKEN` 后自动更新 `pengyanai/homebrew-tap`。
