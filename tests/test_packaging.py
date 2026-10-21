import hashlib
from email.message import Message
import importlib.util
import io
import pathlib
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

from setuptools import Distribution

ROOT = pathlib.Path(__file__).resolve().parents[1]


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


backend = load_module("build_backend", ROOT / "build_backend.py")
wrapper = load_module("mirror_rs", ROOT / "mirror_rs" / "__init__.py")


class Response(io.BytesIO):
    def info(self):
        return Message()


class PackagingTests(unittest.TestCase):
    def test_supported_platforms_select_matching_release_asset(self):
        for system, machine, expected in (
            ("darwin", "arm64", "aarch64-apple-darwin"),
            ("darwin", "x86_64", "x86_64-apple-darwin"),
            ("linux", "aarch64", "aarch64-unknown-linux-musl"),
            ("linux", "x86_64", "x86_64-unknown-linux-musl"),
        ):
            with self.subTest(system=system, machine=machine), patch.object(
                sys, "platform", system
            ), patch.object(backend.platform, "machine", return_value=machine):
                self.assertEqual(backend._target_name(), expected)

    def test_unsupported_architecture_is_rejected(self):
        with patch.object(sys, "platform", "linux"), patch.object(
            backend.platform, "machine", return_value="ppc64le"
        ):
            with self.assertRaisesRegex(SystemExit, "unsupported platform"):
                backend._target_name()

    def test_wheel_has_platform_tag_without_python_abi(self):
        with patch.object(sys, "platform", "darwin"), patch.object(
            backend.platform, "machine", return_value="arm64"
        ):
            command = backend.BDistWheel(Distribution())
            command.ensure_finalized()
            self.assertEqual(command.get_tag(), ("py3", "none", "macosx_11_0_arm64"))
            self.assertFalse(command.root_is_pure)

    def test_linux_wheel_supports_glibc_and_musl_hosts(self):
        with patch.object(sys, "platform", "linux"), patch.object(
            backend.platform, "machine", return_value="x86_64"
        ):
            command = backend.BDistWheel(Distribution())
            command.ensure_finalized()
            self.assertEqual(command.get_tag(), ("py3", "none", "linux_x86_64"))

    def test_verified_binary_is_executable(self):
        data = io.BytesIO()
        payload = b"test executable"
        with tarfile.open(fileobj=data, mode="w:gz") as archive:
            member = tarfile.TarInfo("mirror-rs")
            member.size = len(payload)
            archive.addfile(member, io.BytesIO(payload))
        archive_data = data.getvalue()

        def response(url, **kwargs):
            if url.endswith(".sha256"):
                return Response(hashlib.sha256(archive_data).hexdigest().encode())
            return Response(archive_data)

        with tempfile.TemporaryDirectory() as directory, patch.object(
            backend.urllib.request, "urlopen", side_effect=response
        ):
            backend._download_binary(directory, "0.1.1")
            binary = pathlib.Path(directory) / "mirror-rs"
            self.assertEqual(binary.read_bytes(), payload)
            self.assertEqual(binary.stat().st_mode & 0o777, 0o755)

    def test_missing_binary_fails_without_searching_path(self):
        with patch.object(wrapper.os.path, "isfile", return_value=False), patch.object(
            wrapper.os.path, "exists", return_value=False
        ), patch.object(
            wrapper.os, "execv"
        ) as execute:
            with self.assertRaisesRegex(SystemExit, "bundled binary is missing"):
                wrapper.main()
            execute.assert_not_called()

    def test_binary_checksum_is_verified(self):
        archive = b"not a verified archive"

        def response(url, *args, **kwargs):
            if url.endswith(".sha256"):
                return Response(b"0" * 64 + b"  mirror-rs.tar.gz\n")
            return Response(archive)

        with tempfile.TemporaryDirectory() as directory, patch.object(
            backend.urllib.request, "urlopen", side_effect=response
        ):
            with self.assertRaisesRegex(SystemExit, "checksum mismatch"):
                backend._download_binary(directory, "0.1.1")
            self.assertFalse((pathlib.Path(directory) / "mirror-rs").exists())

    def test_archive_symlink_is_rejected(self):
        data = io.BytesIO()
        with tarfile.open(fileobj=data, mode="w:gz") as archive:
            member = tarfile.TarInfo("mirror-rs")
            member.type = tarfile.SYMTYPE
            member.linkname = "/bin/sh"
            archive.addfile(member)
        payload = data.getvalue()

        def response(url, *args, **kwargs):
            if url.endswith(".sha256"):
                return Response(hashlib.sha256(payload).hexdigest().encode())
            return Response(payload)

        with tempfile.TemporaryDirectory() as directory, patch.object(
            backend.urllib.request, "urlopen", side_effect=response
        ):
            with self.assertRaisesRegex(SystemExit, "regular file"):
                backend._download_binary(directory, "0.1.1")


if __name__ == "__main__":
    unittest.main()
