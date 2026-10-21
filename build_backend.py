"""Bundle the matching GitHub release binary into a platform-specific wheel."""
import hashlib
import io
import os
import platform
import shutil
import sys
import tarfile
import urllib.request

from setuptools.command.bdist_wheel import bdist_wheel
from setuptools.command.build_py import build_py

REPO = "pengyanai/mirror-rs"


def _target_name() -> str:
    machine = platform.machine().lower()
    arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "amd64": "x86_64"}.get(machine)
    suffix = {"darwin": "apple-darwin", "linux": "unknown-linux-musl"}.get(sys.platform)
    if arch and suffix:
        return f"{arch}-{suffix}"
    raise SystemExit(
        f"mirror-rs: unsupported platform {sys.platform}/{machine}. "
        "Supported: macOS (arm64/x86_64), Linux (x86_64/aarch64)."
    )


def _download_binary(dest_dir: str, version: str) -> None:
    target = _target_name()
    url = f"https://github.com/{REPO}/releases/download/v{version}/mirror-rs-{target}.tar.gz"
    print(f"mirror-rs: downloading {url}")
    try:
        with urllib.request.urlopen(url, timeout=120) as response:
            payload = response.read()
        with urllib.request.urlopen(url + ".sha256", timeout=30) as response:
            expected = response.read().decode().split()[0]
    except Exception as error:
        raise SystemExit(f"mirror-rs: failed to download release v{version}: {error}") from error
    if hashlib.sha256(payload).hexdigest() != expected:
        raise SystemExit("mirror-rs: release binary checksum mismatch")
    with tarfile.open(fileobj=io.BytesIO(payload)) as archive:
        member = archive.getmember("mirror-rs")
        if not member.isfile():
            raise SystemExit("mirror-rs: release binary must be a regular file")
        os.makedirs(dest_dir, exist_ok=True)
        bin_path = os.path.join(dest_dir, "mirror-rs")
        with archive.extractfile(member) as source, open(bin_path, "wb") as destination:
            shutil.copyfileobj(source, destination)
    os.chmod(bin_path, 0o755)


class BuildPy(build_py):
    def run(self) -> None:
        super().run()
        _download_binary(
            os.path.join(self.build_lib, "mirror_rs", "bin"),
            self.distribution.get_version(),
        )


class BDistWheel(bdist_wheel):
    def finalize_options(self) -> None:
        super().finalize_options()
        self.root_is_pure = False

    def get_tag(self):
        target = _target_name()
        platform_tag = {
            "aarch64-apple-darwin": "macosx_11_0_arm64",
            "x86_64-apple-darwin": "macosx_11_0_x86_64",
            "aarch64-unknown-linux-musl": "linux_aarch64",
            "x86_64-unknown-linux-musl": "linux_x86_64",
        }[target]
        return "py3", "none", platform_tag
