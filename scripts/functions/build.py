"""Cargo build, binary discovery, and tarball packaging."""

from __future__ import annotations

import os
import shutil
import subprocess
import tarfile
import tempfile
from dataclasses import dataclass
from pathlib import Path

from .cli import ReleaseError, console

SUPPORTED_TRIPLES: frozenset[str] = frozenset(
    {
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-gnu",
        "aarch64-unknown-linux-musl",
        "armv7-unknown-linux-gnueabihf",
        "arm-unknown-linux-gnueabihf",
    }
)


@dataclass(frozen=True)
class BuildArtifact:
    """Result of a successful build-and-package operation."""

    asset_name: str
    asset_path: Path
    host_triple: str


def detect_host_triple() -> str:
    """Detect the host target triple from 'rustc -vV'.

    Parses the 'host:' line from rustc verbose version output.
    Validates the triple against SUPPORTED_TRIPLES.
    """
    result = subprocess.run(
        ["rustc", "-vV"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(f"failed to run 'rustc -vV': {result.stderr.strip()}")

    host_triple = ""
    for line in result.stdout.splitlines():
        if line.startswith("host: "):
            host_triple = line[len("host: ") :].strip()
            break

    if not host_triple:
        raise ReleaseError("could not determine Rust host target triple from 'rustc -vV'")

    if host_triple not in SUPPORTED_TRIPLES:
        supported = ", ".join(sorted(SUPPORTED_TRIPLES))
        raise ReleaseError(
            f"unsupported host target '{host_triple}' (supported: {supported})"
        )

    return host_triple


def cargo_build(tag: str, host_triple: str, repo_root: Path) -> None:
    """Run 'cargo clean' followed by a release build for the peppy binary.

    Sets PEPPY_GIT_TAG env var for the build.
    """
    cache_root = Path(tempfile.gettempdir()) / "peppy-build-cache"
    if cache_root.exists():
        console.print("Clearing build cache...")
        shutil.rmtree(cache_root)

    console.print("Cleaning previous build artifacts...")
    result = subprocess.run(
        ["cargo", "clean"],
        cwd=repo_root,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(f"cargo clean failed: {result.stderr.strip()}")

    console.print(f"Building peppy for [bold]{host_triple}[/bold]...")
    env = {**os.environ, "PEPPY_GIT_TAG": tag}
    result = subprocess.run(
        [
            "cargo",
            "build",
            "-p",
            "peppy",
            "--bin",
            "peppy",
            "--release",
            "--locked",
            "--target",
            host_triple,
        ],
        cwd=repo_root,
        env=env,
    )
    if result.returncode != 0:
        raise ReleaseError(f"cargo build failed (exit {result.returncode})")


def _get_target_dir(repo_root: Path) -> Path:
    """Get the cargo target directory, respecting CARGO_TARGET_DIR."""
    return Path(os.environ.get("CARGO_TARGET_DIR", str(repo_root / "target")))


def find_peppy_binary(host_triple: str, repo_root: Path) -> Path:
    """Locate the compiled peppy binary in the target directory.

    Checks {target_dir}/{host_triple}/release/peppy first, then
    falls back to {target_dir}/release/peppy.
    """
    target_dir = _get_target_dir(repo_root)

    primary = target_dir / host_triple / "release" / "peppy"
    if primary.is_file():
        return primary

    fallback = target_dir / "release" / "peppy"
    if fallback.is_file():
        return fallback

    raise ReleaseError(f"peppy binary not found (expected '{primary}')")


def find_zenohd_binary(host_triple: str, repo_root: Path) -> Path:
    """Locate the built zenohd binary in the build output.

    Searches {target_dir}/{host_triple}/release/build/pmi-*/out/zenohd.
    """
    target_dir = _get_target_dir(repo_root)
    build_dir = target_dir / host_triple / "release" / "build"

    matches = sorted(build_dir.glob("pmi-*/out/zenohd"))
    if matches:
        return matches[0]

    raise ReleaseError(
        f"zenohd binary not found in target dir "
        f"(expected it under '{build_dir}/pmi-*/out/zenohd')"
    )


def find_build_dir(
    host_triple: str,
    repo_root: Path,
    pattern: str,
    label: str,
) -> Path:
    """Locate a required directory in the build output.

    Searches {target_dir}/{host_triple}/release/build/{pattern}.
    Raises ReleaseError if not found.
    """
    target_dir = _get_target_dir(repo_root)
    build_dir = target_dir / host_triple / "release" / "build"

    matches = sorted(build_dir.glob(pattern))
    if matches and matches[0].is_dir():
        return matches[0]

    raise ReleaseError(
        f"{label} directory not found in build output "
        f"(expected it under '{build_dir}/{pattern}')"
    )


def package_release(
    host_triple: str,
    repo_root: Path,
    peppy_bin: Path,
    zenohd_bin: Path,
    apptainer_dir: Path,
    lima_dir: Path,
) -> BuildArtifact:
    """Create a .tgz release archive.

    Creates a temporary directory with the package layout:
      ./bin/peppy
      ./bin/zenohd
      ./apptainer/
      ./lima/

    Writes the archive to {dist_dir}/peppy-{host_triple}.tgz.
    """
    dist_dir = Path(os.environ.get("PEPPY_DIST_DIR", str(repo_root / "dist")))
    dist_dir.mkdir(parents=True, exist_ok=True)

    asset_name = f"peppy-{host_triple}.tgz"
    asset_path = dist_dir / asset_name

    with tempfile.TemporaryDirectory(prefix="peppy_release_pkg_") as pkg_dir_str:
        pkg_dir = Path(pkg_dir_str)
        bin_dir = pkg_dir / "bin"
        bin_dir.mkdir()

        shutil.copy2(peppy_bin, bin_dir / "peppy")
        (bin_dir / "peppy").chmod(0o755)

        shutil.copy2(zenohd_bin, bin_dir / "zenohd")
        (bin_dir / "zenohd").chmod(0o755)

        shutil.copytree(apptainer_dir, pkg_dir / "apptainer")
        shutil.copytree(lima_dir, pkg_dir / "lima")

        with tarfile.open(asset_path, "w:gz") as tar:
            for child in sorted(pkg_dir.iterdir()):
                tar.add(child, arcname=f"./{child.name}")

    console.print(f"Built artifact: [bold]{asset_path}[/bold]")
    return BuildArtifact(
        asset_name=asset_name,
        asset_path=asset_path,
        host_triple=host_triple,
    )


def build_and_package(tag: str, repo_root: Path) -> BuildArtifact:
    """Full build-and-package pipeline: detect triple, build, find binaries, create tarball.

    Returns a BuildArtifact on success.
    """
    host_triple = detect_host_triple()

    cargo_build(tag, host_triple, repo_root)

    peppy_bin = find_peppy_binary(host_triple, repo_root)
    zenohd_bin = find_zenohd_binary(host_triple, repo_root)

    apptainer_dir = find_build_dir(
        host_triple,
        repo_root,
        "containers-*/out/apptainer-install",
        "apptainer install",
    )
    lima_dir = find_build_dir(
        host_triple,
        repo_root,
        "containers-*/out/lima-install",
        "lima install",
    )

    return package_release(
        host_triple, repo_root, peppy_bin, zenohd_bin, apptainer_dir, lima_dir
    )
