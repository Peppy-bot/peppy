"""Cargo build, binary discovery, and tarball packaging."""

from __future__ import annotations

import os
import subprocess
import tarfile
import tempfile
import shutil
from dataclasses import dataclass
from pathlib import Path

from .cli import RELEASE_TRIPLES, ReleaseError, console


@dataclass(frozen=True)
class BuildArtifact:
    """Result of a successful build-and-package operation."""

    asset_name: str
    asset_path: Path
    host_triple: str


def cargo_build(tag: str, target_triple: str, repo_root: Path) -> None:
    """Run a release build for the peppy binary.

    Sets PEPPY_GIT_TAG env var for the build.
    Each target triple gets its own directory under target/{triple}/release/,
    so no cargo clean is needed.
    """
    console.print(f"Building peppy for [bold]{target_triple}[/bold]...")
    env = {**os.environ, "PEPPY_GIT_TAG": tag}
    result = subprocess.run(
        [
            "cargo",
            "build",
            "-p",
            "peppy",
            "--release",
            "--locked",
            "--target",
            target_triple,
        ],
        cwd=repo_root,
        env=env,
    )
    if result.returncode != 0:
        raise ReleaseError(f"cargo build failed (exit {result.returncode})")


def _get_target_dir(repo_root: Path) -> Path:
    """Get the cargo target directory, respecting CARGO_TARGET_DIR."""
    return Path(os.environ.get("CARGO_TARGET_DIR", str(repo_root / "target")))


def find_peppy_binary(target_triple: str, repo_root: Path) -> Path:
    """Locate the compiled peppy binary in the target directory.

    Checks {target_dir}/{target_triple}/release/peppy first, then
    falls back to {target_dir}/release/peppy.
    """
    target_dir = _get_target_dir(repo_root)

    primary = target_dir / target_triple / "release" / "peppy"
    if primary.is_file():
        return primary

    fallback = target_dir / "release" / "peppy"
    if fallback.is_file():
        return fallback

    raise ReleaseError(f"peppy binary not found (expected '{primary}')")


def find_zenohd_binary(target_triple: str, repo_root: Path) -> Path | None:
    """Locate the built zenohd binary in the build output.

    Searches {target_dir}/{target_triple}/release/build/pmi-*/out/zenohd.
    Returns None if no pre-built zenohd was embedded (e.g. riscv64).
    """
    target_dir = _get_target_dir(repo_root)
    build_dir = target_dir / target_triple / "release" / "build"

    matches = sorted(build_dir.glob("pmi-*/out/zenohd"))
    if matches:
        return matches[0]

    return None


def find_build_dir(
    target_triple: str,
    repo_root: Path,
    pattern: str,
    label: str,
) -> Path:
    """Locate a required directory in the build output.

    Searches {target_dir}/{target_triple}/release/build/{pattern}.
    Raises ReleaseError if not found.
    """
    target_dir = _get_target_dir(repo_root)
    build_dir = target_dir / target_triple / "release" / "build"

    matches = sorted(build_dir.glob(pattern))
    if matches and matches[0].is_dir():
        return matches[0]

    raise ReleaseError(
        f"{label} directory not found in build output "
        f"(expected it under '{build_dir}/{pattern}')"
    )


def package_release(
    target_triple: str,
    repo_root: Path,
    peppy_bin: Path,
    zenohd_bin: Path | None,
    apptainer_dir: Path | None,
    lima_dir: Path | None = None,
) -> BuildArtifact:
    """Create a .tgz release archive.

    Creates a temporary directory with the package layout:
      ./bin/peppy
      ./bin/zenohd        (if available for target)
      ./bin/apptainer/    (if available for target)
      ./bin/lima/         (macOS only)

    Writes the archive to {dist_dir}/peppy-{target_triple}.tgz.
    """
    dist_dir = Path(os.environ.get("PEPPY_DIST_DIR", str(repo_root / "dist")))
    dist_dir.mkdir(parents=True, exist_ok=True)

    asset_name = f"peppy-{target_triple}.tgz"
    asset_path = dist_dir / asset_name

    with tempfile.TemporaryDirectory(prefix="peppy_release_pkg_") as pkg_dir_str:
        pkg_dir = Path(pkg_dir_str)
        bin_dir = pkg_dir / "bin"
        bin_dir.mkdir()

        shutil.copy2(peppy_bin, bin_dir / "peppy")
        (bin_dir / "peppy").chmod(0o755)

        if zenohd_bin is not None:
            shutil.copy2(zenohd_bin, bin_dir / "zenohd")
            (bin_dir / "zenohd").chmod(0o755)

        if apptainer_dir is not None:
            shutil.copytree(apptainer_dir, bin_dir / "apptainer")
        if lima_dir is not None:
            shutil.copytree(lima_dir, bin_dir / "lima")

        with tarfile.open(asset_path, "w:gz") as tar:
            for child in sorted(pkg_dir.iterdir()):
                tar.add(child, arcname=f"./{child.name}")

    console.print(f"Built artifact: [bold]{asset_path}[/bold]")
    return BuildArtifact(
        asset_name=asset_name,
        asset_path=asset_path,
        host_triple=target_triple,
    )


def build_and_package(
    tag: str,
    target_triple: str,
    repo_root: Path,
    *,
    limactl: Path | None = None,
) -> BuildArtifact:
    """Build and package for a specific target triple.

    If limactl is provided, builds inside the Lima VM (for Linux targets from macOS).
    Otherwise builds natively.
    """
    if target_triple not in RELEASE_TRIPLES:
        supported = ", ".join(sorted(RELEASE_TRIPLES))
        raise ReleaseError(
            f"unsupported target '{target_triple}' (supported: {supported})"
        )

    if limactl is not None:
        from .lima import cargo_build_in_lima

        cargo_build_in_lima(limactl, tag, target_triple, repo_root)
    else:
        cargo_build(tag, target_triple, repo_root)

    peppy_bin = find_peppy_binary(target_triple, repo_root)
    zenohd_bin = find_zenohd_binary(target_triple, repo_root)

    try:
        apptainer_dir: Path | None = find_build_dir(
            target_triple,
            repo_root,
            "containers-*/out/apptainer-install",
            "apptainer install",
        )
    except ReleaseError:
        apptainer_dir = None

    lima_dir = (
        find_build_dir(
            target_triple,
            repo_root,
            "containers-*/out/lima-install",
            "lima install",
        )
        if "apple-darwin" in target_triple
        else None
    )

    return package_release(
        target_triple, repo_root, peppy_bin, zenohd_bin, apptainer_dir, lima_dir
    )
