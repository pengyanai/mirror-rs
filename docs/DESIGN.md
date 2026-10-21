# Design and validation

## Proxy architecture

Docker points `registry-mirrors` at a fixed local endpoint. Axum handles concurrent requests on Tokio, and a shared reqwest client forwards requests to the ranked upstream mirrors. Response bodies use `bytes_stream()` and `Body::from_stream()`, preserving backpressure without buffering an entire image layer.

The proxy reuses reqwest's HTTP connection pool. Routing locks protect small metadata snapshots and are released before network I/O. Replacing these APIs with custom sockets, a custom connection pool, or lock-free structures has no demonstrated benefit for this workload.

Probes resolve an OCI image index to a Linux platform manifest and sample a real layer. The score combines manifest response time with sampled download throughput. Probe results are a network sample, not a throughput guarantee.

## Performance review, 2026-10-21

The review found four concrete sources of unnecessary delay or failure in the original implementation:

- A client-wide 30-second total timeout also applied to streaming large layers. Active downloads could terminate after 30 seconds.
- Startup launched two complete probe cycles concurrently, using separate clients and connection pools.
- Registry Bearer tokens were discarded after every request. Concurrent requests repeated authentication work.
- Request failures did not influence routing until the next periodic probe. A dead primary mirror was retried for every request.

Additional correctness findings affected the same paths: authenticated retries bypassed failover status checks, client credentials could be forwarded to a different registry, request bodies were copied unnecessarily, and SIGTERM handling was registered only after SIGINT arrived.

The implementation uses reqwest for HTTP transport, Tokio/Axum for asynchronous streaming, `http-auth` for authentication challenge parsing, and `moka` for expiry and concurrent cache initialization. Tokens are scoped by mirror and repository, follow `expires_in` (60 seconds when omitted), and are proactively reused by proxy and probe requests. Rejected cached tokens are invalidated. Docker client Authorization is not forwarded to unrelated mirrors; a mirror's explicitly configured static token remains supported.

Forwarding uses a 5-second connection timeout and 30-second read inactivity timeout, without a total transfer deadline. Connection errors and 5xx responses demote a mirror for 5 seconds; 404 remains a resource-specific miss. Cooled mirrors remain available as a last resort. Startup binds the listener before one initial probe; subsequent probes use Tokio intervals with missed ticks skipped.

Regression tests use local Axum upstreams rather than public mirror availability as the oracle.

No Internet throughput speedup is claimed from source review or local regression tests. An end-to-end throughput comparison requires the same layer, upstream, cache state and network conditions. The tests establish behavior and remove duplicated work; they do not establish a universal MB/s result.

## Package distribution

GitHub Actions builds macOS arm64/x86_64 and Linux arm64/x86_64 binaries. Linux builds use musl; macOS builds declare a deployment target matching the wheel compatibility tag. Each archive contains one executable and has a SHA256 sidecar.

Python packaging downloads the binary matching the package version, verifies SHA256 and copies only a regular archive member into the wheel build directory. It does not unpack arbitrary archive paths or mutate the source package. The wheel uses `py3-none` with a platform tag, because the executable has no Python ABI dependency. Static Linux executables use `linux_x86_64` or `linux_aarch64` so installation works on both glibc and musl hosts.

The Python console entry point uses `os.execv` to hand control to the executable. Missing binaries fail explicitly rather than falling back to a command that could recursively invoke the entry point.

The source distribution includes its setuptools command module. Python installation is available from the Git repository and GitHub Release packages. PyPI publication is separate and is not configured.

Homebrew formula generation reads the exact release tag and SHA256 assets through `gh` CLI. A repository-scoped token with write access to `pengyanai/homebrew-tap` is required for automatic cross-repository updates. Missing credentials skip the tap update explicitly without preventing the binary release.

## Validation

- `actionlint` validates both GitHub Actions workflows, including expression context availability.
- Rust checks cover formatting, Clippy, release tests and CLI smoke tests. Local Axum regressions verify concurrent token fetch coalescing, token expiry and revocation recovery, authenticated failover, credential isolation, short-term mirror cooling, probe cadence, streaming integrity and async setup health checks.
- A continuous 32-second stream verifies that an active transfer survives the former 30-second total timeout. An isolated process smoke test checks `/healthz`, status JSON and clean SIGTERM shutdown.
- Eight Python regression tests cover supported platform selection, architecture rejection, wheel compatibility tags, executable permissions, checksum validation, regular-file extraction and missing-binary errors.
- Release jobs build and install each platform wheel, then run `mirror-rs --version` and `mirror-rs init`.
- CI builds the source distribution to catch missing build support files.

The existing proxy service and Docker configuration do not need to be changed to test package installation. Automatic `setup` and `teardown` affect local service configuration and are tested separately from package installation.
