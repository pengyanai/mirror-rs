#!/usr/bin/env python3
"""Generate a Homebrew formula for mirror-rs from a GitHub release.

Reads the release (public API, no token needed) and writes a formula that
selects the right prebuilt binary per platform/arch.

Usage:
    gen_brew_formula.py [tag] [out_path]

    tag       release tag, e.g. v0.1.0 (default: latest release)
    out_path  where to write the formula (default: Formula/mirror-rs.rb)
"""
import json
import os
import sys
import urllib.request

REPO = "pengyanai/mirror-rs"


def fetch(url: str) -> bytes:
    req = urllib.request.Request(
        url, headers={"User-Agent": "mirror-rs-brew-updater"}
    )
    with urllib.request.urlopen(req) as r:
        return r.read()


def fetch_json(url: str):
    return json.loads(fetch(url))


def main() -> None:
    tag = sys.argv[1] if len(sys.argv) > 1 and sys.argv[1] else None
    out = sys.argv[2] if len(sys.argv) > 2 else "Formula/mirror-rs.rb"

    if not tag:
        tag = fetch_json(f"https://api.github.com/repos/{REPO}/releases/latest")["tag_name"]
    version = tag.lstrip("v")
    base = f"https://github.com/{REPO}/releases/download/{tag}"

    rel = fetch_json(f"https://api.github.com/repos/{REPO}/releases/tags/{tag}")
    assets = {a["name"]: a["browser_download_url"] for a in rel["assets"]}

    def sha256(pkg: str) -> str:
        data = fetch(assets[f"{pkg}.tar.gz.sha256"]).decode()
        return data.split()[0]

    targets = {
        "aarch64-apple-darwin": sha256("mirror-rs-aarch64-apple-darwin"),
        "x86_64-apple-darwin": sha256("mirror-rs-x86_64-apple-darwin"),
        "aarch64-unknown-linux-musl": sha256("mirror-rs-aarch64-unknown-linux-musl"),
        "x86_64-unknown-linux-musl": sha256("mirror-rs-x86_64-unknown-linux-musl"),
    }

    formula = f'''class MirrorRs < Formula
  desc "Seamless, transparent Docker registry mirror proxy with speed-based load balancing"
  homepage "https://github.com/{REPO}"
  version "{version}"

  if OS.mac?
    if Hardware::CPU.arm?
      url "{base}/mirror-rs-aarch64-apple-darwin.tar.gz"
      sha256 "{targets['aarch64-apple-darwin']}"
    else
      url "{base}/mirror-rs-x86_64-apple-darwin.tar.gz"
      sha256 "{targets['x86_64-apple-darwin']}"
    end
  else
    if Hardware::CPU.arm?
      url "{base}/mirror-rs-aarch64-unknown-linux-musl.tar.gz"
      sha256 "{targets['aarch64-unknown-linux-musl']}"
    else
      url "{base}/mirror-rs-x86_64-unknown-linux-musl.tar.gz"
      sha256 "{targets['x86_64-unknown-linux-musl']}"
    end
  end

  def install
    bin.install "mirror-rs"
  end

  test do
    system bin/"mirror-rs", "--version"
  end
end
'''
    out_dir = os.path.dirname(out)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
    with open(out, "w") as f:
        f.write(formula)
    print(f"Wrote {out} for {tag}")


if __name__ == "__main__":
    main()
