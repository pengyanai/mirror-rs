"""Python wrapper for the mirror-rs binary.

The actual proxy is a Rust binary bundled under ``mirror_rs/bin/`` at build
time. This module just execs it, forwarding all CLI args and the exit code.
"""
import os
import sys

__version__ = "0.1.1"


def _binary_path() -> str:
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.join(here, "bin", "mirror-rs")


def main() -> None:
    bin_path = _binary_path()
    if not os.path.isfile(bin_path):
        raise SystemExit("mirror-rs: bundled binary is missing; reinstall the package")
    os.execv(bin_path, [bin_path] + sys.argv[1:])
