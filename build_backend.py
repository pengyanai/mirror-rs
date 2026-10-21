"""Custom build backend for mirror-rs.

Downloads the prebuilt binary for the current platform from the GitHub
release and bundles it into the wheel, so `pip install` yields a working
`mirror-rs` console script with no Rust toolchain required.
"""
import os
import platform
import sys
import tarfile
import urllib.request

from setuptools.command.build_py import build_py

REPO = "pengyanai/mirror-rs"
VERSION = "0.1.0"  # keep in sync with pyproject.toml

REPO_ROOT = os.path.dirname(os.path.abspath(__file__))


def _target_name() -> str:
    sysname = sys.platform
    machine = platform.machine().lower()
    if sysname == "darwin":
        arch = "aarch64" if machine in ("arm64", "aarch64") else "x86_64"
        return f"{arch}-apple-darwin"
    if sysname == "linux":
        arch = "aarch64" if machine in ("aarch64", "arm64") else "x86_64"
        return f"{arch}-unknown-linux-musl"
    raise SystemExit(
        f"mirror-rs: unsupported platform {sysname}/{machine}. "
        "Supported: macOS (arm64/x86_64), Linux (x86_64/aarch64)."
    )


def _download_binary(dest_dir: str) -> None:
    target = _target_name()
    pkg = f"mirror-rs-{target}"
    url = f"https://github.com/{REPO}/releases/download/v{VERSION}/{pkg}.tar.gz"
    os.makedirs(dest_dir, exist_ok=True)
    tar_path = os.path.join(dest_dir, f"{pkg}.tar.gz")
    print(f"mirror-rs: downloading {url}")
    try:
        urllib.request.urlretrieve(url, tar_path)
    except Exception as e:  # noqa: BLE001
        raise SystemExit(
            f"mirror-rs: failed to download binary from {url}: {e}\n"
            f"Make sure the GitHub release v{VERSION} exists."
        )
    with tarfile.open(tar_path) as tf:
        tf.extract("mirror-rs", dest_dir)
    os.remove(tar_path)
    bin_path = os.path.join(dest_dir, "mirror-rs")
    os.chmod(bin_path, 0o755)
    print(f"mirror-rs: bundled binary at {bin_path}")


class BuildPy(build_py):
    def run(self) -> None:
        # Download into the source package dir so it gets packaged into the wheel.
        src_dir = os.path.join(REPO_ROOT, "mirror_rs", "bin")
        _download_binary(src_dir)
        super().run()
