"""Python wrapper for the mirror-rs binary.

The actual proxy is a Rust binary bundled under ``mirror_rs/bin/`` at build
time. This module just execs it, forwarding all CLI args and the exit code.
"""
import os
import subprocess
import sys

__version__ = "0.1.0"


def _binary_path() -> str:
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.join(here, "bin", "mirror-rs")


def main() -> None:
    bin_path = _binary_path()
    if not os.path.exists(bin_path):
        # Fall back to PATH (e.g. if the binary is also installed via brew).
        bin_path = "mirror-rs"
    sys.exit(subprocess.call([bin_path] + sys.argv[1:]))
